use minorminor_core::{
    ai, cleanup, discovery,
    files::{self, Resource, Resources},
    id,
    index::{self, Identity},
    scan::{self, ScanSpec},
    server::{self, Engine},
};
use rusqlite::params;
use serde_json::{json, Value};
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    os::unix::fs::{symlink, MetadataExt},
    path::{Path, PathBuf},
    thread,
    time::{Duration, SystemTime},
};

struct Fixture {
    _temp: tempfile::TempDir,
    home: PathBuf,
    data: PathBuf,
    db: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let home = root.join("home");
        let data = root.join("data");
        fs::create_dir(&home).unwrap();
        let db = index::initialize(&data).unwrap();
        Self {
            _temp: temp,
            home,
            data,
            db,
        }
    }
    fn put(&self, name: &str, data: impl AsRef<[u8]>) -> PathBuf {
        let path = self.home.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, data).unwrap();
        path
    }
    fn old(&self, name: &str) -> PathBuf {
        let path = self.put(name, b"cache fixture");
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(48 * 3600))
            .unwrap();
        path
    }
    fn scan(&self) -> String {
        let id = id();
        scan::run(ScanSpec {
            id: id.clone(),
            root: self.home.clone(),
            home: self.home.clone(),
            data: self.data.clone(),
            disk: false,
            publish: true,
        })
        .unwrap();
        id
    }
    fn frozen(&self, path: &Path) -> cleanup::Frozen {
        let m = fs::metadata(path).unwrap();
        cleanup::Frozen {
            path: path.into(),
            root: path.into(),
            kind: "cache".into(),
            bytes: (m.blocks() * 512) as i64,
            identity: Identity::of(&m),
            parent: Identity::of(&fs::metadata(path.parent().unwrap()).unwrap()),
        }
    }
    fn resource(&self, path: &Path, scan: &str) -> Resource {
        let m = fs::metadata(path).unwrap();
        Resource {
            path: path.into(),
            root: self.home.clone(),
            scan: scan.into(),
            identity: Identity::of(&m),
            directory: m.is_dir(),
        }
    }
}

#[test]
fn complete_scan_keeps_deep_projects_and_excludes_dependency_projects() {
    let f = Fixture::new();
    f.put("Alpha/package.json", br#"{"name":"alpha"}"#);
    let mut deep = String::from("Alpha");
    for _ in 0..32 {
        deep.push_str("/nested");
    }
    f.put(&(deep + "/Cargo.toml"), b"[package]\nname='deep'");
    f.put("Alpha/node_modules/example/package.json", b"{}");
    f.put("Alpha/fixtures/example/package.json", b"{}");
    for n in 0..1405 {
        f.put(&format!("Alpha/files/{n}.txt"), b"");
    }
    let scan = f.scan();
    let db = index::connect(&f.db).unwrap();
    let info = index::scan_info(&db, &scan).unwrap();
    assert!(info["stats"]["files"].as_u64().unwrap() > 1405);
    let projects = files::projects(&db, &scan).unwrap();
    assert_eq!(projects["rows"].as_array().unwrap().len(), 2);
    assert!(projects["rows"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["path"].as_str().unwrap().contains("nested")));
}
#[test]
fn links_are_not_followed_and_folder_footprints_are_not_zeroed_by_siblings() {
    let f = Fixture::new();
    let file = f.put("left/item.bin", vec![7; 8192]);
    fs::create_dir(f.home.join("right")).unwrap();
    fs::hard_link(&file, f.home.join("right/item.bin")).unwrap();
    symlink(f.home.join("left"), f.home.join("shortcut")).unwrap();
    let allocation = fs::metadata(&file).unwrap().blocks() * 512;
    let scan = f.scan();
    let db = index::connect(&f.db).unwrap();
    let overview = scan::overview(&db, &scan).unwrap();
    assert_eq!(overview["scan"]["stats"]["bytes"], allocation);
    assert_eq!(overview["scan"]["stats"]["links"], 1);
    for name in ["left", "right"] {
        assert_eq!(
            overview["rows"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["name"] == name)
                .unwrap()["bytes"],
            allocation
        );
    }
    let root: i64 = db
        .query_row(
            "SELECT bytes FROM entries WHERE scan=? AND path=?",
            params![scan, index::path_string(&f.home)],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(root as u64, allocation);
}
#[test]
fn unfamiliar_harness_uses_structure_and_project_session_headers() {
    let f = Fixture::new();
    f.put("Alpha/package.json", b"{}");
    f.put(".brand-new-harness/config.toml", b"enabled=true");
    f.put(".brand-new-harness/memory/MEMORY.md", b"memory");
    f.put(".brand-new-harness/sessions/one.jsonl",json!({"type":"session","cwd":f.home.join("Alpha"),"messages":[{"content":"DO_NOT_PERSIST_TRANSCRIPT"}]}).to_string());
    f.put("Alpha/fixtures/not-an-install/config.toml", b"true");
    f.put("Alpha/fixtures/not-an-install/sessions/test.json", b"{}");
    let scan = f.scan();
    let db = index::connect(&f.db).unwrap();
    let tools = discovery::tools(&db, &scan).unwrap();
    let generic = tools["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "brand-new-harness")
        .unwrap();
    assert_eq!(generic["uncertain"], true);
    assert!(!tools.to_string().contains("not-an-install"));
    let projects = files::projects(&db, &scan).unwrap();
    let alpha = projects["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "Alpha")
        .unwrap();
    assert_eq!(alpha["sessions"], 1);
    assert_eq!(alpha["categories"]["session"]["files"], 1);
    let dump: String = db
        .query_row(
            "SELECT group_concat(title,' ') FROM associations WHERE scan=?",
            [scan],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!dump.contains("DO_NOT_PERSIST_TRANSCRIPT"));
}
#[test]
fn immutable_database_metadata_recovers_a_unique_compressed_session() {
    let f = Fixture::new();
    f.put("Alpha/package.json", b"{}");
    let missing = f.home.join(".local/share/opencode/sessions/old.jsonl");
    let actual = f.put(
        ".local/share/opencode/sessions/archive/old.jsonl.gz",
        b"invalid gzip is only indexed, not executed",
    );
    let dbpath = f.home.join(".local/share/opencode/opencode.db");
    let source = rusqlite::Connection::open(&dbpath).unwrap();
    source
        .execute_batch(
            "CREATE TABLE sessions(directory TEXT,title TEXT,file_path TEXT,messages TEXT)",
        )
        .unwrap();
    source
        .execute(
            "INSERT INTO sessions VALUES(?,?,?,?)",
            params![
                index::path_string(&f.home.join("Alpha")),
                "Fixture session",
                index::path_string(&missing),
                "NEVER_READ_MESSAGES"
            ],
        )
        .unwrap();
    drop(source);
    let before = fs::read(&dbpath).unwrap();
    let scan = f.scan();
    assert_eq!(fs::read(&dbpath).unwrap(), before);
    let db = index::connect(&f.db).unwrap();
    let (path, status, managed): (String, String, bool) = db
        .query_row(
            "SELECT path,status,managed FROM associations WHERE scan=? AND indexed_path=?",
            params![scan, index::path_string(&missing)],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(path, index::path_string(&actual));
    assert_eq!(status, "recovered");
    assert!(managed);
}
#[test]
fn browsing_has_opaque_ids_and_rejects_stale_files_and_foreign_directories() {
    let f = Fixture::new();
    f.put("Alpha/package.json", b"{}");
    f.put("Alpha/src/main.rs", b"fn main() {}");
    let scan = f.scan();
    let db = index::connect(&f.db).unwrap();
    let projects = files::projects(&db, &scan).unwrap();
    let pid = projects["rows"][0]["id"].as_str().unwrap();
    let resources = Resources::default();
    let page = files::list(&db, &resources, &f.home, &scan, &json!({"project":pid})).unwrap();
    let src = page["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "src")
        .unwrap();
    assert!(src["bytes"].as_i64().unwrap() > 0);
    let nested = files::list(
        &db,
        &resources,
        &f.home,
        &scan,
        &json!({"project":pid,"directory":src["id"]}),
    )
    .unwrap();
    let key = nested["rows"][0]["id"].as_str().unwrap();
    assert!(resources.get(key, &scan).is_ok());
    f.put("Alpha/src/main.rs", b"changed source");
    assert!(resources.get(key, &scan).is_err());
    assert!(files::list(
        &db,
        &resources,
        &f.home,
        &scan,
        &json!({"project":pid,"directory":"/etc/passwd"})
    )
    .is_err());
}
#[test]
fn previews_require_selection_freeze_identity_expire_and_cannot_replay() {
    let f = Fixture::new();
    f.put("Alpha/package.json", b"{}");
    f.old("Alpha/.cache/old.bin");
    f.put("Alpha/.cache/new.bin", b"fresh");
    f.old("Alpha/.cache/secret-token.txt");
    let scan = f.scan();
    let mut db = index::connect(&f.db).unwrap();
    let projects = files::projects(&db, &scan).unwrap();
    let project = projects["rows"][0]["id"].clone();
    assert!(cleanup::prepare(&mut db, &scan, &json!({"categories":["cache"]}), &f.home).is_err());
    let plan = cleanup::prepare(
        &mut db,
        &scan,
        &json!({"projects":[project],"categories":["cache"]}),
        &f.home,
    )
    .unwrap();
    assert_eq!(plan["files"], 1);
    assert!(cleanup::claim(
        &mut db,
        &json!({"planId":plan["id"],"ids":["made-up"],"confirmation":"TRASH_REVIEWED_FILES"})
    )
    .is_err());
    let body = json!({"planId":plan["id"],"all":true,"confirmation":"TRASH_REVIEWED_FILES"});
    assert_eq!(cleanup::claim(&mut db, &body).unwrap().len(), 1);
    assert!(cleanup::claim(&mut db, &body).is_err());
    assert!(cleanup::prepare(
        &mut db,
        &scan,
        &json!({"allCaches":true,"categories":["cache"]}),
        &f.home
    )
    .is_err());
}
#[test]
fn personal_previews_need_a_second_acknowledgement() {
    let f = Fixture::new();
    f.put("Alpha/package.json", b"{}");
    f.old("Alpha/MEMORY.md");
    let scan = f.scan();
    let mut db = index::connect(&f.db).unwrap();
    let project = files::projects(&db, &scan).unwrap()["rows"][0]["id"].clone();
    assert!(cleanup::prepare(
        &mut db,
        &scan,
        &json!({"projects":[project],"categories":["memory"]}),
        &f.home
    )
    .is_err());
    let plan = cleanup::prepare(
        &mut db,
        &scan,
        &json!({"projects":[project],"categories":["memory"],"personal":true}),
        &f.home,
    )
    .unwrap();
    let mut body = json!({"planId":plan["id"],"all":true,"confirmation":"TRASH_REVIEWED_FILES"});
    assert!(cleanup::claim(&mut db, &body).is_err());
    body["personalConfirmation"] = json!("ACKNOWLEDGE_HISTORY_LOSS");
    assert!(cleanup::claim(&mut db, &body).is_ok());
}
#[test]
fn expired_previews_and_restart_authorization_are_rejected() {
    let f = Fixture::new();
    f.old("Library/Caches/fixture/old.bin");
    let scan = f.scan();
    let mut db = index::connect(&f.db).unwrap();
    let plan = cleanup::prepare(
        &mut db,
        &scan,
        &json!({"allCaches":true,"categories":["cache"]}),
        &f.home,
    )
    .unwrap();
    db.execute("UPDATE plans SET expires=0", []).unwrap();
    assert!(cleanup::claim(
        &mut db,
        &json!({"planId":plan["id"],"all":true,"confirmation":"TRASH_REVIEWED_FILES"})
    )
    .is_err());
    drop(db);
    index::initialize(&f.data).unwrap();
    let db = index::connect(&f.db).unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM plans", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert!(index::latest(&db).unwrap().is_some());
}
#[test]
fn cleanup_moves_only_reviewed_files_and_records_exact_destinations() {
    let f = Fixture::new();
    let reviewed = f.old("Alpha/.cache/reviewed.bin");
    let unselected = f.old("Alpha/.cache/unselected.bin");
    let frozen = f.frozen(&reviewed);
    let db = index::connect(&f.db).unwrap();
    let (_, target) = cleanup::move_one(&db, "fixture-job", &frozen, &f.home, |_| Ok(())).unwrap();
    assert!(!reviewed.exists());
    assert!(unselected.exists());
    assert!(target.unwrap().is_file());
    let receipt = cleanup::receipt(&db, "fixture-job", 0).unwrap();
    assert_eq!(receipt["rows"][1]["status"], "moved");
    assert_eq!(receipt["rows"][1]["path"], index::path_string(&reviewed));
}
#[test]
fn changed_files_symlinks_hardlinks_and_failed_guards_are_preserved() {
    let f = Fixture::new();
    let file = f.old("Alpha/.cache/item.bin");
    let frozen = f.frozen(&file);
    let db = index::connect(&f.db).unwrap();
    fs::write(&file, b"changed").unwrap();
    assert!(cleanup::move_one(&db, "a", &frozen, &f.home, |_| Ok(())).is_err());
    assert!(file.exists());
    let file = f.old("Alpha/.cache/link.bin");
    let frozen = f.frozen(&file);
    fs::remove_file(&file).unwrap();
    symlink(f.home.join("outside"), &file).unwrap();
    assert!(cleanup::move_one(&db, "b", &frozen, &f.home, |_| Ok(())).is_err());
    let file = f.old("Alpha/.cache/hard.bin");
    fs::hard_link(&file, f.home.join("copy.bin")).unwrap();
    let frozen = f.frozen(&file);
    assert!(cleanup::move_one(&db, "c", &frozen, &f.home, |_| Ok(())).is_err());
    let file = f.old("Alpha/.cache/guard.bin");
    let frozen = f.frozen(&file);
    assert!(
        cleanup::move_one(&db, "d", &frozen, &f.home, |_| Err(minorminor_core::fail(
            "active app"
        )))
        .is_err()
    );
    assert!(file.exists());
}
#[test]
fn summary_scope_is_bounded_redacted_and_does_not_follow_links() {
    let f = Fixture::new();
    let project = f.home.join("Alpha");
    f.put(
        "Alpha/README.md",
        b"Selected document\napi_key=SECRET_SHOULD_NOT_LEAVE\n",
    );
    f.put("Alpha/.env", b"PRIVATE_ENV_CONTENT");
    f.put("Alpha/node_modules/lib/secret.txt", b"DEPENDENCY_CONTENT");
    let sibling = f.put("Sibling.txt", b"UNSELECTED_SIBLING_CONTENT");
    symlink(&sibling, f.home.join("Alpha/sibling-link")).unwrap();
    let prepared = ai::prepare(f.resource(&project, "fixture"), &f.home, 3).unwrap();
    let serialized = prepared.payload.to_string();
    for private in [
        "SECRET_SHOULD_NOT_LEAVE",
        "PRIVATE_ENV_CONTENT",
        "DEPENDENCY_CONTENT",
        "UNSELECTED_SIBLING_CONTENT",
    ] {
        assert!(!serialized.contains(private));
    }
    assert!(serialized.contains("Selected document"));
    assert!(prepared.coverage["filesRead"].as_u64().unwrap() <= 12);
    f.put("Alpha/README.md", b"changed after preparation");
    let config = ai::Config::default();
    assert!(ai::summary(prepared, &config).is_err());
}
#[test]
fn credentials_databases_and_binary_files_have_no_summary_body() {
    let f = Fixture::new();
    let secret = f.put("credentials.json", b"SECRET_FILE");
    let prepared = ai::prepare(f.resource(&secret, "fixture"), &f.home, 0).unwrap();
    assert_eq!(prepared.coverage["filesRead"], 0);
    let binary = f.put("sample.bin", [0, 1, 2, 3]);
    let prepared = ai::prepare(f.resource(&binary, "fixture"), &f.home, 0).unwrap();
    assert_eq!(prepared.coverage["filesRead"], 0);
    let config=ai::Config::validate(&json!({"provider":"compatible","baseUrl":"https://example.com/v1","model":"test","apiKey":"secret","analysisDocumentRead":true}),&ai::Config::default()).unwrap();
    assert_eq!(
        config.public(0)["permissions"]["analysisDocumentRead"],
        false
    );
    assert!(!config.public(0).to_string().contains("\"apiKey\""));
    for url in [
        "http://example.com/v1",
        "https://user:pass@example.com/v1",
        "https://example.com/v1?token=secret",
    ] {
        assert!(
            ai::Config::validate(&json!({"provider":"compatible","baseUrl":url}), &config).is_err()
        );
    }
}
fn model(reply: Value) -> (ai::Config, thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut received = Vec::new();
        let mut buffer = [0; 8192];
        loop {
            let n = stream.read(&mut buffer).unwrap();
            if n == 0 {
                break;
            }
            received.extend_from_slice(&buffer[..n]);
            let text = String::from_utf8_lossy(&received);
            if let Some(end) = text.find("\r\n\r\n") {
                let size = text[..end]
                    .lines()
                    .find_map(|line| {
                        line.to_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|s| s.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if received.len() >= end + 4 + size {
                    break;
                }
            }
        }
        let body = json!({"choices":[{"message":reply}]}).to_string();
        write!(stream,"HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{}",body.len(),body).unwrap();
        String::from_utf8(received).unwrap()
    });
    (
        ai::Config {
            provider: "compatible".into(),
            base_url: format!("http://{address}"),
            model: "fixture".into(),
            api_key: String::new(),
        },
        handle,
    )
}
#[test]
fn model_tool_calls_and_additional_read_requests_are_denied() {
    let (config, handle) =
        model(json!({"content":"{}","tool_calls":[{"function":{"name":"read_file"}}]}));
    assert!(ai::analyze(&config, &json!({"items":[]})).is_err());
    let request = handle.join().unwrap();
    assert!(request.contains("tool_choice"));
    assert!(!request.contains("\"tools\":"));
    let (config, handle) =
        model(json!({"content":json!({"requested_files":["README.md"]}).to_string()}));
    assert!(ai::analyze(&config, &json!({"items":[]})).is_err());
    handle.join().unwrap();
}
#[test]
fn summary_sends_only_confirmed_excerpt_to_the_selected_model() {
    let f = Fixture::new();
    let file = f.put("chosen.txt", b"Only this selected content");
    f.put("sibling.txt", b"DO_NOT_SEND_SIBLING");
    let prepared = ai::prepare(f.resource(&file, "fixture"), &f.home, 0).unwrap();
    let (config, handle) = model(
        json!({"content":json!({"summary":"fixture summary","points":[],"limitations":"sample"}).to_string()}),
    );
    let result = ai::summary(prepared, &config).unwrap();
    assert_eq!(result["summary"], "fixture summary");
    let request = handle.join().unwrap();
    assert!(request.contains("Only this selected content"));
    assert!(!request.contains("DO_NOT_SEND_SIBLING"));
}
#[tokio::test]
async fn http_rejects_host_origin_missing_tokens_and_arbitrary_files() {
    let f = Fixture::new();
    let engine = Engine::new(
        f.home.clone(),
        f.data.clone(),
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .into(),
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let service = tokio::spawn(server::serve(engine.clone(), listener));
    tokio::time::sleep(Duration::from_millis(20)).await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let url = format!("http://{address}");
    assert_eq!(
        client
            .get(format!("{url}/api/bootstrap"))
            .header("host", "malicious.example")
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        client
            .post(format!("{url}/api/scan"))
            .json(&json!({"scope":"home"}))
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        client
            .post(format!("{url}/api/scan"))
            .header("origin", "https://malicious.example")
            .header("x-minorminor-token", &engine.token)
            .json(&json!({"scope":"home"}))
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        client
            .get(format!("{url}/server.mjs"))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    let boot: Value = client
        .get(format!("{url}/api/bootstrap"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(boot["native"], true);
    assert_eq!(boot["config"]["permissions"]["analysisDocumentRead"], false);
    service.abort();
}

#[test]
fn rescans_keep_the_last_complete_snapshot_and_reclaim_older_indexes() {
    let f = Fixture::new();
    f.put("Alpha/package.json", b"{}");
    let first = f.scan();
    let second = f.scan();
    let third = f.scan();
    let db = index::connect(&f.db).unwrap();
    assert_eq!(index::latest(&db).unwrap().as_deref(), Some(third.as_str()));
    assert_eq!(
        db.query_row("SELECT count(*) FROM scans", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM entries WHERE scan=?", [first], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap(),
        0
    );
    assert!(index::scan_info(&db, &second).is_ok());
}
#[test]
fn structured_credentials_and_database_sidecars_are_protected() {
    let redacted = files::redact(
        r#"{"api_key":"SECRET_VALUE","authorization":"Bearer TOKEN_VALUE"} https://user:PASS_VALUE@example.com"#,
    );
    for secret in ["SECRET_VALUE", "TOKEN_VALUE", "PASS_VALUE"] {
        assert!(!redacted.contains(secret));
    }
    for suffix in ["sqlite3", "sqlite3-wal", "sqlite3-shm", "db-wal"] {
        assert!(scan::protected(Path::new(&format!(
            "/home/Library/Caches/app/state.{suffix}"
        ))));
    }
}
#[test]
fn native_helpers_cannot_leave_output_pipes_open_after_the_parent_exits() {
    let mut command = std::process::Command::new("/bin/sh");
    command.args(["-c", "sleep 20 & printf done"]);
    let started = std::time::Instant::now();
    let output =
        minorminor_core::mole::command_output(command, None, Duration::from_secs(2), 1024).unwrap();
    assert_eq!(output, b"done");
    assert!(started.elapsed() < Duration::from_secs(3));
}
#[test]
fn exported_receipts_include_every_page() {
    let f = Fixture::new();
    f.put("Alpha/package.json", b"{}");
    f.scan();
    let db = index::connect(&f.db).unwrap();
    for n in 0..205 {
        db.execute("INSERT INTO receipts(job,created,path,status,bytes,reason) VALUES('export-test','today',?,'skipped',0,'fixture')",[format!("file-{n}")]).unwrap();
    }
    drop(db);
    let engine = Engine::new(f.home.clone(), f.data.clone(), PathBuf::new()).unwrap();
    let result = engine
        .dispatch("export", json!({"kind":"receipts","jobId":"export-test"}))
        .unwrap();
    let exported: Value =
        serde_json::from_slice(&fs::read(result["path"].as_str().unwrap()).unwrap()).unwrap();
    assert_eq!(exported["rows"].as_array().unwrap().len(), 205);
}
