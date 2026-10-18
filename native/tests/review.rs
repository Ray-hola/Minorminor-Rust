use minorminor_core::{cleanup, scan, server::Engine};
use serde_json::json;
use std::{fs, path::Path};

#[test]
fn recently_written_files_are_retained() {
    let temp = tempfile::tempdir().unwrap();
    let home = fs::canonicalize(temp.path()).unwrap();
    let path = home.join("old.log");
    fs::write(&path, "fixture").unwrap();
    let metadata = fs::metadata(&path).unwrap();
    assert!(cleanup::block(&path, &metadata, "log", &home, false).is_some());
}

#[test]
fn database_sidecars_are_retained_even_in_log_or_cache_folders() {
    for suffix in [
        "sqlite",
        "sqlite-wal",
        "sqlite-shm",
        "db-wal",
        "db-shm",
        "vscdb-wal",
        "vscdb-shm",
    ] {
        let name = format!("/Users/example/Library/Logs/state.{suffix}");
        assert!(scan::protected(Path::new(&name)), "{name}");
        assert_eq!(scan::category(Path::new(&name), true), "other");
    }
}

#[test]
fn native_language_preference_survives_engine_restart_and_rejects_extra_fields() {
    let temp = tempfile::tempdir().unwrap();
    let home = fs::canonicalize(temp.path()).unwrap();
    let data = home.join("state");
    {
        let engine = Engine::new(home.clone(), data.clone(), home.clone()).unwrap();
        assert!(engine
            .dispatch("preferences", json!({"locale":"fr"}))
            .is_err());
        assert!(engine
            .dispatch("preferences", json!({"locale":"en","path":"/tmp"}))
            .is_err());
        assert_eq!(
            engine
                .dispatch("preferences", json!({"locale":"en"}))
                .unwrap(),
            json!({"locale":"en"})
        );
    }
    let engine = Engine::new(home.clone(), data, home).unwrap();
    let boot = engine.dispatch("bootstrap", json!({})).unwrap();
    assert_eq!(boot["preferences"], json!({"locale":"en"}));
    assert_eq!(boot["config"]["permissions"]["analysisDocumentRead"], false);
}

#[test]
fn session_header_metadata_links_nonstandard_layouts_and_nearby_memory() {
    use minorminor_core::{files, id, index};
    let temp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let home = root.join("home");
    let data = root.join("state");
    let project = home.join("Atlas");
    let sessions = home.join(".claude/projects/encoded-project");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(sessions.join("memory")).unwrap();
    fs::write(project.join("package.json"), "{}").unwrap();
    fs::write(
        sessions.join("session.jsonl"),
        json!({"type":"user","cwd":project,"message":{"content":"PRIVATE_CONTENT_NOT_INDEXED"}})
            .to_string(),
    )
    .unwrap();
    fs::write(sessions.join("memory/overview.md"), "Project conventions").unwrap();
    fs::create_dir_all(home.join(".cache/opencode")).unwrap();
    let db_path = index::initialize(&data).unwrap();
    let scan_id = id();
    scan::run(scan::ScanSpec {
        id: scan_id.clone(),
        root: home.clone(),
        home: home.clone(),
        data,
        disk: false,
        publish: true,
    })
    .unwrap();
    let db = index::connect(&db_path).unwrap();
    let result = files::projects(&db, &scan_id).unwrap();
    let atlas = result["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "Atlas")
        .unwrap();
    assert_eq!(atlas["sessions"], 1);
    assert_eq!(atlas["categories"]["session"]["files"], 1);
    assert_eq!(atlas["categories"]["memory"]["files"], 1);
    let resources = files::Resources::default();
    let page = files::list(
        &db,
        &resources,
        &home,
        &scan_id,
        &json!({"project": atlas["id"], "kind": "memory"}),
    )
    .unwrap();
    let selected = resources
        .get(page["rows"][0]["id"].as_str().unwrap(), &scan_id)
        .unwrap();
    assert_eq!(selected.root, selected.path);
    assert_eq!(
        files::inspect(&selected).unwrap()["excerpt"],
        "Project conventions"
    );
    let titles: String = db
        .query_row(
            "SELECT group_concat(title,' ') FROM associations",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!titles.contains("PRIVATE_CONTENT_NOT_INDEXED"));
    let count: i64 = db
        .query_row(
            "SELECT count(*) FROM tools WHERE name='OpenCode'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(count > 0);
}

#[test]
fn compressed_excerpts_report_the_decompressed_read_limit() {
    use minorminor_core::{ai, files, index::Identity};
    use std::io::Write;
    let temp = tempfile::tempdir().unwrap();
    let home = fs::canonicalize(temp.path()).unwrap();
    let path = home.join("selected.txt.gz");
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&vec![b'a'; 100_000]).unwrap();
    fs::write(&path, encoder.finish().unwrap()).unwrap();
    let resource = files::Resource {
        path: path.clone(),
        root: home.clone(),
        scan: "fixture".into(),
        identity: Identity::of(&fs::metadata(path).unwrap()),
        directory: false,
    };
    assert_eq!(files::inspect(&resource).unwrap()["truncated"], true);
    let prepared = ai::prepare(resource, &home, 0).unwrap();
    assert_eq!(prepared.coverage["filesRead"], 1);
    assert_eq!(prepared.coverage["truncated"], true);
}
