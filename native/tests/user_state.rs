use minorminor_core::{fs_compression, index, user_state};
use rusqlite::Connection;
use serde_json::json;
use std::fs;

#[test]
fn migration_preserves_rows_identity_and_retries_after_committed_copy_interruption() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("legacy.sqlite3");
    let db = Connection::open(&path).unwrap();
    db.execute_batch("CREATE TABLE settings(key TEXT PRIMARY KEY,value TEXT NOT NULL); CREATE TABLE receipts(job TEXT NOT NULL,created TEXT NOT NULL,path TEXT NOT NULL,target TEXT,status TEXT NOT NULL,bytes INTEGER NOT NULL,reason TEXT NOT NULL);
      INSERT INTO settings VALUES('ai-config','{\"apiKey\":\"synthetic-only\"}'); INSERT INTO receipts(rowid,job,created,path,target,status,bytes,reason) VALUES(10,'old','today','/synthetic',NULL,'skipped',11,'keep'),(22,'new','tomorrow','/synthetic2','/synthetic3','moved',33,'keep');").unwrap();
    user_state::attach(&db, &path).unwrap();
    // Make the destination schemas without prematurely migrating this legacy DB.
    db.execute_batch("CREATE TABLE user_state.settings(key TEXT PRIMARY KEY,value TEXT NOT NULL); CREATE TABLE user_state.receipts(job TEXT NOT NULL,created TEXT NOT NULL,path TEXT NOT NULL,target TEXT,status TEXT NOT NULL,bytes INTEGER NOT NULL,reason TEXT NOT NULL);").unwrap();
    assert!(
        user_state::migrate(&db, || Err(minorminor_core::fail("interrupted after copy"))).is_err()
    );
    assert_eq!(
        db.query_row(
            "SELECT value FROM main.settings WHERE key='ai-config'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "{\"apiKey\":\"synthetic-only\"}"
    );
    db.execute(
        "UPDATE main.settings SET value='newer' WHERE key='ai-config'",
        [],
    )
    .unwrap();
    user_state::initialize(&db).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT value FROM settings WHERE key='ai-config'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "newer"
    );
    let ids = db
        .prepare("SELECT rowid FROM receipts ORDER BY rowid")
        .unwrap()
        .query_map([], |r| r.get::<_, i64>(0))
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(ids, vec![10, 22]);
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM main.sqlite_master WHERE name IN ('settings','receipts')",
            [],
            |r| r.get::<_, u64>(0)
        )
        .unwrap(),
        0
    );
    user_state::initialize(&db).unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM receipts", [], |r| r.get::<_, u64>(0))
            .unwrap(),
        2
    );
}

#[cfg(target_os = "macos")]
#[test]
fn settings_and_receipts_stay_writable_without_expanding_a_readonly_compressed_index() {
    use std::os::{macos::fs::MetadataExt as DarwinMetadataExt, unix::fs::MetadataExt};
    let temp = tempfile::tempdir().unwrap();
    let path = index::initialize(temp.path()).unwrap();
    let db = index::connect(&path).unwrap();
    db.execute(
        "INSERT INTO scans VALUES('keep','/fixture','today','complete','{}')",
        [],
    )
    .unwrap();
    db.execute_batch("CREATE TABLE padding(value BLOB); INSERT INTO padding VALUES(zeroblob(2000000)); PRAGMA main.wal_checkpoint(TRUNCATE)").unwrap();
    index::save_setting(&db, "ai-config", &json!({"apiKey":"synthetic-only"})).unwrap();
    drop(db);
    let compressed = temp.path().join("compressed.sqlite3");
    fs_compression::copy(&path, &compressed, |_, _, _| Ok(()))
        .unwrap()
        .unwrap();
    fs::rename(&compressed, &path).unwrap();
    let before = path.metadata().unwrap();
    let db = index::connect_readonly(&path).unwrap();
    assert_eq!(index::latest(&db).unwrap().as_deref(), Some("keep"));
    db.execute_batch("BEGIN IMMEDIATE").unwrap();
    index::save_setting(&db, "locale", &json!("en")).unwrap();
    db.execute(
        "INSERT INTO receipts VALUES('job','today','/synthetic',NULL,'skipped',0,'keep')",
        [],
    )
    .unwrap();
    db.execute_batch("COMMIT").unwrap();
    drop(db);
    // Initialization may revoke stale execution authority, but must not open
    // the compressed main file for writing even when it performs no main writes.
    index::initialize(temp.path()).unwrap();
    let after = path.metadata().unwrap();
    assert_eq!(after.st_flags() & libc::UF_COMPRESSED, libc::UF_COMPRESSED);
    assert_eq!(before.blocks(), after.blocks());
    let db = index::connect_readonly(&path).unwrap();
    assert_eq!(index::setting(&db, "locale").unwrap(), Some(json!("en")));
    assert_eq!(
        db.query_row("SELECT count(*) FROM receipts", [], |r| r.get::<_, u64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn filename_upgrade_keeps_history_and_ignores_files_recreated_by_an_older_app() {
    let temp = tempfile::tempdir().unwrap();
    let modern = index::initialize(temp.path()).unwrap();
    let db = index::connect(&modern).unwrap();
    db.execute(
        "INSERT INTO scans VALUES('retained','/fixture','today','complete','{}')",
        [],
    )
    .unwrap();
    index::save_setting(&db, "locale", &json!("en")).unwrap();
    db.execute(
        "INSERT INTO receipts VALUES('retained','today','/fixture',NULL,'skipped',1,'keep')",
        [],
    )
    .unwrap();
    db.execute_batch("PRAGMA main.wal_checkpoint(TRUNCATE)")
        .unwrap();
    drop(db);
    let legacy = temp.path().join("index.sqlite3");
    fs::rename(&modern, &legacy).unwrap();
    assert_eq!(index::initialize(temp.path()).unwrap(), modern);
    assert!(!legacy.exists());
    let old = Connection::open(&legacy).unwrap();
    old.execute_batch("CREATE TABLE settings(key TEXT PRIMARY KEY,value TEXT NOT NULL); INSERT INTO settings VALUES('locale','\"zh-CN\"')").unwrap();
    drop(old);
    index::initialize(temp.path()).unwrap();
    let db = index::connect_readonly(&modern).unwrap();
    assert_eq!(index::latest(&db).unwrap().as_deref(), Some("retained"));
    assert_eq!(index::setting(&db, "locale").unwrap(), Some(json!("en")));
    assert_eq!(
        db.query_row("SELECT count(*) FROM receipts", [], |r| r.get::<_, u64>(0))
            .unwrap(),
        1
    );
    assert!(legacy.exists());
}

#[test]
fn filename_upgrade_preserves_an_unrecognized_legacy_file() {
    let temp = tempfile::tempdir().unwrap();
    let legacy = temp.path().join("index.sqlite3");
    let db = Connection::open(&legacy).unwrap();
    db.execute_batch("CREATE TABLE unrelated(value TEXT); INSERT INTO unrelated VALUES('keep')")
        .unwrap();
    drop(db);
    assert!(index::initialize(temp.path()).is_err());
    assert!(!index::path(temp.path()).exists());
    let db = Connection::open(&legacy).unwrap();
    assert_eq!(
        db.query_row("SELECT value FROM unrelated", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "keep"
    );
}
