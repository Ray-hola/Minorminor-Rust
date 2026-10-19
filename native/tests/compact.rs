use minorminor_core::{compact, index, storage};
use rusqlite::{params, Connection};
use serde_json::json;

fn legacy(db: &Connection) {
    db.execute_batch("DROP VIEW entries; DROP TABLE _entries; DROP TABLE entry_scans; DROP TABLE entry_parents;
      CREATE TABLE entries(scan TEXT NOT NULL,path TEXT NOT NULL,parent TEXT NOT NULL,name TEXT NOT NULL,directory INTEGER NOT NULL,
      bytes INTEGER NOT NULL,logical INTEGER NOT NULL,unique_bytes INTEGER NOT NULL,files INTEGER NOT NULL DEFAULT 0,dirs INTEGER NOT NULL DEFAULT 0,
      identity TEXT NOT NULL,category TEXT NOT NULL,project TEXT,tool TEXT,skipped INTEGER NOT NULL DEFAULT 0,PRIMARY KEY(scan,path));
      CREATE INDEX entries_parent ON entries(scan,parent); CREATE INDEX entries_project ON entries(scan,project,category); CREATE INDEX entries_tool ON entries(scan,tool);
      PRAGMA auto_vacuum=NONE; VACUUM;").unwrap();
    index::prepare_scan_indexes(db, |_| Ok(())).unwrap();
}
fn records(db: &Connection) -> Vec<String> {
    let mut s=db.prepare("SELECT scan,path,parent,name,directory,bytes,logical,unique_bytes,files,dirs,identity,category,project,tool,skipped FROM entries ORDER BY scan,path").unwrap();
    s.query_map([], |r| {
        let text = |i| r.get::<_, String>(i);
        Ok(json!([
            text(0)?,
            text(1)?,
            text(2)?,
            text(3)?,
            r.get::<_, i64>(4)?,
            r.get::<_, i64>(5)?,
            r.get::<_, i64>(6)?,
            r.get::<_, i64>(7)?,
            r.get::<_, i64>(8)?,
            r.get::<_, i64>(9)?,
            serde_json::from_str::<serde_json::Value>(&text(10)?).unwrap(),
            text(11)?,
            r.get::<_, Option<String>>(12)?,
            r.get::<_, Option<String>>(13)?,
            r.get::<_, i64>(14)?
        ])
        .to_string())
    })
    .unwrap()
    .collect::<std::result::Result<_, _>>()
    .unwrap()
}

#[test]
fn wide_index_upgrade_is_lossless_and_rolls_back_interruption_at_publication() {
    let temp = tempfile::tempdir().unwrap();
    let path = index::initialize(temp.path()).unwrap();
    let db = index::connect(&path).unwrap();
    let table: String = db
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name='_entries'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let schema = db.prepare("SELECT sql FROM sqlite_master WHERE name='entries' OR type='trigger' AND tbl_name='entries'").unwrap()
        .query_map([], |r| r.get::<_,String>(0)).unwrap().collect::<std::result::Result<Vec<_>,_>>().unwrap();
    db.execute_batch("DROP VIEW entries; DROP TABLE _entries")
        .unwrap();
    db.execute_batch(&format!("{table} WITHOUT ROWID")).unwrap();
    for sql in schema {
        db.execute_batch(&sql).unwrap();
    }
    compact::indexes(&db, |_| Ok(())).unwrap();
    db.execute_batch("BEGIN").unwrap();
    for scan in ["one", "two"] {
        db.execute(
            "INSERT INTO scans VALUES(?, '/fixture','today','complete','{}')",
            [scan],
        )
        .unwrap();
        for i in 0..6001_u64 {
            let parent = format!(
                "/fixture/项目/Library/Application Support/long-owned-project-directory/group-{}",
                i / 25
            );
            let identity = json!({"dev":u64::MAX,"ino":i,"size":u64::MAX-1,"mtime":-2,"mtime_ns":123,"ctime":4,"ctime_ns":5,"nlink":if i%10==0{2}else{1}});
            db.execute("INSERT INTO entries(scan,path,parent,name,directory,bytes,logical,unique_bytes,files,dirs,identity,category,project,tool,skipped) VALUES(?,?,?,?,?,9,8,7,6,5,?,'cache',?,?,4)",params![scan,format!("{parent}/file-{i}"),parent,format!("file-{i}"),i%3==0,identity.to_string(),if i%2==0{Some("12345678-1234-1234-1234-123456789abc")}else{None},if i%5==0{Some("synthetic-tool")}else{None}]).unwrap();
        }
    }
    db.execute_batch("COMMIT").unwrap();
    index::save_setting(&db, "ui-preferences", &json!({"locale":"en"})).unwrap();
    db.execute(
        "INSERT INTO receipts VALUES('keep','today','/synthetic',NULL,'skipped',0,'keep')",
        [],
    )
    .unwrap();
    let before = records(&db);
    let latest = index::latest(&db).unwrap();
    assert!(compact::needs_upgrade(&db).unwrap());
    assert!(compact::migrate(&db, |stage, count| {
        if stage == "metadata" && count >= 2000 {
            Err(minorminor_core::fail("interrupted copy"))
        } else {
            Ok(())
        }
    })
    .is_err());
    assert_eq!(records(&db), before);
    assert!(compact::needs_upgrade(&db).unwrap());
    assert!(compact::migrate(&db, |stage, _| {
        if stage == "projects" {
            Err(minorminor_core::fail("interrupted publication"))
        } else {
            Ok(())
        }
    })
    .is_err());
    assert_eq!(records(&db), before);
    assert!(compact::needs_upgrade(&db).unwrap());
    compact::migrate(&db, |_, _| Ok(())).unwrap();
    assert!(!compact::needs_upgrade(&db).unwrap());
    assert_eq!(records(&db), before);
    assert_eq!(index::latest(&db).unwrap(), latest);
    assert_eq!(
        index::setting(&db, "ui-preferences").unwrap().unwrap()["locale"],
        "en"
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM receipts", [], |r| r.get::<_, u64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        db.query_row("PRAGMA quick_check", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
    let mut stmt = db.prepare("PRAGMA index_xinfo(entries_parent)").unwrap();
    let columns = stmt
        .query_map([], |r| r.get::<_, Option<String>>(2))
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap();
    assert!(
        !columns.iter().flatten().any(|name| name == "path"),
        "secondary indexes must not copy full paths: {columns:?}"
    );
}
#[test]
fn legacy_migration_is_lossless_restartable_smaller_and_physically_reclaimed() {
    let temp = tempfile::tempdir().unwrap();
    let path = index::initialize(temp.path()).unwrap();
    let db = index::connect(&path).unwrap();
    legacy(&db);
    let scan = "12345678-1234-1234-1234-123456789abc";
    db.execute(
        "INSERT INTO scans VALUES(?,'/fixture','today','complete','{}')",
        [scan],
    )
    .unwrap();
    index::save_setting(
        &db,
        "ai-config",
        &json!({"model":"keep","apiKey":"synthetic-test-only"}),
    )
    .unwrap();
    db.execute(
        "INSERT INTO receipts VALUES('job','today','/synthetic',NULL,'skipped',0,'keep')",
        [],
    )
    .unwrap();
    db.execute_batch("BEGIN").unwrap();
    for i in 0..20000_u64 {
        let parent=format!("/fixture/项目/{}/very-long-directory/Library/Application Support/synthetic-agent/projects/project-{}",i/100,i/100);
        let name = if i % 100 == 0 {
            "package.json".to_owned()
        } else {
            format!("file-{i}.bin")
        };
        let identity = json!({"dev":u64::MAX,"ino":i+1,"size":u64::MAX,"mtime":-1234,"mtime_ns":999999999,"ctime":1700000000,"ctime_ns":123456789,"nlink":if i%19==0{2}else{1}});
        db.execute("INSERT INTO entries(scan,path,parent,name,directory,bytes,logical,unique_bytes,files,dirs,identity,category,project,tool,skipped) VALUES(?,?,?,?,0,4096,100,4096,1,0,?,'cache',?,NULL,?)",params![scan,format!("{parent}/{name}"),parent,name,identity.to_string(),if i%3==0{Some("fixture-project")}else{None},i%5]).unwrap();
    }
    db.execute_batch(
        "COMMIT; PRAGMA wal_checkpoint(TRUNCATE); VACUUM; PRAGMA wal_checkpoint(TRUNCATE)",
    )
    .unwrap();
    let before = storage::status(&db).unwrap()["bytes"].as_u64().unwrap();
    let expected = records(&db);
    assert!(compact::migrate(&db, |_, n| if n >= 1000 {
        let mut stmt = db
            .prepare(&format!("EXPLAIN QUERY PLAN {}", compact::VERIFY_BATCH))
            .unwrap();
        let plan = stmt
            .query_map([0_i64, 1000], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        assert!(!plan.iter().any(|p| p.contains("MATERIALIZE")), "{plan:?}");
        assert!(
            plan.iter()
                .any(|p| p.contains("INDEX") && p.contains("(scan=? AND path=?)")),
            "{plan:?}"
        );
        Err(minorminor_core::fail("simulated interruption"))
    } else {
        Ok(())
    })
    .is_err());
    assert!(compact::is_legacy(&db).unwrap());
    assert_eq!(records(&db), expected);
    compact::migrate(&db, |_, _| Ok(())).unwrap();
    assert!(!compact::is_legacy(&db).unwrap());
    assert_eq!(records(&db), expected);
    assert_eq!(index::latest(&db).unwrap().as_deref(), Some(scan));
    let config = index::setting(&db, "ai-config").unwrap();
    assert!(storage::prepare_repack(&db, |_, _| Ok(())).unwrap());
    drop(db);
    let after = storage::install_repack(temp.path()).unwrap();
    let db = index::connect(&path).unwrap();
    assert_eq!(records(&db), expected);
    assert_eq!(index::setting(&db, "ai-config").unwrap(), config);
    assert_eq!(
        db.query_row("SELECT count(*) FROM receipts", [], |r| r.get::<_, u64>(0))
            .unwrap(),
        1
    );
    assert_eq!(after["incremental"], true);
    let after = after["bytes"].as_u64().unwrap();
    eprintln!(
        "compact benchmark: 20000 records, before={before}, after={after}, reduction={:.1}%",
        100.0 * (1.0 - after as f64 / before as f64)
    );
    assert!(
        after * 100 < before * 65,
        "insufficient reduction: {before} -> {after}"
    );
    // Changing scan/category and aggregate counters must keep normalized joins coherent.
    db.execute("UPDATE entries SET bytes=123,skipped=4,project='new' WHERE scan=? AND path=(SELECT min(path) FROM entries WHERE scan=?)",params![scan,scan]).unwrap();
    assert_eq!(
        db.query_row("SELECT bytes FROM entries WHERE project='new'", [], |r| r
            .get::<_, u64>(
            0
        ))
        .unwrap(),
        123
    );
}
#[test]
fn parent_lookup_is_indexed_and_records_do_not_cross_snapshots() {
    let temp = tempfile::tempdir().unwrap();
    let path = index::initialize(temp.path()).unwrap();
    let db = index::connect(&path).unwrap();
    db.execute_batch("BEGIN").unwrap();
    for scan in ["one", "two"] {
        for i in 0..5000 {
            let parent = format!("/fixture/{}", i / 10);
            db.execute("INSERT INTO entries(scan,path,parent,name,directory,bytes,logical,unique_bytes,identity,category) VALUES(?,?,?,'file',0,1,1,1,'{}','cache')",params![scan,format!("{parent}/{i}"),parent]).unwrap();
        }
    }
    db.execute_batch("COMMIT; ANALYZE").unwrap();
    let sql = "SELECT path FROM entries WHERE scan=? AND parent=?";
    let mut stmt = db.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
    let plan = stmt
        .query_map(params!["one", "/fixture/20"], |r| r.get::<_, String>(3))
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap();
    assert!(
        plan.iter().any(|p| p.contains("entries_parent")),
        "{plan:?}"
    );
    let children = |query: &str| {
        db.prepare(query)
            .unwrap()
            .query_map(params!["one", "/fixture/20", "/fixture/20"], |r| {
                Ok((r.get::<_, String>(1)?, r.get::<_, u64>(2)?))
            })
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap()
    };
    assert_eq!(children(compact::CHILDREN), children("SELECT name,path,bytes,logical,files,dirs,directory,skipped FROM entries WHERE scan=?1 AND parent=?2 AND path<>?3 ORDER BY bytes DESC"));
    let plan = db
        .prepare(&format!("EXPLAIN QUERY PLAN {}", compact::CHILDREN))
        .unwrap()
        .query_map(params!["one", "/fixture/20", "/fixture/20"], |r| {
            r.get::<_, String>(3)
        })
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap();
    assert!(
        plan.iter()
            .any(|line| line.contains("entries_parent (parent=? AND scan=?)")),
        "{plan:?}"
    );
    db.execute(
        "DELETE FROM entries WHERE scan='one' AND path='/fixture/20/200'",
        [],
    )
    .unwrap();
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM entries WHERE path='/fixture/20/200'",
            [],
            |r| r.get::<_, u64>(0)
        )
        .unwrap(),
        1
    );
    db.execute("INSERT OR REPLACE INTO entries(scan,path,parent,name,directory,bytes,logical,unique_bytes,identity,category) VALUES('two','/fixture/20/200','/fixture/20','file',0,99,1,1,'{}','log')",[]).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT bytes FROM entries WHERE scan='two' AND path='/fixture/20/200'",
            [],
            |r| r.get::<_, u64>(0)
        )
        .unwrap(),
        99
    );
}

#[test]
fn replacing_an_entry_never_replaces_shared_scan_or_directory_tokens() {
    let temp = tempfile::tempdir().unwrap();
    let file = index::initialize(temp.path()).unwrap();
    let db = index::connect(&file).unwrap();
    for (path, parent) in [
        ("/a/one", "/a"),
        ("/a/two", "/a"),
        ("/b/three", "/b"),
        ("/a/one", "/a"),
    ] {
        db.execute("INSERT OR REPLACE INTO entries(scan,path,parent,name,directory,bytes,logical,unique_bytes,identity,category) VALUES('snapshot',?,?,'file',0,1,1,1,'{}','other')",params![path,parent]).unwrap();
    }
    assert_eq!(
        db.query_row("SELECT count(*) FROM entries", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        3
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM _entries", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        3
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM entry_scans", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM entry_parents", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        2
    );
}
