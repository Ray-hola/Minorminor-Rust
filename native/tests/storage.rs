use minorminor_core::{access, index, server::Engine, storage};
use rusqlite::params;
use serde_json::json;
use std::{fs, path::Path};

#[test]
fn capacity_guard_has_a_real_boundary() {
    assert!(storage::check_capacity(256, 256).is_ok());
    assert!(storage::check_capacity(255, 256).is_err());
    assert!(storage::check_capacity(0, 256).is_err());
}

#[test]
fn project_session_counts_use_the_covering_index_and_keep_kind_and_scan_scope() {
    let temp = tempfile::tempdir().unwrap();
    let db = index::connect(&index::initialize(temp.path()).unwrap()).unwrap();
    db.execute_batch("BEGIN").unwrap();
    for scan in ["one", "two"] {
        for i in 0..3000 {
            db.execute(
                "INSERT INTO associations VALUES(?,?,?,'indexed',?,'tool',?,'','readable','',0)",
                params![
                    scan,
                    i.to_string(),
                    format!("/fixture/{i}"),
                    format!("project-{}", i % 100),
                    if i % 3 == 0 { "memory" } else { "session" }
                ],
            )
            .unwrap();
        }
    }
    db.execute_batch("COMMIT").unwrap();
    let sql = "SELECT count(*) FROM associations WHERE scan=? AND project=? AND kind='session'";
    assert_eq!(
        db.query_row(sql, params!["one", "project-17"], |r| r.get::<_, u64>(0))
            .unwrap(),
        20
    );
    let plan = db
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .unwrap()
        .query_map(params!["one", "project-17"], |r| r.get::<_, String>(3))
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap();
    assert!(
        plan.iter().any(|line| line
            .contains("COVERING INDEX associations_project (scan=? AND project=? AND kind=?)")),
        "{plan:?}"
    );
}

#[test]
fn planner_optimization_uses_bounded_sampling_without_removing_entries() {
    let temp = tempfile::tempdir().unwrap();
    let db = index::connect(&index::initialize(temp.path()).unwrap()).unwrap();
    db.execute_batch("BEGIN").unwrap();
    for i in 0..7005 {
        entry(&db, "keep", i);
    }
    db.execute_batch("COMMIT").unwrap();
    let limits = |sql: &str| {
        db.prepare(&format!("EXPLAIN {sql}"))
            .unwrap()
            .query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, i64>(3)?)))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap()
            .into_iter()
            .filter_map(|(opcode, limit)| (opcode == "SqlExec").then_some(limit))
            .collect::<Vec<_>>()
    };
    let unbounded = limits("PRAGMA optimize=0x10002");
    assert!(!unbounded.is_empty() && unbounded.iter().all(|limit| *limit == 0));
    let bounded = limits(index::OPTIMIZE_SQL);
    assert!(
        !bounded.is_empty() && bounded.iter().all(|limit| *limit > 0 && *limit <= 2000),
        "{bounded:?}"
    );
    db.execute_batch("PRAGMA analysis_limit=1000").unwrap();
    // A stricter connection limit is reused rather than temporarily overridden.
    assert!(limits(index::OPTIMIZE_SQL).iter().all(|limit| *limit == 0));
    assert_eq!(
        db.query_row("PRAGMA analysis_limit", [], |r| r.get::<_, u64>(0))
            .unwrap(),
        1000
    );
    index::optimize(&db).unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM entries", [], |r| r.get::<_, u64>(0))
            .unwrap(),
        7005
    );
    assert!(
        db.query_row("SELECT count(*) FROM sqlite_stat1", [], |r| r
            .get::<_, u64>(0))
            .unwrap()
            > 0
    );
}

#[test]
fn incremental_reclamation_drains_the_whole_batch_and_preserves_live_data() {
    let temp = tempfile::tempdir().unwrap();
    let path = index::initialize(temp.path()).unwrap();
    let db = index::connect(&path).unwrap();
    db.execute_batch("CREATE TABLE ballast(id INTEGER PRIMARY KEY, value BLOB); BEGIN")
        .unwrap();
    for i in 0..2500 {
        db.execute("INSERT INTO ballast VALUES(?,zeroblob(6000))", [i])
            .unwrap();
    }
    db.execute_batch("COMMIT; DROP TABLE ballast").unwrap();
    index::save_setting(&db, "keep", &json!({"value":"survives reclamation"})).unwrap();
    let free = || {
        db.query_row("PRAGMA freelist_count", [], |r| r.get::<_, u64>(0))
            .unwrap()
    };
    let before = free();
    assert!(before > 2048);
    // A partial statement really does reclaim only one page, regardless of N.
    db.execute_batch("PRAGMA incremental_vacuum(512)").unwrap();
    assert_eq!(free(), before - 1);
    storage::reclaim_pages(&db, 512).unwrap();
    assert_eq!(free(), before - 513);
    let mut amounts = vec![];
    storage::recover(&db, false, |stage, amount| {
        if stage == "compact" {
            amounts.push(amount);
        }
        Ok(())
    })
    .unwrap();
    assert_eq!(free(), 0);
    assert_eq!(amounts.last(), Some(&0));
    assert!(
        amounts.len() <= ((before - 513).div_ceil(2048) + 1) as usize,
        "{amounts:?}"
    );
    assert!(amounts.windows(2).all(|pair| pair[1] < pair[0]));
    assert_eq!(
        index::setting(&db, "keep").unwrap().unwrap()["value"],
        "survives reclamation"
    );
    assert_eq!(
        db.query_row("PRAGMA quick_check", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
}
fn entry(db: &rusqlite::Connection, scan: &str, i: usize) {
    db.execute("INSERT INTO entries(scan,path,parent,name,directory,bytes,logical,unique_bytes,identity,category) VALUES(?,?,?,'fixture',0,1,1,1,'{}','cache')",params![scan,format!("/fixture/{i}/{}","x".repeat(800)),"/fixture"]).unwrap();
}
#[test]
fn recovery_releases_pages_and_preserves_complete_results_settings_and_history() {
    let temp = tempfile::tempdir().unwrap();
    let path = index::initialize(temp.path()).unwrap();
    let db = index::connect(&path).unwrap();
    assert_eq!(
        db.query_row("PRAGMA auto_vacuum", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
    db.execute(
        "INSERT INTO scans VALUES('keep','/','2026-01-02','complete','{}')",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO scans VALUES('failed','/','2026-01-03','failed','{}')",
        [],
    )
    .unwrap();
    index::save_setting(&db, "ui-preferences", &json!({"locale":"en"})).unwrap();
    db.execute(
        "INSERT INTO receipts VALUES('job','today','/fixture',NULL,'skipped',0,'keep')",
        [],
    )
    .unwrap();
    entry(&db, "keep", 0);
    db.execute_batch("BEGIN").unwrap();
    for i in 1..2400 {
        entry(&db, "failed", i);
    }
    db.execute_batch("COMMIT").unwrap();
    let mut progress = vec![];
    let result = storage::recover(&db, false, |_, n| {
        progress.push(n);
        Ok(())
    })
    .unwrap();
    assert_eq!(index::latest(&db).unwrap().as_deref(), Some("keep"));
    assert_eq!(
        db.query_row("SELECT count(*) FROM entries", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM receipts", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        index::setting(&db, "ui-preferences").unwrap().unwrap()["locale"],
        "en"
    );
    assert!(
        result["after"]["bytes"].as_u64().unwrap() < result["before"]["bytes"].as_u64().unwrap()
    );
    assert!(progress.iter().any(|v| *v >= 2000));
}
#[test]
fn explicit_rebuild_preserves_user_files_and_receipts_and_upgrades_legacy_database() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("keep-document.txt");
    fs::write(&file, b"user document").unwrap();
    let data = temp.path().join("state");
    let path = index::initialize(&data).unwrap();
    let db = index::connect(&path).unwrap();
    db.execute_batch("PRAGMA auto_vacuum=NONE; VACUUM").unwrap();
    db.execute(
        "INSERT INTO scans VALUES('keep','/','today','complete','{}')",
        [],
    )
    .unwrap();
    entry(&db, "keep", 0);
    db.execute(
        "INSERT INTO receipts VALUES('job','today','/fixture',NULL,'skipped',0,'keep')",
        [],
    )
    .unwrap();
    index::save_setting(&db, "ai-config", &json!({"model":"example"})).unwrap();
    index::save_setting(&db, "last-job", &json!({"kind":"scan"})).unwrap();
    storage::recover(&db, true, |_, _| Ok(())).unwrap();
    assert!(index::latest(&db).unwrap().is_none());
    assert_eq!(fs::read(file).unwrap(), b"user document");
    assert_eq!(
        db.query_row("SELECT count(*) FROM receipts", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        index::setting(&db, "ai-config").unwrap().unwrap()["model"],
        "example"
    );
    assert!(index::setting(&db, "last-job").unwrap().is_none());
    assert_eq!(
        db.query_row("PRAGMA auto_vacuum", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
}
#[test]
fn sqlite_full_never_replaces_the_complete_snapshot_and_can_recover() {
    let temp = tempfile::tempdir().unwrap();
    let path = index::initialize(temp.path()).unwrap();
    let db = index::connect(&path).unwrap();
    db.execute(
        "INSERT INTO scans VALUES('keep','/','today','complete','{}')",
        [],
    )
    .unwrap();
    entry(&db, "keep", 0);
    let pages: u64 = db.query_row("PRAGMA page_count", [], |r| r.get(0)).unwrap();
    db.execute_batch(&format!("PRAGMA max_page_count={}", pages + 20))
        .unwrap();
    db.execute(
        "INSERT INTO scans VALUES('failed','/','tomorrow','running','{}')",
        [],
    )
    .unwrap();
    let mut failed = false;
    for i in 0..1000 {
        let outcome = db.execute(
            "INSERT INTO errors VALUES('failed',?,'TEST',?)",
            params![format!("/{i}"), "fixture".repeat(600)],
        );
        if outcome.is_err() {
            failed = true;
            break;
        }
    }
    assert!(failed);
    assert_eq!(index::latest(&db).unwrap().as_deref(), Some("keep"));
    storage::recover(&db, false, |_, _| Ok(())).unwrap();
    assert_eq!(index::latest(&db).unwrap().as_deref(), Some("keep"));
}
#[test]
fn permission_guidance_is_not_a_permission_grant_and_gui_lock_prevents_duplicate_writers() {
    let temp = tempfile::tempdir().unwrap();
    let path = index::initialize(temp.path()).unwrap();
    let db = index::connect(&path).unwrap();
    assert_eq!(
        access::status(&db).unwrap()["authorization"],
        "system-managed"
    );
    access::acknowledge(&db).unwrap();
    assert_eq!(access::status(&db).unwrap()["setupShown"], true);
    let lock = access::instance_lock(temp.path()).unwrap().unwrap();
    assert!(access::instance_lock(temp.path()).unwrap().is_none());
    drop(lock);
    assert!(access::instance_lock(temp.path()).unwrap().is_some());
    assert_eq!(
        access::bundle_for(Path::new(
            "/Applications/Example.app/Contents/MacOS/Example"
        ))
        .unwrap(),
        Path::new("/Applications/Example.app")
    );
    assert!(access::bundle_for(Path::new("/tmp/not-an-app/Example")).is_none());
}
#[test]
fn rebuild_requires_its_own_explicit_confirmation() {
    let temp = tempfile::tempdir().unwrap();
    let home = fs::canonicalize(temp.path()).unwrap();
    let engine = Engine::new(home.clone(), home.join("state"), home).unwrap();
    assert!(engine
        .dispatch("storage/recover", json!({"reset":true}))
        .is_err());
    assert!(engine
        .dispatch(
            "storage/recover",
            json!({"reset":true,"confirmation":"TRASH_REVIEWED_FILES"})
        )
        .is_err());
}
