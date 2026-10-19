use minorminor_core::{cloud, database, index, read_recovery, reports, scan};
use rusqlite::params;
use serde_json::json;
use std::{fs, os::unix::fs::PermissionsExt, path::Path};

#[test]
fn suffixes_do_not_turn_locks_or_backup_pointers_into_databases() {
    assert_eq!(database::format(b"SQLite format 3\0rest"), "sqlite");
    assert_eq!(database::format(b""), "empty");
    assert_eq!(
        database::format(b"/tmp/fixture.sqlite3\n"),
        "path-reference"
    );
    assert_eq!(database::format(b"/first\n/second"), "unknown");
    assert_eq!(database::format(b"not a database"), "unknown");
    assert_eq!(database::format(&vec![b'/'; 4096]), "unknown");
}

#[test]
fn recovery_rescans_readable_directories_and_preserves_every_unresolved_reason() {
    let t = tempfile::tempdir().unwrap();
    let home = t.path().canonicalize().unwrap().join("home");
    let data = t.path().canonicalize().unwrap().join("data");
    fs::create_dir_all(home.join("restored")).unwrap();
    let path = index::initialize(&data).unwrap();
    scan::run(scan::ScanSpec {
        id: "s".into(),
        root: home.clone(),
        home: home.clone(),
        data: data.clone(),
        disk: false,
        publish: true,
        index_path: None,
    })
    .unwrap();
    let mut db = index::connect(&path).unwrap();
    let before = index::scan_info(&db, "s").unwrap()["stats"]["files"]
        .as_u64()
        .unwrap();
    fs::write(home.join("restored/new.txt"), "recovered file").unwrap();
    fs::write(home.join("empty.lock.sqlite"), []).unwrap();
    fs::write(home.join("pointer.sqlite3"), "/tmp/fixture.sqlite3").unwrap();
    fs::write(home.join("corrupt.sqlite3"), "corrupt").unwrap();
    let cloud = home.join("Library/Mobile Documents/unavailable");
    for (path, code) in [
        (home.join("restored"), "EACCES"),
        (home.join("missing"), "EPERM"),
        (home.join("restricted"), "EACCES"),
        (cloud.clone(), "ETIMEDOUT"),
        (home.join("empty.lock.sqlite"), "UNSUPPORTED_DATABASE"),
        (home.join("pointer.sqlite3"), "DB_READ_ERROR"),
        (home.join("corrupt.sqlite3"), "DB_READ_ERROR"),
    ] {
        db.execute(
            "INSERT INTO errors VALUES('s',?,?,'original')",
            params![path.to_str().unwrap(), code],
        )
        .unwrap();
    }
    let probe = |p: &Path, b| {
        if p.ends_with("restricted") {
            Ok(json!({"code":libc::EACCES}))
        } else if p == cloud {
            Ok(json!({"code":libc::ETIMEDOUT}))
        } else {
            cloud::probe_local(p, b)
        }
    };
    let result = read_recovery::run_with(&mut db, "s", &home, &data, probe).unwrap();
    assert_eq!(
        result["summary"],
        json!({"recovered":1,"resolved":2,"missing":1,"permission":1,"cloud-timeout":1,"format":1})
    );
    assert_eq!(result["remaining"], 4);
    assert_eq!(
        index::scan_info(&db, "s").unwrap()["stats"]["files"],
        before + 1
    );
    assert!(db
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM entries WHERE scan='s' AND path=?)",
            [home.join("restored/new.txt").to_str().unwrap()],
            |r| r.get::<_, bool>(0)
        )
        .unwrap());
    drop(db);
    let mut db = index::connect(&path).unwrap();
    let report = reports::diagnostics(&db, "s", "errors", &json!({})).unwrap();
    assert_eq!(report["total"], 7);
    assert_eq!(
        reports::diagnostics(&db, "s", "errors", &json!({"status":"missing"})).unwrap()["total"],
        1
    );
    read_recovery::run_with(&mut db, "s", &home, &data, |_, _| {
        Ok(json!({"code":libc::ENOENT}))
    })
    .unwrap();
    assert_eq!(
        reports::diagnostics(&db, "s", "errors", &json!({})).unwrap()["total"],
        7,
        "retries update the original audit row, rather than duplicating failures"
    );
    assert_eq!(
        read_recovery::audit(&db, "s")
            .unwrap()
            .iter()
            .find(|r| r["path"] == home.join("missing").to_str().unwrap())
            .unwrap()["originalCode"],
        "EPERM"
    );
    assert!(
        read_recovery::run_with(&mut db, "stale", &home, &data, |_, _| panic!(
            "must reject stale scan before probing"
        ))
        .is_err()
    );
}

#[test]
fn probe_distinguishes_real_access_denial_disappearance_and_symlinks() {
    let t = tempfile::tempdir().unwrap();
    let denied = t.path().canonicalize().unwrap().join("denied");
    fs::create_dir(&denied).unwrap();
    fs::set_permissions(&denied, fs::Permissions::from_mode(0)).unwrap();
    let result = cloud::probe_local(&denied, false);
    fs::set_permissions(&denied, fs::Permissions::from_mode(0o700)).unwrap();
    if unsafe { libc::geteuid() } != 0 {
        assert_eq!(result.unwrap()["code"], libc::EACCES);
    }
    assert_eq!(
        cloud::probe_local(&t.path().canonicalize().unwrap().join("missing"), false)
            .unwrap_err()
            .raw_os_error(),
        Some(libc::ENOENT)
    );
    let link = t.path().canonicalize().unwrap().join("link");
    std::os::unix::fs::symlink(&denied, &link).unwrap();
    assert_eq!(
        cloud::probe_local(&link, false).unwrap_err().raw_os_error(),
        Some(libc::ELOOP)
    );
}

#[test]
fn physical_directory_recovery_keeps_logical_paths_and_parent_totals() {
    use minorminor_core::{files::Resource, measure};
    let t = tempfile::tempdir().unwrap();
    let base = t.path().canonicalize().unwrap();
    let home = base.join("home");
    let data = base.join("data");
    let logical = home.join("logical");
    let physical = home.join("physical");
    fs::create_dir_all(&logical).unwrap();
    fs::write(logical.join("old"), "old").unwrap();
    let path = index::initialize(&data).unwrap();
    scan::run(scan::ScanSpec {
        id: "s".into(),
        root: home.clone(),
        home: home.clone(),
        data: data.clone(),
        disk: false,
        publish: true,
        index_path: None,
    })
    .unwrap();
    fs::rename(&logical, &physical).unwrap();
    fs::write(physical.join("new"), "new").unwrap();
    let metadata = cloud::probe_local(&physical, false).unwrap();
    let metadata: cloud::Metadata = serde_json::from_value(metadata["metadata"].clone()).unwrap();
    let mut db = index::connect(&path).unwrap();
    measure::run_mapped(
        &mut db,
        Resource {
            path: physical,
            root: home.clone(),
            scan: "s".into(),
            identity: metadata.identity,
            directory: true,
        },
        &logical,
        &home,
        &data,
    )
    .unwrap();
    assert_eq!(
        db.query_row(
            "SELECT files FROM entries WHERE scan='s' AND path=?",
            [logical.to_str().unwrap()],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        2
    );
    assert_eq!(
        db.query_row(
            "SELECT files FROM entries WHERE scan='s' AND path=?",
            [home.to_str().unwrap()],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        2
    );
    assert!(db
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM entries WHERE scan='s' AND path=?)",
            [logical.join("new").to_str().unwrap()],
            |r| r.get::<_, bool>(0)
        )
        .unwrap());
}

#[cfg(target_os = "macos")]
#[test]
fn absent_data_volume_alias_is_not_misreported_as_disappearance() {
    let t = tempfile::tempdir().unwrap();
    let base = t.path().canonicalize().unwrap();
    let home = base.join("home");
    let data = base.join("data");
    let target = home.join("fixture");
    fs::create_dir_all(&target).unwrap();
    let path = index::initialize(&data).unwrap();
    scan::run(scan::ScanSpec {
        id: "s".into(),
        root: home.clone(),
        home: home.clone(),
        data: data.clone(),
        disk: false,
        publish: true,
        index_path: None,
    })
    .unwrap();
    let mut db = index::connect(&path).unwrap();
    db.execute(
        "UPDATE scans SET root='/System/Volumes/Data' WHERE id='s'",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO errors VALUES('s',?,'EPERM','fixture')",
        [target.to_str().unwrap()],
    )
    .unwrap();
    let result = read_recovery::run_with(&mut db, "s", &home, &data, |p, b| {
        if p == target {
            Ok(json!({"code":libc::ENOENT}))
        } else {
            let mut row = cloud::probe_local(p, b)?;
            row["code"] = json!(libc::EPERM);
            Ok(row)
        }
    })
    .unwrap();
    assert_eq!(result["summary"], json!({"permission":1}));
    let audit = read_recovery::audit(&db, "s").unwrap();
    assert!(audit[0]["actualPath"]
        .as_str()
        .unwrap()
        .starts_with("/System/Volumes/Data/"));
    assert_eq!(audit[0]["code"], "EPERM");
}
