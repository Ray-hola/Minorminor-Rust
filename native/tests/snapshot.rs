use minorminor_core::{index, server, snapshot, user_state};
use serde_json::json;
use std::fs;

#[test]
fn a_failed_fresh_scan_never_replaces_the_published_snapshot_or_user_history() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let data = temp.path().join("data");
    fs::create_dir(&home).unwrap();
    fs::write(home.join("fail.dat"), b"synthetic").unwrap();
    let path = index::initialize(&data).unwrap();
    let db = index::connect(&path).unwrap();
    db.execute(
        "INSERT INTO scans VALUES('keep','/fixture','today','complete','{}')",
        [],
    )
    .unwrap();
    index::save_setting(&db, "locale", &json!("en")).unwrap();
    db.execute(
        "INSERT INTO receipts VALUES('old','today','/synthetic',NULL,'skipped',0,'keep')",
        [],
    )
    .unwrap();
    drop(db);
    let work = snapshot::Work::prepare(&data, "full-fixture", false).unwrap();
    {
        let db = index::connect(&work.path).unwrap();
        db.execute_batch("CREATE TRIGGER fixture_full BEFORE INSERT ON _entries WHEN NEW.name='fail.dat' BEGIN SELECT RAISE(ABORT,'database or disk is full'); END").unwrap();
    }
    let outcome = server::worker(
        json!({"action":"scan","jobId":"full-fixture","home":home,"root":home,"data":data,"resources":temp.path(),"disk":false,"ollamaBase":"http://127.0.0.1:1"}),
    );
    assert!(outcome.is_err());
    assert!(!work.path.exists());
    let db = index::connect(&path).unwrap();
    assert_eq!(index::latest(&db).unwrap().as_deref(), Some("keep"));
    assert_eq!(index::setting(&db, "locale").unwrap(), Some(json!("en")));
    assert_eq!(
        db.query_row("SELECT count(*) FROM receipts", [], |r| r.get::<_, u64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn successful_publication_preserves_state_and_reclaims_only_its_own_work_files() {
    let temp = tempfile::tempdir().unwrap();
    let path = index::initialize(temp.path()).unwrap();
    {
        let db = index::connect(&path).unwrap();
        db.execute(
            "INSERT INTO scans VALUES('old','/fixture','today','complete','{}')",
            [],
        )
        .unwrap();
        index::save_setting(&db, "locale", &json!("zh")).unwrap();
    }
    let work = snapshot::Work::prepare(temp.path(), "new-fixture", false).unwrap();
    work.claim().unwrap();
    {
        let db = index::connect(&work.path).unwrap();
        db.execute(
            "INSERT INTO scans VALUES('new','/fixture','tomorrow','complete','{}')",
            [],
        )
        .unwrap();
    }
    let extra = temp.path().join("user-document");
    fs::write(&extra, b"keep").unwrap();
    let result = work.finish(false, |_, _, _| Ok(())).unwrap();
    assert!(result["indexReady"] == false);
    assert!(extra.exists());
    let db = index::connect_readonly(&path).unwrap();
    assert_eq!(index::latest(&db).unwrap().as_deref(), Some("new"));
    assert_eq!(index::setting(&db, "locale").unwrap(), Some(json!("zh")));
    assert!(temp.path().join(user_state::FILE).exists());
    assert!(!temp.path().join(".scan-new-fixture.sqlite3").exists());
    assert!(!temp
        .path()
        .join(".scan-new-fixture-compressed.sqlite3")
        .exists());
}

#[test]
fn interrupted_work_and_invalid_job_paths_cannot_delete_the_current_index() {
    let temp = tempfile::tempdir().unwrap();
    let path = index::initialize(temp.path()).unwrap();
    assert!(snapshot::Work::prepare(temp.path(), "../index", false).is_err());
    {
        let work = snapshot::Work::prepare(temp.path(), "cancel-fixture", false).unwrap();
        work.claim().unwrap();
        assert!(work.path.exists());
    }
    assert!(path.exists());
    assert!(!temp.path().join(".scan-cancel-fixture.sqlite3").exists());
    snapshot::discard(temp.path(), "../index");
    assert!(path.exists());
}

#[test]
fn private_copy_preserves_committed_wal_pages_without_modifying_the_published_snapshot() {
    let temp = tempfile::tempdir().unwrap();
    let path = index::initialize(temp.path()).unwrap();
    let source = index::connect(&path).unwrap();
    source
        .execute_batch("PRAGMA main.wal_autocheckpoint=0")
        .unwrap();
    source
        .execute(
            "INSERT INTO scans VALUES('wal-snapshot','/fixture','today','complete','{}')",
            [],
        )
        .unwrap();
    assert!(
        fs::metadata(format!("{}-wal", path.display()))
            .unwrap()
            .len()
            > 0
    );
    let work = snapshot::Work::prepare(temp.path(), "wal-fixture", true).unwrap();
    let copy = index::connect(&work.path).unwrap();
    assert_eq!(
        index::latest(&copy).unwrap().as_deref(),
        Some("wal-snapshot")
    );
    assert_eq!(
        index::latest(&source).unwrap().as_deref(),
        Some("wal-snapshot")
    );
    assert_eq!(
        source
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name='_snapshot_job'",
                [],
                |r| r.get::<_, u64>(0)
            )
            .unwrap(),
        0
    );
    #[cfg(target_os = "macos")]
    assert_eq!(work.method, "apfs-clone");
}

#[test]
fn orphan_recovery_requires_our_manifest_a_dead_owner_and_regular_owned_files() {
    let temp = tempfile::tempdir().unwrap();
    index::initialize(temp.path()).unwrap();
    let work = snapshot::Work::prepare(temp.path(), "alive-fixture", false).unwrap();
    assert_eq!(snapshot::cleanup_orphans(temp.path()).unwrap(), 0);
    assert!(work.path.exists());
    let name = temp.path().join(".scan-dead-fixture.owner.json");
    let raw = temp.path().join(".scan-dead-fixture.sqlite3");
    fs::write(&raw, b"temporary interrupted index").unwrap();
    fs::write(
        &name,
        serde_json::to_vec(
            &json!({"format":"minorminor-scan-work-v1","job":"dead-fixture","pid":2147483646_u64}),
        )
        .unwrap(),
    )
    .unwrap();
    let unmarked = temp.path().join(".scan-unmarked.sqlite3");
    fs::write(&unmarked, b"preserve unknown file").unwrap();
    assert_eq!(snapshot::cleanup_orphans(temp.path()).unwrap(), 1);
    assert!(!raw.exists());
    assert!(!name.exists());
    assert!(unmarked.exists());
    assert!(work.path.exists());
}

#[test]
fn repeated_recovery_keeps_compressed_index_without_expansion() {
    let temp = tempfile::tempdir().unwrap();
    let path = index::initialize(temp.path()).unwrap();
    let work = snapshot::Work::prepare(temp.path(), "compressed-fixture", false).unwrap();
    work.claim().unwrap();
    {
        let db = index::connect(&work.path).unwrap();
        db.execute(
            "INSERT INTO scans VALUES('keep','/fixture','today','complete','{}')",
            [],
        )
        .unwrap();
    }
    let ready = work.finish(false, |_, _, _| Ok(())).unwrap();
    #[cfg(target_os = "macos")]
    {
        assert_eq!(ready["compressed"], true);
        use std::os::unix::fs::MetadataExt;
        let blocks = path.metadata().unwrap().blocks();
        server::worker(json!({"action":"index-recover","jobId":"repeat","home":temp.path(),"data":temp.path(),"resources":temp.path(),"reset":false})).unwrap();
        assert!(minorminor_core::fs_compression::is_compressed(&path));
        assert_eq!(path.metadata().unwrap().blocks(), blocks);
        assert!(!temp.path().join(".scan-repeat.sqlite3").exists());
    }
}

#[test]
fn measurement_orphan_cleanup_preserves_live_unknown_and_unmarked_directories() {
    let t = tempfile::tempdir().unwrap();
    for (name, pid, unknown) in [
        ("dead", 2147483646u64, false),
        ("live", std::process::id() as u64, false),
        ("unknown", 2147483646u64, true),
    ] {
        let path = t.path().join(format!(".refresh-{name}"));
        fs::create_dir(&path).unwrap();
        fs::write(path.join(".owner.json"),serde_json::to_vec(&json!({"format":"minorminor-scan-work-v1","kind":"measurement","job":name,"pid":pid})).unwrap()).unwrap();
        fs::write(path.join("index.sqlite3"), b"owned temporary index").unwrap();
        if unknown {
            fs::write(path.join("user-document.txt"), b"preserve").unwrap();
        }
    }
    let unmarked = t.path().join(".refresh-unmarked");
    fs::create_dir(&unmarked).unwrap();
    fs::write(unmarked.join("index.sqlite3"), b"preserve unknown owner").unwrap();
    assert_eq!(
        minorminor_core::measure::cleanup_orphans(t.path()).unwrap(),
        1
    );
    assert!(!t.path().join(".refresh-dead").exists());
    assert!(t.path().join(".refresh-live/index.sqlite3").exists());
    assert!(t.path().join(".refresh-unknown/user-document.txt").exists());
    assert!(unmarked.join("index.sqlite3").exists());
}
