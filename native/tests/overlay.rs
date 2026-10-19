use minorminor_core::{files, index, overlay, scan, snapshot};
use serde_json::json;
#[test]
fn partial_refresh_is_transactional_persistent_and_keeps_compressed_bytes_unchanged() {
    let t = tempfile::tempdir().unwrap();
    let path = index::initialize(t.path()).unwrap();
    let work = snapshot::Work::prepare(t.path(), "delta-fixture", false).unwrap();
    work.claim().unwrap();
    {
        let db = index::connect(&work.path).unwrap();
        db.execute(
            "INSERT INTO scans VALUES('s','/example','today','complete','{}')",
            [],
        )
        .unwrap();
        db.execute(
            "INSERT INTO projects VALUES('s','p','/example','Example','[]')",
            [],
        )
        .unwrap();
        for (path, parent, name, directory, bytes) in [
            ("/example", "/", "example", 1, 10),
            ("/example/old", "/example", "old", 0, 10),
        ] {
            db.execute("INSERT INTO entries(scan,path,parent,name,directory,bytes,logical,unique_bytes,files,identity,category,project) VALUES('s',?,?,?,?,?,?,?,?,'{\"dev\":1,\"ino\":2}','cache','p')",rusqlite::params![path,parent,name,directory,bytes,bytes,bytes,1-directory]).unwrap();
        }
        db.execute(
            "INSERT INTO errors VALUES('s','/example/blocked','EACCES','before')",
            [],
        )
        .unwrap();
    }
    work.finish(false, |_, _, _| Ok(())).unwrap();
    let before = std::fs::metadata(&path).unwrap();
    {
        let db = index::connect(&path).unwrap();
        overlay::enable(&db).unwrap();
        db.execute_batch("BEGIN;DELETE FROM errors WHERE scan='s';INSERT INTO errors VALUES('s','/example/gone','ENOENT','after');ROLLBACK").unwrap();
        assert_eq!(
            db.query_row("SELECT code FROM errors", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "EACCES"
        );
        db.execute_batch("BEGIN;DELETE FROM errors WHERE scan='s';INSERT INTO errors VALUES('s','/example/gone','ENOENT','after');COMMIT").unwrap();
        db.execute_batch("BEGIN; DELETE FROM entries WHERE path='/example/old'; UPDATE entries SET bytes=100 WHERE path='/example'; ROLLBACK").unwrap();
        assert_eq!(
            db.query_row("SELECT bytes FROM entries WHERE path='/example'", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            10
        );
        db.execute_batch("BEGIN; DELETE FROM entries WHERE path='/example/old'; INSERT INTO entries(scan,path,parent,name,directory,bytes,logical,unique_bytes,files,identity,category,project) VALUES('s','/example/new','/example','new',0,20,20,20,1,'{\"dev\":1,\"ino\":3}','cache','p');UPDATE entries SET bytes=20,logical=20,unique_bytes=20 WHERE path='/example';UPDATE scans SET stats='{\"files\":1}' WHERE id='s';COMMIT").unwrap();
    }
    {
        let db = index::connect(&path).unwrap();
        assert!(overlay::active(&db).unwrap());
        assert_eq!(
            db.query_row("SELECT code FROM errors", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "ENOENT"
        );
        assert_eq!(
            db.query_row("SELECT code FROM main.errors", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "EACCES"
        );
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM entries WHERE path='/example/old'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM entries", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            2
        );
        assert_eq!(index::scan_info(&db, "s").unwrap()["stats"]["files"], 1);
        assert_eq!(scan::overview(&db, "s").unwrap()["rows"][0]["bytes"], 20);
        // Compare the indexed project aggregate with its logical-view reference.
        let totals = files::projects(&db, "s").unwrap();
        assert_eq!(totals["rows"][0]["bytes"], 20);
        assert_eq!(
            db.query_row(
                "SELECT bytes FROM main.entries WHERE path='/example'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            10
        );
        index::save_setting(&db, "locale", &json!("en")).unwrap();
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::MetadataExt;
        assert!(minorminor_core::fs_compression::is_compressed(&path));
        assert_eq!(path.metadata().unwrap().blocks(), before.blocks());
    }
    let private = snapshot::Work::prepare(t.path(), "materialize-fixture", true).unwrap();
    let db = index::connect(&private.path).unwrap();
    assert_eq!(
        db.query_row("SELECT code FROM main.errors", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "ENOENT"
    );
    assert_eq!(
        db.query_row(
            "SELECT bytes FROM main.entries WHERE path='/example'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        20
    );
}
