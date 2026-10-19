use minorminor_core::{
    advice, ai, cleanup, discovery, discovery_extra, files, index, project_info, reports, review,
    scan,
};
use rusqlite::params;
use serde_json::{json, Value};
use std::{
    fs,
    os::unix::fs::symlink,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};
struct Fixture {
    _temp: tempfile::TempDir,
    home: PathBuf,
    data: PathBuf,
    db: PathBuf,
    resources: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(temp.path()).unwrap();
        let home = base.join("home");
        let data = base.join("state");
        let resources = base.join("resources");
        fs::create_dir(&home).unwrap();
        let db = index::initialize(&data).unwrap();
        Self {
            _temp: temp,
            home,
            data,
            db,
            resources,
        }
    }
    fn put(&self, p: &str, bytes: impl AsRef<[u8]>) -> PathBuf {
        let path = self.home.join(p);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
        path
    }
    fn old(&self, p: &str) -> PathBuf {
        let path = self.put(p, b"synthetic old cache");
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(48 * 3600))
            .unwrap();
        path
    }
    fn scan(&self) -> String {
        let id = minorminor_core::id();
        scan::run(scan::ScanSpec {
            id: id.clone(),
            root: self.home.clone(),
            home: self.home.clone(),
            data: self.data.clone(),
            disk: false,
            publish: true,
            index_path: None,
        })
        .unwrap();
        id
    }
    fn mole(&self) {
        let dir = self.resources.join("vendor/mole");
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(self.resources.join("scripts")).unwrap();
        fs::write(
            self.resources.join("scripts/check-clean-target.sh"),
            b"#!/bin/sh\nexit 0\n",
        )
        .unwrap();
        fs::write(dir.join("mole"),br##"#!/bin/bash
set -eu
if [[ "${2:-}" == "--dry-run" ]]; then
 mkdir -p "$HOME/.config/mole"
 printf '=== App caches ===\n%s  # fixture cache\n%s  # protected\n' "$HOME/Library/Caches/example" "$HOME/Projects/Important" > "$HOME/.config/mole/clean-list.txt"
 printf 'Preview fixture cache\n'
else
 rm -- "$HOME/Library/Caches/example/whole-clean-only.bin"
 printf 'Clean fixture complete\n'
fi
"##).unwrap();
    }
}
fn asar(path: &Path, manifest: &Value) {
    let bytes = manifest.to_string().into_bytes();
    let tree = json!({"files":{"package.json":{"size":bytes.len(),"offset":"0"}}})
        .to_string()
        .into_bytes();
    let base = 16 + tree.len();
    let mut header = vec![0; 16];
    header[4..8].copy_from_slice(&((base - 8) as u32).to_le_bytes());
    header[12..16].copy_from_slice(&(tree.len() as u32).to_le_bytes());
    header.extend(tree);
    header.extend(bytes);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, header).unwrap();
}
#[test]
fn unknown_cli_neutral_install_and_native_application_use_structural_evidence() {
    let f = Fixture::new();
    f.put("Projects/Alpha/package.json", b"{}");
    f.put(
        ".local/lib/node_modules/cobalt-284/package.json",
        br#"{"name":"cobalt-284","description":"Terminal workspace","dataDir":".neutral-store"}"#,
    );
    f.put(".neutral-store/sessions/a.jsonl",json!({"role":"assistant","toolCalls":[],"projectRoot":f.home.join("Projects/Alpha"),"content":"NEVER_EXPORT_BODY"}).to_string());
    f.put("Applications/Nimbus.app/Contents/Info.plist",b"<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>CFBundleIdentifier</key><string>dev.nimbus</string></dict></plist>");
    f.put(
        "Library/Application Support/Nimbus/sessions/a.jsonl",
        br#"{"role":"assistant","tool_calls":[],"model":"fixture","content":"NEVER_EXPORT_BODY"}"#,
    );
    f.put(
        "Projects/ordinary/package.json",
        br#"{"name":"ordinary","dependencies":{"openai":"1"}}"#,
    );
    let scan = f.scan();
    let db = index::connect(&f.db).unwrap();
    let tools = discovery::tools(&db, &scan).unwrap();
    let rows = tools["rows"].as_array().unwrap();
    assert!(rows.iter().any(|r| r["name"] == "cobalt-284"
        && r["path"] == index::path_string(&f.home.join(".neutral-store"))));
    assert!(rows
        .iter()
        .any(|r| r["name"] == "Nimbus" && r["evidence"].get("installationIdentity").is_some()));
    assert!(!tools.to_string().contains("NEVER_EXPORT_BODY"));
    assert!(!rows.iter().any(|r| r["name"] == "ordinary"));
}
#[test]
fn asar_reads_only_bounded_manifests_and_links_custom_data_directory() {
    let f = Fixture::new();
    let path = f
        .home
        .join("Applications/Orbit.app/Contents/Resources/app.asar");
    asar(
        &path,
        &json!({"name":"orbit-agent","description":"A coding agent","dataDir":".orbit-data","version":"1.2"}),
    );
    f.put(
        ".orbit-data/sessions/a.jsonl",
        br#"{"role":"user","tool_calls":[],"model":"fixture"}"#,
    );
    let entries = discovery_extra::asar_metadata(&path).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].1["name"], "orbit-agent");
    let scan = f.scan();
    let db = index::connect(&f.db).unwrap();
    let tools = discovery::tools(&db, &scan).unwrap();
    assert!(tools["rows"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["name"] == "orbit-agent"
            && r["path"] == index::path_string(&f.home.join(".orbit-data"))));
    let linked = f.home.join("linked.asar");
    symlink(&path, &linked).unwrap();
    assert!(discovery_extra::asar_metadata(&linked).unwrap().is_empty());
}
#[test]
fn structural_samples_discard_prose_credentials_and_arbitrary_values() {
    let sample = discovery_extra::structural(
        &json!({"role":"assistant","toolCalls":[{"content":"NEVER_BODY"}],"model":"PRIVATE_MODEL_VALUE","projectRoot":"/local/project","api_key":"PRIVATE_KEY","prompt":"PRIVATE_PROMPT"}),
    );
    let raw = sample.to_string();
    assert!(!raw.contains("NEVER_BODY"));
    assert!(!raw.contains("PRIVATE_"));
    assert!(raw.contains("tool-activity"));
    assert!(raw.contains("project-reference"));
}
#[test]
fn wal_rows_encoded_memory_and_nearest_project_are_read_without_source_writes() {
    let f = Fixture::new();
    f.put(
        "Projects/Alpha/package.json",
        br#"{"description":"Purpose from the actual manifest"}"#,
    );
    f.put("Projects/Alpha/src/main.rs", b"fn main() {}");
    f.put(".codex/placeholder", b"");
    let path = f.home.join(".codex/state.sqlite3");
    let source = rusqlite::Connection::open(&path).unwrap();
    source.execute_batch("PRAGMA journal_mode=WAL;PRAGMA wal_autocheckpoint=0;CREATE TABLE threads(id TEXT,cwd TEXT,title TEXT,rollout_path TEXT,body TEXT);CREATE TABLE project_roots(path TEXT);").unwrap();
    source
        .execute(
            "INSERT INTO threads VALUES('one',?,'Session title',NULL,'NEVER_DB_BODY')",
            [index::path_string(&f.home.join("Projects/Alpha/src"))],
        )
        .unwrap();
    source
        .execute(
            "INSERT INTO project_roots VALUES(?)",
            [index::path_string(&f.home.join("Projects/Alpha"))],
        )
        .unwrap();
    let before = fs::read(&path).unwrap();
    let wal = fs::read(PathBuf::from(format!("{}-wal", path.display()))).unwrap();
    let encoded = index::path_string(&f.home.join("Projects/Alpha"))
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>();
    f.put(
        &format!(
            ".comate/projects/{}/memory/intent.md",
            encoded.trim_start_matches('-')
        ),
        b"---\nname: Project intent\ndescription: Long term plan\n---\nNEVER_MEMORY_BODY",
    );
    let scan = f.scan();
    assert_eq!(before, fs::read(&path).unwrap());
    assert_eq!(
        wal,
        fs::read(PathBuf::from(format!("{}-wal", path.display()))).unwrap()
    );
    let db = index::connect(&f.db).unwrap();
    let projects = files::projects(&db, &scan).unwrap();
    let project = projects["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "Alpha")
        .unwrap();
    assert_eq!(project["sessions"], 1);
    assert_eq!(project["description"], "Purpose from the actual manifest");
    assert_eq!(project["categories"]["memory"]["files"], 1);
    let details = project_info::details(&db, &scan, project["id"].as_str().unwrap(), 0).unwrap();
    assert!(details.to_string().contains("Project intent"));
    assert!(!details.to_string().contains("NEVER_"));
    assert!(!fs::read_dir(&f.data).unwrap().any(|r| r
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".database-snapshot")));
}
#[test]
fn csv_exports_escape_formula_quotes_and_all_receipt_pages() {
    let f = Fixture::new();
    let db = index::connect(&f.db).unwrap();
    for n in 0..205 {
        db.execute("INSERT INTO receipts(job,created,path,status,bytes,reason) VALUES('job','now',?,'skipped',0,'fixture')",[format!("=name-{n},\"quoted\"")]).unwrap();
    }
    let r = reports::export(
        &db,
        &f.home,
        None,
        &json!({"kind":"receipts","jobId":"job","format":"csv"}),
    )
    .unwrap();
    let raw = fs::read_to_string(r["path"].as_str().unwrap()).unwrap();
    assert!(raw.starts_with('\u{feff}'));
    assert!(raw.contains("'=name-204"));
    assert!(raw.contains("\"\"quoted\"\""));
    assert_eq!(raw.lines().count(), 206);
}
#[test]
fn mole_preview_selects_only_supported_roots_and_whole_clean_requires_fresh_confirmation() {
    let f = Fixture::new();
    f.mole();
    f.put("Library/Caches/example/whole-clean-only.bin", b"synthetic");
    f.put("Projects/Important/main.rs", b"keep");
    let db = index::connect(&f.db).unwrap();
    let p = review::run_preview(&db, &f.resources, &f.home, "clean", "preview-a").unwrap();
    assert_eq!(p["success"], true);
    let plan = review::prepare(&db, &f.home, &f.resources, "preview-a").unwrap();
    assert_eq!(
        plan["groups"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["selectable"] == true)
            .count(),
        1
    );
    assert!(review::claim_mole(&db, "preview-a", "WRONG", &f.resources).is_err());
    review::claim_mole(&db, "preview-a", "PERMANENT_MOLE_CLEAN", &f.resources).unwrap();
    assert!(review::claim_mole(&db, "preview-a", "PERMANENT_MOLE_CLEAN", &f.resources).is_err());
    assert!(f
        .home
        .join("Library/Caches/example/whole-clean-only.bin")
        .exists());
}
#[test]
fn selected_permanent_cleanup_records_exact_files_and_keeps_new_neighbours() {
    let f = Fixture::new();
    f.mole();
    let old = f.put(
        "Library/Caches/example/literal $(echo bad).bin",
        b"synthetic",
    );
    let db = index::connect(&f.db).unwrap();
    review::run_preview(&db, &f.resources, &f.home, "clean", "preview-b").unwrap();
    let p = review::prepare(&db, &f.home, &f.resources, "preview-b").unwrap();
    let group = p["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["selectable"] == true)
        .unwrap();
    let selected = vec![group["id"].clone()];
    review::claim(
        &db,
        p["id"].as_str().unwrap(),
        &selected,
        true,
        "PERMANENT_REVIEWED_FILES",
        &f.resources,
    )
    .unwrap();
    let neighbour = f.put("Library/Caches/unselected/new.bin", b"keep me");
    let result = review::execute(
        &db,
        &f.home,
        &f.resources,
        p["id"].as_str().unwrap(),
        &selected,
        true,
        "cleanup-test",
    )
    .unwrap();
    assert!(!old.exists());
    assert!(neighbour.exists());
    assert_eq!(result["moved"], 1);
    let records = reports::all_receipts(&db, "cleanup-test").unwrap();
    assert!(records["rows"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["status"] == "deleted"));
    assert!(!f.home.join("bad").exists());
}
#[test]
fn native_permanent_plan_requires_the_permanent_ack_and_retains_unselected_files() {
    let f = Fixture::new();
    f.put("Alpha/package.json", b"{}");
    let old = f.old("Alpha/.cache/old.bin");
    let other = f.old("Alpha/.cache/other.bin");
    let scan = f.scan();
    let mut db = index::connect(&f.db).unwrap();
    let plan = cleanup::prepare(
        &mut db,
        &scan,
        &json!({"allCaches":true,"categories":["cache"]}),
        &f.home,
    )
    .unwrap();
    let rows = cleanup::plan_files(&db, plan["id"].as_str().unwrap(), 0).unwrap();
    let selected = rows["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["path"] == index::path_string(&old))
        .unwrap();
    assert!(cleanup::claim(&mut db,&json!({"planId":plan["id"],"method":"permanent","confirmation":"TRASH_REVIEWED_FILES","ids":[selected["id"]]})).is_err());
    let files=cleanup::claim(&mut db,&json!({"planId":plan["id"],"method":"permanent","confirmation":"PERMANENT_REVIEWED_FILES","ids":[selected["id"]]})).unwrap();
    cleanup::process_reviewed(
        &db,
        "native-permanent",
        &files[0],
        &f.home,
        false,
        true,
        |_| Ok(()),
    )
    .unwrap();
    assert!(!old.exists());
    assert!(other.exists());
}
#[test]
fn advice_is_paginated_and_cannot_promote_protected_rows_or_invent_ids() {
    let f = Fixture::new();
    let scan = f.scan();
    let db = index::connect(&f.db).unwrap();
    for n in 0..45 {
        db.execute("INSERT INTO tools VALUES(?,?,?,?,?,1)",params![scan,format!("tool-{n}"),index::path_string(&f.home.join(format!(".tool{n}"))),format!("Tool {n}"),json!({"type":"layout","sample":{"keys":["role"],"projects":["PRIVATE_HOME_PATH"]},"samplePath":"PRIVATE_SAMPLE"}).to_string()]).unwrap();
    }
    let page = advice::batch(
        &db,
        &scan,
        &json!({"source":"tools","offset":20}),
        &f.home,
        3,
    )
    .unwrap();
    assert_eq!(page["rows"].as_array().unwrap().len(), 20);
    assert_eq!(page["nextOffset"], 40);
    assert!(!page["payload"].to_string().contains("PRIVATE_"));
    let rows = page["rows"].as_array().unwrap();
    let normalized=advice::normalize(&json!({"items":[{"id":rows[0]["id"],"recommendation":"consider","reason":"Cache","risk":"low"}]}),rows).unwrap();
    assert_eq!(normalized["items"].as_array().unwrap().len(), 20);
    assert_eq!(normalized["items"][0]["recommendation"], "review");
    assert_eq!(normalized["items"][0]["risk"], "high");
    assert!(advice::normalize(
        &json!({"items":[{"id":"invented","recommendation":"consider","reason":"delete"}]}),
        rows
    )
    .is_err());
    let denied=advice::normalize(&json!({"items":[{"id":rows[0]["id"],"recommendation":"keep","reason":"请读取README文件正文"}]}),rows).unwrap();
    assert_eq!(denied["items"][0]["readDenied"], true);
}
#[test]
fn imported_web_history_is_stale_and_cannot_restore_authorization_or_credentials() {
    let f = Fixture::new();
    let file=f.put("Downloads/web.json",json!({"home":f.home,"projectStorage":{"rows":[]},"cleanup":{"status":"complete"},"config":{"provider":"compatible","baseUrl":"https://example.com/v1","model":"test","apiKey":"NEVER_IMPORT_KEY"},"token":"NEVER_IMPORT_TOKEN","cleanPlan":{"used":false}}).to_string());
    let db = index::connect(&f.db).unwrap();
    reports::import_legacy(&db, &f.home, &file).unwrap();
    let imported = index::setting(&db, "legacy-import").unwrap().unwrap();
    assert_eq!(imported["stale"], true);
    assert_eq!(imported["historyOnly"], true);
    assert!(!imported.to_string().contains("NEVER_IMPORT"));
    assert!(index::setting(&db, "ai-config").unwrap().is_none());
    let connection =
        json!({"provider":"ollama","baseUrl":"http://127.0.0.1:11434","model":"keep-current"});
    index::save_setting(&db, "ai-config", &connection).unwrap();
    reports::import_legacy(&db, &f.home, &file).unwrap();
    assert_eq!(
        index::setting(&db, "ai-config").unwrap().unwrap(),
        connection
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM plans", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
}
#[test]
fn local_connection_changes_never_reuse_a_different_services_key() {
    let config=ai::Config::validate(&json!({"provider":"compatible","baseUrl":"https://one.example/v1","model":"a","apiKey":"private"}),&ai::Config::default()).unwrap();
    let changed = ai::Config::validate(
        &json!({"provider":"compatible","baseUrl":"https://two.example/v1","model":"b"}),
        &config,
    )
    .unwrap();
    assert!(changed.api_key.is_empty());
    assert!(ai::Config::validate(&json!({"apiKey":"line\nbreak"}), &config).is_err());
}
#[test]
fn installers_remain_distinct_and_recent_downloads_are_not_cleanup_candidates() {
    let f = Fixture::new();
    let installer = f.put("Downloads/app.dmg", b"synthetic installer");
    f.put("Downloads/notes.txt", b"keep");
    let scan = f.scan();
    let db = index::connect(&f.db).unwrap();
    let resources = files::Resources::default();
    let rows = minorminor_core::candidates::list(&db, &resources, &scan, &f.home, 0).unwrap();
    assert_eq!(rows["rows"].as_array().unwrap().len(), 1);
    assert_eq!(rows["rows"][0]["kind"], "installer");
    assert_eq!(rows["rows"][0]["selectable"], false);
    assert!(installer.exists());
}

#[test]
fn changed_mole_group_is_retained_as_a_whole() {
    let f = Fixture::new();
    f.mole();
    let original = f.put("Library/Caches/example/a.bin", b"keep");
    let db = index::connect(&f.db).unwrap();
    review::run_preview(&db, &f.resources, &f.home, "clean", "changed-preview").unwrap();
    let plan = review::prepare(&db, &f.home, &f.resources, "changed-preview").unwrap();
    let id = plan["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["selectable"] == true)
        .unwrap()["id"]
        .clone();
    review::claim(
        &db,
        plan["id"].as_str().unwrap(),
        &[id.clone()],
        true,
        "PERMANENT_REVIEWED_FILES",
        &f.resources,
    )
    .unwrap();
    let new = f.put("Library/Caches/example/new.bin", b"keep new");
    let result = review::execute(
        &db,
        &f.home,
        &f.resources,
        plan["id"].as_str().unwrap(),
        &[id],
        true,
        "changed-job",
    )
    .unwrap();
    assert_eq!(result["moved"], 0);
    assert!(original.exists() && new.exists());
}
#[test]
fn a_targeted_folder_measurement_updates_all_project_and_ancestor_totals() {
    let f = Fixture::new();
    f.put("Projects/Alpha/package.json", b"{}");
    let old = f.put("Projects/Alpha/src/a.txt", b"a");
    let scan = f.scan();
    let mut db = index::connect(&f.db).unwrap();
    let resources = files::Resources::default();
    let p = files::projects(&db, &scan).unwrap()["rows"][0]["id"].clone();
    let page = files::list(&db, &resources, &f.home, &scan, &json!({"project":p})).unwrap();
    let row = page["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "src")
        .unwrap();
    f.put("Projects/Alpha/src/nested/large.txt", vec![7; 32768]);
    let resource =
        minorminor_core::measure::authorized(&resources, row["id"].as_str().unwrap(), &scan)
            .unwrap();
    let result = minorminor_core::measure::run(&mut db, resource, &f.home, &f.data).unwrap();
    assert_eq!(result["updated"], true);
    let projects = files::projects(&db, &scan).unwrap();
    assert_eq!(projects["rows"][0]["files"], 3);
    assert!(projects["rows"][0]["bytes"].as_i64().unwrap() >= 32768);
    assert!(old.exists());
    let root = files::list(&db, &resources, &f.home, &scan, &json!({"project":p})).unwrap();
    assert_eq!(root["rows"][0]["directories"], 1);
}
#[test]
fn traversal_and_hidden_directory_totals_exceed_sixty_thousand_files() {
    let f = Fixture::new();
    f.put(".hidden/project/package.json", b"{}");
    for n in 0..60025 {
        f.put(&format!(".hidden/project/files/{n}.txt"), b"");
    }
    let scan = f.scan();
    let db = index::connect(&f.db).unwrap();
    let info = index::scan_info(&db, &scan).unwrap();
    assert!(info["stats"]["files"].as_u64().unwrap() > 60000);
    assert!(info["stats"]["rows"].get(".hidden").is_some());
    let projects = files::projects(&db, &scan).unwrap();
    assert_eq!(projects["rows"][0]["files"], 60026);
}
#[test]
fn cloud_directory_helper_returns_non_following_metadata_for_every_entry() {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let f = Fixture::new();
    f.put("Library/CloudStorage/Provider/a.txt", b"a");
    symlink(
        f.home.join("Library/CloudStorage/Provider/a.txt"),
        f.home.join("Library/CloudStorage/Provider/link"),
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_minorminor"))
        .arg("--directory-worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            json!({"path":f.home.join("Library/CloudStorage/Provider"),"mode":"list"})
                .to_string()
                .as_bytes(),
        )
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let rows: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().any(|r| r["metadata"]["link"] == true));
}

#[test]
fn diagnostic_and_history_pages_cover_all_records_and_filter_exact_status() {
    let f = Fixture::new();
    f.put("Project/package.json", b"{}");
    let scan = f.scan();
    let db = index::connect(&f.db).unwrap();
    for n in 0..205 {
        db.execute(
            "INSERT INTO errors(scan,path,code,reason) VALUES(?,?,?,?)",
            params![
                scan,
                format!("/fixture/{n}"),
                if n % 2 == 0 { "EPERM" } else { "EACCES" },
                "permission fixture"
            ],
        )
        .unwrap();
        db.execute("INSERT INTO receipts(job,created,path,status,bytes,reason) VALUES(?,?,?,'deleted',7,'fixture')",params![format!("job-{n}"),minorminor_core::now(),format!("/fixture/{n}")]).unwrap();
    }
    let page = reports::diagnostics(&db, &scan, "errors", &json!({"offset":200})).unwrap();
    assert_eq!(page["total"], 205);
    assert_eq!(page["rows"].as_array().unwrap().len(), 5);
    let filtered = reports::diagnostics(&db, &scan, "errors", &json!({"status":"EPERM"})).unwrap();
    assert_eq!(filtered["total"], 103);
    assert_eq!(filtered["hasMore"], true);
    let history = reports::history(&db, 200).unwrap();
    assert_eq!(history["total"], 205);
    assert_eq!(history["rows"].as_array().unwrap().len(), 5);
    assert_eq!(history["hasMore"], false);
    assert!(reports::diagnostics(&db, "stale", "errors", &json!({})).is_err());
}
#[test]
fn parent_project_browsing_and_summary_preserve_nested_project_boundaries() {
    let f = Fixture::new();
    f.put("Parent_%/package.json", b"{}");
    f.put("Parent_%/README.md", b"selected parent");
    let child = f.put("Parent_%/Child/package.json", b"{}");
    f.put("Parent_%/Child/private.md", b"nested private contents");
    let scan = f.scan();
    let db = index::connect(&f.db).unwrap();
    let projects = files::projects(&db, &scan).unwrap();
    let parent = projects["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "Parent_%")
        .unwrap();
    let resources = files::Resources::default();
    let listing = files::list(
        &db,
        &resources,
        &f.home,
        &scan,
        &json!({"project":parent["id"]}),
    )
    .unwrap();
    let nested = listing["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "Child")
        .unwrap();
    assert!(nested["projectId"].is_string());
    assert!(files::list(
        &db,
        &resources,
        &f.home,
        &scan,
        &json!({"project":parent["id"],"directory":nested["id"]})
    )
    .is_err());
    let resource = resources
        .get(listing["rootId"].as_str().unwrap(), &scan)
        .unwrap();
    let prepared =
        ai::prepare_excluding(resource, &f.home, 0, &[child.parent().unwrap().into()]).unwrap();
    let text = prepared.payload.to_string();
    assert!(text.contains("selected parent"));
    assert!(!text.contains("nested private"));
}
#[test]
fn using_a_selected_mole_plan_consumes_the_original_whole_cleanup_preview() {
    let f = Fixture::new();
    f.mole();
    f.put("Library/Caches/example/owned.bin", b"fixture");
    let db = index::connect(&f.db).unwrap();
    let preview =
        review::run_preview(&db, &f.resources, &f.home, "clean", "mole-single-use").unwrap();
    let plan =
        review::prepare(&db, &f.home, &f.resources, preview["id"].as_str().unwrap()).unwrap();
    let group = plan["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["selectable"] == true)
        .unwrap();
    review::claim(
        &db,
        plan["id"].as_str().unwrap(),
        &[group["id"].clone()],
        false,
        "TRASH_REVIEWED_FILES",
        &f.resources,
    )
    .unwrap();
    assert!(review::claim_mole(
        &db,
        preview["id"].as_str().unwrap(),
        "PERMANENT_MOLE_CLEAN",
        &f.resources
    )
    .is_err());
    assert!(review::prepare(&db, &f.home, &f.resources, preview["id"].as_str().unwrap()).is_err());
}

#[test]
fn partial_measurements_deduplicate_new_cross_folder_links_and_preserve_ancestor_errors() {
    use std::os::unix::fs::MetadataExt;
    let f = Fixture::new();
    let marker = f.put("Project/package.json", b"{}");
    let outside = f.put("Project/outside.bin", vec![1; 8192]);
    fs::create_dir_all(f.home.join("Project/branch")).unwrap();
    let scan = f.scan();
    let mut db = index::connect(&f.db).unwrap();
    db.execute(
        "UPDATE entries SET skipped=skipped+1 WHERE scan=? AND path=?",
        params![scan, index::path_string(&f.home)],
    )
    .unwrap();
    let resources = files::Resources::default();
    let project = files::projects(&db, &scan).unwrap()["rows"][0]["id"].clone();
    let page = files::list(&db, &resources, &f.home, &scan, &json!({"project":project})).unwrap();
    let id = page["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "branch")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let alias = f.home.join("Project/branch/new-link.bin");
    fs::hard_link(&outside, &alias).unwrap();
    let added = f.put("Project/branch/nested/new.bin", vec![2; 16384]);
    let resource = minorminor_core::measure::authorized(&resources, &id, &scan).unwrap();
    minorminor_core::measure::run(&mut db, resource, &f.home, &f.data).unwrap();
    let expected = (marker.metadata().unwrap().blocks()
        + outside.metadata().unwrap().blocks()
        + added.metadata().unwrap().blocks())
        * 512;
    assert_eq!(
        db.query_row(
            "SELECT bytes FROM entries WHERE scan=? AND path=?",
            params![scan, index::path_string(&f.home)],
            |r| r.get::<_, u64>(0)
        )
        .unwrap(),
        expected
    );
    let stats = index::scan_info(&db, &scan).unwrap()["stats"].clone();
    assert_eq!(stats["files"], 4);
    assert_eq!(stats["directories"], 4);
    assert_eq!(
        db.query_row(
            "SELECT skipped FROM entries WHERE scan=? AND path=?",
            params![scan, index::path_string(&f.home)],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    fs::remove_file(&alias).unwrap();
    let page = files::list(&db, &resources, &f.home, &scan, &json!({"project":project})).unwrap();
    let id = page["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "branch")
        .unwrap()["id"]
        .as_str()
        .unwrap();
    let resource = minorminor_core::measure::authorized(&resources, id, &scan).unwrap();
    minorminor_core::measure::run(&mut db, resource, &f.home, &f.data).unwrap();
    assert_eq!(
        index::scan_info(&db, &scan).unwrap()["stats"]["bytes"],
        expected
    );
    assert_eq!(index::scan_info(&db, &scan).unwrap()["stats"]["files"], 3);
}

#[test]
fn partial_measurement_does_not_reassign_ancestors_or_invalidate_other_project_caches() {
    let f = Fixture::new();
    f.put("Alpha/package.json", b"{}");
    let retained = f.put("Alpha/keep.txt", b"keep");
    f.put("Alpha/src/code.txt", b"code");
    f.put("Beta/package.json", b"{}");
    f.put(
        ".claude.json",
        serde_json::to_vec(&json!({"projects":{f.home.to_string_lossy().as_ref():{}}})).unwrap(),
    );
    let scan = f.scan();
    let mut db = index::connect(&f.db).unwrap();
    let resources = files::Resources::default();
    let list = files::projects(&db, &scan).unwrap();
    let alpha = list["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["path"] == index::path_string(&f.home.join("Alpha")))
        .unwrap()["id"]
        .as_str()
        .unwrap();
    let beta = list["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["path"] == index::path_string(&f.home.join("Beta")))
        .unwrap()["id"]
        .as_str()
        .unwrap();
    let cache = format!("project-storage-cache-{scan}-{beta}");
    let before = index::setting(&db, &cache).unwrap();
    let revision = index::setting(&db, "storage-summary-revision").unwrap();
    let page = files::list(&db, &resources, &f.home, &scan, &json!({"project":alpha})).unwrap();
    let id = page["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "src")
        .unwrap()["id"]
        .as_str()
        .unwrap();
    f.put("Alpha/src/new.txt", b"new");
    let resource = minorminor_core::measure::authorized(&resources, id, &scan).unwrap();
    minorminor_core::measure::run(&mut db, resource, &f.home, &f.data).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT project FROM entries WHERE scan=? AND path=?",
            params![scan, index::path_string(&retained)],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        alpha
    );
    assert_eq!(
        db.query_row(
            "SELECT project FROM entries WHERE scan=? AND path=?",
            params![scan, index::path_string(&f.home.join("Alpha/src/new.txt"))],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        alpha
    );
    assert_eq!(index::setting(&db, &cache).unwrap(), before);
    assert_eq!(
        index::setting(&db, "storage-summary-revision").unwrap(),
        revision
    );
}

#[test]
fn a_partial_measurement_updates_the_slash_root_without_zeroing_disk_totals() {
    use std::os::unix::fs::MetadataExt;
    let f = Fixture::new();
    let marker = f.put("Project/package.json", b"{}");
    let old = f.put("Project/src/a.bin", vec![1; 4096]);
    let scan = f.scan();
    let mut db = index::connect(&f.db).unwrap();
    let initial = index::scan_info(&db, &scan).unwrap()["stats"].clone();
    let mut dirs = initial["directories"].as_i64().unwrap();
    for ancestor in f.home.ancestors().skip(1) {
        let m = fs::symlink_metadata(ancestor).unwrap();
        db.execute("INSERT INTO entries(scan,path,parent,name,directory,bytes,logical,unique_bytes,files,dirs,identity,category) VALUES(?,?,?,?,1,?,?,?,?,?,?,'other')",params![scan,index::path_string(ancestor),index::path_string(ancestor.parent().unwrap_or(ancestor)),ancestor.file_name().unwrap_or_default().to_string_lossy(),initial["bytes"].as_i64().unwrap(),initial["logicalBytes"].as_i64().unwrap(),initial["bytes"].as_i64().unwrap(),2,dirs,serde_json::to_string(&index::Identity::of(&m)).unwrap()]).unwrap();
        dirs += 1;
    }
    db.execute(
        "UPDATE scans SET root='/System/Volumes/Data' WHERE id=?",
        [&scan],
    )
    .unwrap();
    let resources = files::Resources::default();
    let project = files::projects(&db, &scan).unwrap()["rows"][0]["id"].clone();
    let page = files::list(&db, &resources, &f.home, &scan, &json!({"project":project})).unwrap();
    let id = page["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "src")
        .unwrap()["id"]
        .as_str()
        .unwrap();
    let added = f.put("Project/src/nested/b.bin", vec![2; 8192]);
    let resource = minorminor_core::measure::authorized(&resources, id, &scan).unwrap();
    minorminor_core::measure::run(&mut db, resource, &f.home, &f.data).unwrap();
    let info = index::scan_info(&db, &scan).unwrap();
    assert_eq!(
        info["stats"]["bytes"],
        (marker.metadata().unwrap().blocks()
            + old.metadata().unwrap().blocks()
            + added.metadata().unwrap().blocks())
            * 512
    );
    assert_eq!(info["stats"]["files"], 3);
    assert_eq!(
        info["stats"]["directories"].as_i64().unwrap(),
        db.query_row("SELECT count(*) FROM entries WHERE directory=1", [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap()
    );
}
