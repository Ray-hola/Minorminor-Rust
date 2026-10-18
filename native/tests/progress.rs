use minorminor_core::{
    index,
    server::{merge_progress, Engine},
};
use serde_json::json;
use std::fs;
#[test]
fn stage_changes_keep_disk_counts_and_clear_the_previous_stage_total() {
    let mut job = json!({"stats":{"stage":"storage","files":1200,"directories":50,"bytes":4096,"completed":50,"total":50}});
    merge_progress(&mut job, &json!({"stage":"models"}));
    assert_eq!(job["stats"]["files"], 1200);
    assert_eq!(job["stats"]["bytes"], 4096);
    assert!(job["stats"].get("total").is_none());
    assert!(job["progressAt"].is_string());
}
#[test]
fn status_and_cancel_do_not_need_a_working_database() {
    let temp = tempfile::tempdir().unwrap();
    let home = fs::canonicalize(temp.path()).unwrap();
    let data = home.join("state");
    let engine = Engine::new(home.clone(), data.clone(), home).unwrap();
    fs::rename(&data, data.with_extension("unavailable")).unwrap();
    assert!(engine.dispatch("jobs", json!({})).is_ok());
    assert!(engine.dispatch("cancel", json!({})).is_ok());
}
#[test]
fn index_pruning_commits_batches_and_keeps_the_complete_snapshot() {
    let temp = tempfile::tempdir().unwrap();
    let path = index::initialize(temp.path()).unwrap();
    let db = index::connect(&path).unwrap();
    db.execute(
        "INSERT INTO scans(id,root,created,status) VALUES('keep','/','today','complete')",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO scans(id,root,created,status) VALUES('old','/','yesterday','failed')",
        [],
    )
    .unwrap();
    db.execute_batch("BEGIN").unwrap();
    for i in 0..2100 {
        db.execute(
            "INSERT INTO errors(scan,path,code,reason) VALUES('old',?,'TEST','fixture')",
            [format!("/{i}")],
        )
        .unwrap();
    }
    db.execute_batch("COMMIT").unwrap();
    let mut counts = vec![];
    index::prune_with_progress(&db, |_, removed| {
        assert!(db.is_autocommit());
        counts.push(removed);
        Ok(())
    })
    .unwrap();
    assert!(counts.contains(&1000));
    assert!(counts.contains(&2000));
    assert_eq!(counts.last(), Some(&2100));
    assert_eq!(index::latest(&db).unwrap().as_deref(), Some("keep"));
}

#[test]
fn scan_estimate_uses_only_a_complete_snapshot_of_the_same_root() {
    use minorminor_core::server::scan_estimate;
    let temp = tempfile::tempdir().unwrap();
    let path = index::initialize(temp.path()).unwrap();
    let db = index::connect(&path).unwrap();
    let capacity = json!({"used":1000,"free":200,"total":1200});
    let root = std::path::Path::new("/Users/example");
    assert_eq!(
        scan_estimate(&db, root, &capacity).unwrap(),
        json!({"estimatedBytes":1000,"estimateSource":"volume-used"})
    );
    for (id, root, status, bytes) in [
        ("home", "/Users/example", "complete", 400),
        ("disk", "/", "complete", 900),
        ("failed", "/Users/example", "failed", 20),
        ("partial", "/Users/example", "index-complete", 600),
    ] {
        db.execute(
            "INSERT INTO scans(id,root,created,status,stats) VALUES(?,?,?,?,?)",
            rusqlite::params![
                id,
                root,
                "today",
                status,
                json!({"bytes":bytes}).to_string()
            ],
        )
        .unwrap();
    }
    assert_eq!(
        scan_estimate(&db, root, &capacity).unwrap(),
        json!({"estimatedBytes":400,"estimateSource":"previous-scan"})
    );
    assert_eq!(
        scan_estimate(&db, std::path::Path::new("/Users/another"), &capacity).unwrap()
            ["estimatedBytes"],
        1000
    );
}
#[test]
fn missing_or_zero_capacity_does_not_create_a_false_scan_total() {
    use minorminor_core::server::{disk_status, scan_estimate};
    let temp = tempfile::tempdir().unwrap();
    let path = index::initialize(temp.path()).unwrap();
    let db = index::connect(&path).unwrap();
    assert_eq!(
        scan_estimate(&db, temp.path(), &json!({"used":0})).unwrap()["estimatedBytes"],
        serde_json::Value::Null
    );
    let capacity = disk_status(temp.path()).unwrap();
    assert!(capacity["total"].as_u64().unwrap() > 0);
    assert!(capacity["capturedAt"].is_string());
    assert_eq!(
        capacity["used"].as_u64().unwrap() + capacity["free"].as_u64().unwrap(),
        capacity["total"].as_u64().unwrap()
    );
}
