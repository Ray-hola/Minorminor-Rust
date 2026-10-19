use crate::{
    discovery::{self, add, catalog},
    files, index, scan, Result,
};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::{
    collections::{BTreeSet, HashSet},
    fs,
    io::{Read, Seek, SeekFrom},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};
fn excluded(path: &Path) -> bool {
    path.components().any(|c| {
        [
            "node_modules",
            "vendor",
            ".git",
            ".Trash",
            ".runtime",
            "fixtures",
            "backup",
            "backups",
            ".venv",
            "venv",
            "site-packages",
        ]
        .contains(&c.as_os_str().to_string_lossy().as_ref())
    })
}
fn candidate_root(path: &Path, home: &Path) -> Option<PathBuf> {
    let relative = path.strip_prefix(home).ok()?;
    if excluded(relative) {
        return None;
    }
    let text = relative.to_string_lossy();
    for prefix in [
        "Library/Application Support/",
        "Library/Caches/",
        "Library/Logs/",
        ".config/",
        ".local/share/",
        ".local/state/",
        ".cache/",
    ] {
        if let Some(tail) = text.strip_prefix(prefix) {
            return tail
                .split('/')
                .next()
                .filter(|s| !s.is_empty())
                .map(|s| home.join(prefix).join(s));
        }
    }
    for key in [
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
        "XDG_CACHE_HOME",
    ] {
        if let Some(value) = std::env::var_os(key) {
            let base = PathBuf::from(value);
            if base.starts_with(home) {
                if let Ok(tail) = path.strip_prefix(&base) {
                    if let Some(first) = tail.components().next() {
                        return Some(base.join(first.as_os_str()));
                    }
                }
            }
        }
    }
    let first = relative.components().next()?.as_os_str().to_string_lossy();
    if first.starts_with('.') && !first.starts_with(".env") {
        Some(home.join(first.as_ref()))
    } else {
        None
    }
}
pub fn structural(value: &Value) -> Value {
    fn visit(
        value: &Value,
        depth: usize,
        keys: &mut BTreeSet<String>,
        features: &mut BTreeSet<String>,
        projects: &mut BTreeSet<String>,
        visited: &mut usize,
    ) {
        if depth > 5 || *visited > 500 {
            return;
        }
        *visited += 1;
        match value {
            Value::Array(a) => {
                for v in a.iter().take(12) {
                    visit(v, depth + 1, keys, features, projects, visited)
                }
            }
            Value::Object(o) => {
                for (k, v) in o {
                    let mut key = String::new();
                    for c in k.chars() {
                        if c.is_ascii_uppercase() {
                            key.push('_');
                            key.push(c.to_ascii_lowercase())
                        } else {
                            key.push(if c == '-' { '_' } else { c })
                        }
                    }
                    if [
                        "cwd",
                        "workdir",
                        "working_dir",
                        "working_directory",
                        "project_path",
                        "project_root",
                        "workspace_path",
                        "session_id",
                        "conversation_id",
                        "messages",
                        "role",
                        "tool_calls",
                        "tool_call_id",
                        "tool_result",
                        "function_call",
                        "model",
                        "model_id",
                        "provider",
                        "tools",
                        "type",
                    ]
                    .contains(&key.as_str())
                    {
                        keys.insert(key.clone());
                    }
                    if [
                        "cwd",
                        "workdir",
                        "working_dir",
                        "working_directory",
                        "project_path",
                        "project_root",
                        "workspace_path",
                    ]
                    .contains(&key.as_str())
                    {
                        if let Some(s) = v.as_str().filter(|s| {
                            s.starts_with('/') && s.len() < 4096 && !s.chars().any(char::is_control)
                        }) {
                            projects.insert(s.into());
                            features.insert("project-reference".into());
                        }
                    }
                    if key == "role"
                        && v.as_str()
                            .is_some_and(|s| ["assistant", "user", "tool"].contains(&s))
                    {
                        features.insert("conversation-record".into());
                    }
                    if ["tool_calls", "tool_call_id", "function_call", "tool_result"]
                        .contains(&key.as_str())
                    {
                        features.insert("tool-activity".into());
                    }
                    if ["model", "model_id", "provider"].contains(&key.as_str()) {
                        features.insert("model-metadata".into());
                    }
                    if key == "type"
                        && v.as_str().is_some_and(|s| {
                            ["session", "session_meta", "session_start"].contains(&s)
                        })
                    {
                        features.insert("session-header".into());
                    }
                    if ![
                        "content",
                        "text",
                        "prompt",
                        "system",
                        "instructions",
                        "api_key",
                        "token",
                        "password",
                        "secret",
                    ]
                    .contains(&key.as_str())
                    {
                        visit(v, depth + 1, keys, features, projects, visited);
                    }
                }
            }
            _ => {}
        }
    }
    let (mut keys, mut features, mut projects, mut visited) =
        (BTreeSet::new(), BTreeSet::new(), BTreeSet::new(), 0);
    visit(
        value,
        0,
        &mut keys,
        &mut features,
        &mut projects,
        &mut visited,
    );
    json!({"keys":keys,"features":features,"projects":projects})
}
pub struct Collector {
    sampled: usize,
    deferred: usize,
    bytes: usize,
    seen: HashSet<PathBuf>,
}
impl Collector {
    pub fn new() -> Self {
        Self {
            sampled: 0,
            deferred: 0,
            bytes: 0,
            seen: HashSet::new(),
        }
    }
    pub fn observe(
        &mut self,
        db: &Connection,
        scan_id: &str,
        path: &Path,
        names: &[String],
        home: &Path,
    ) -> Result<()> {
        let Some(root) = candidate_root(path, home) else {
            return Ok(());
        };
        if scan::protected(&root) {
            return Ok(());
        }
        let layout: Vec<&str> = names
            .iter()
            .filter_map(|n| match n.to_lowercase().as_str() {
                "cache" | "caches" | "tmp" | "temp" => Some("cache"),
                "log" | "logs" | "diagnostics" => Some("log"),
                "sessions" | "conversations" | "threads" | "history" => Some("session"),
                "memory" | "memories" | "knowledge" => Some("memory"),
                "config" | "settings" | "profiles" => Some("config"),
                _ => None,
            })
            .collect();
        if layout.iter().collect::<BTreeSet<_>>().len() >= 3 {
            add(
                db,
                scan_id,
                &root,
                root.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .trim_start_matches('.'),
                json!({"type":"layout","markers":layout,"confidence":"low"}),
                true,
            )?;
        }
        for name in names
            .iter()
            .filter(|n| {
                n.ends_with(".json")
                    || n.ends_with(".jsonl")
                    || n.ends_with(".ndjson")
                    || n.ends_with(".toml")
                    || n.ends_with(".yaml")
                    || n.ends_with(".yml")
                    || n.ends_with(".db")
                    || n.ends_with(".sqlite")
                    || n.ends_with(".sqlite3")
            })
            .take(12)
        {
            let file = path.join(name);
            let database = ["db", "sqlite", "sqlite3"]
                .contains(&file.extension().and_then(|s| s.to_str()).unwrap_or(""));
            if !database && scan::protected(&file) || !self.seen.insert(file.clone()) {
                continue;
            }
            if self.sampled >= 4096 {
                self.deferred += 1;
                continue;
            }
            self.sampled += 1;
            let Ok((bytes, identity)) = files::read_prefix(&file, &root, 16384, false) else {
                self.deferred += 1;
                continue;
            };
            self.bytes += bytes.len();
            let mut sample = json!({"features":[],"keys":[],"projects":[]});
            let mut tables = Vec::new();
            if database {
                if !bytes.starts_with(b"SQLite format 3\0") {
                    self.deferred += 1;
                    db.execute(
                        "INSERT INTO errors(scan,path,code,reason) VALUES(?,?,?,?)",
                        params![
                            scan_id,
                            index::path_string(&file),
                            "UNSUPPORTED_DATABASE",
                            "不是可读取的 SQLite 数据库，未猜测内容或清理权限"
                        ],
                    )?;
                    continue;
                }
                if let Ok(mut url) = reqwest::Url::from_file_path(&file) {
                    url.set_query(Some("mode=ro&immutable=1"));
                    if let Ok(source) = Connection::open_with_flags(
                        url.as_str(),
                        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                            | rusqlite::OpenFlags::SQLITE_OPEN_URI,
                    ) {
                        source.execute_batch("PRAGMA trusted_schema=OFF;PRAGMA query_only=ON;")?;
                        if let Ok(mut stmt) = source
                            .prepare("SELECT name FROM sqlite_schema WHERE type='table' LIMIT 40")
                        {
                            for t in stmt.query_map([], |r| r.get::<_, String>(0))?.flatten() {
                                if [
                                    "session",
                                    "conversation",
                                    "message",
                                    "thread",
                                    "tool_call",
                                    "model",
                                    "memory",
                                ]
                                .iter()
                                .any(|s| t.to_lowercase().contains(s))
                                {
                                    tables.push(t);
                                }
                            }
                        }
                    }
                }
                let mut features = Vec::new();
                if tables.iter().any(|t| {
                    ["session", "message", "thread", "conversation"]
                        .iter()
                        .any(|s| t.contains(s))
                }) {
                    features.push("conversation-record");
                }
                if tables.iter().any(|t| t.contains("tool_call")) {
                    features.push("tool-activity");
                }
                if tables.iter().any(|t| t.contains("model")) {
                    features.push("model-metadata");
                }
                sample["features"] = json!(features);
            } else {
                let text = String::from_utf8_lossy(&bytes);
                let mut values = Vec::new();
                if let Ok(v) = serde_json::from_str::<Value>(&text) {
                    values.push(v)
                } else {
                    for line in text.lines().take(40) {
                        if let Ok(v) = serde_json::from_str::<Value>(line) {
                            values.push(v)
                        }
                    }
                }
                if values.is_empty() {
                    let mut v = serde_json::Map::new();
                    for line in text.lines() {
                        if let Some((k, _)) = line.split_once('=').or_else(|| line.split_once(':'))
                        {
                            let k = k.trim().trim_matches(['\'', '"']);
                            if ["model", "provider", "tools"].contains(&k) {
                                v.insert(k.into(), Value::Bool(true));
                            }
                        }
                    }
                    values.push(Value::Object(v));
                }
                sample = structural(&json!(values));
            }
            sample["tables"] = json!(tables);
            sample["truncated"] = json!(identity.size > bytes.len() as u64);
            sample["format"] = json!(if database {
                "sqlite-schema"
            } else {
                "structure"
            });
            let features = sample["features"].as_array().unwrap();
            if features.len() < 2 {
                continue;
            }
            let existing: Option<String> = db
                .query_row(
                    "SELECT id FROM tools WHERE scan=? AND path=? LIMIT 1",
                    params![scan_id, index::path_string(&root)],
                    |r| r.get(0),
                )
                .ok();
            let evidence = json!({"type":"structure","confidence":"medium","sample":sample,"samplePath":file,"installation":false});
            if existing.is_none() {
                add(
                    db,
                    scan_id,
                    &root,
                    root.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .trim_start_matches('.'),
                    evidence,
                    true,
                )?;
            } else if let Some(id) = existing {
                let uncertain: bool = db.query_row(
                    "SELECT uncertain FROM tools WHERE scan=? AND id=?",
                    params![scan_id, id],
                    |r| r.get(0),
                )?;
                if uncertain {
                    db.execute(
                        "UPDATE tools SET evidence=? WHERE scan=? AND id=?",
                        params![evidence.to_string(), scan_id, id],
                    )?;
                }
            }
        }
        Ok(())
    }
    pub fn finish(&self, db: &Connection, scan: &str) -> Result<()> {
        index::save_setting(
            db,
            &format!("discovery-coverage-{scan}"),
            &json!({"sampledFiles":self.sampled,"deferred":self.deferred,"prefixBytes":self.bytes,"sampleLimit":4096,"prefixLimit":16384,"exhaustive":false}),
        )
    }
}
fn present(db: &Connection, scan: &str, path: &Path) -> bool {
    db.query_row(
        "SELECT EXISTS(SELECT 1 FROM entries WHERE scan=? AND path=?)",
        params![scan, index::path_string(path)],
        |r| r.get(0),
    )
    .unwrap_or(false)
}
fn package(
    db: &Connection,
    scan: &str,
    home: &Path,
    value: &Value,
    origin: &Path,
    bundle: Option<&str>,
    embedded: bool,
) -> Result<()> {
    let name = value["name"].as_str().unwrap_or("");
    let cat = catalog();
    let identity = cat["packageIdentities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r[0] == name);
    let component = cat["componentIdentities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r[0] == name);
    let description = value["description"].as_str().unwrap_or("").to_lowercase();
    let runtime = [
        "coding agent",
        "ai agent",
        "agentic",
        "ai assistant",
        "llm agent",
        "智能体",
        "编程助手",
    ]
    .iter()
    .any(|s| description.contains(s));
    let title = identity
        .map(|r| r[1].as_str().unwrap())
        .or_else(|| component.map(|r| r[1][0].as_str().unwrap()))
        .unwrap_or(name);
    if title.is_empty() {
        return Ok(());
    }

    let evidence = json!({"type":if embedded{"embedded-component"}else{"installation"},"package":name,"version":value["version"],"installation":true,"confidence":if identity.is_some(){"high"}else{"medium"},"origin":origin,"embedded":embedded});
    let install = origin
        .ancestors()
        .find(|p| p.extension().and_then(|s| s.to_str()) == Some("app"))
        .unwrap_or_else(|| origin.parent().unwrap_or(origin));
    if identity.is_some() || component.is_some() || runtime {
        add(
            db,
            scan,
            install,
            title,
            evidence.clone(),
            identity.is_none(),
        )?;
    }
    if embedded {
        return Ok(());
    }
    let slug = name.rsplit('/').next().unwrap_or("");
    let mut roots = Vec::new();
    if !slug.is_empty() && !slug.contains(['\\', '/']) && slug != ".." && slug != "." {
        for base in [
            ".config",
            ".local/share",
            ".local/state",
            ".cache",
            "Library/Application Support",
            "Library/Caches",
            "Library/Logs",
        ] {
            roots.push(home.join(base).join(slug));
        }
        roots.push(home.join(format!(".{slug}")));
        for key in [
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_STATE_HOME",
            "XDG_CACHE_HOME",
        ] {
            if let Some(base) = std::env::var_os(key) {
                roots.push(PathBuf::from(base).join(slug));
            }
        }
    }
    for field in [
        value.get("dataFolderName"),
        value.get("configDir"),
        value.get("dataDir"),
        value.get("userDataDir"),
        value.get("directories").and_then(|v| v.get("config")),
        value.get("directories").and_then(|v| v.get("data")),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(s) = field.as_str() {
            roots.push(if s.starts_with("~/") {
                home.join(&s[2..])
            } else {
                home.join(s)
            });
        }
    }
    if let Some(bundle) = bundle {
        for base in [
            "Library/Application Support",
            "Library/Caches",
            "Library/Logs",
        ] {
            roots.push(home.join(base).join(bundle));
        }
    }
    for root in roots {
        if root.starts_with(home)
            && root != home
            && !excluded(root.strip_prefix(home).unwrap())
            && present(db, scan, &root)
            && !scan::protected(&root)
        {
            let probe = probe_structure(&root, home);
            if let Some(sample) = probe {
                let exists: bool = db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM tools WHERE scan=? AND path=?)",
                    params![scan, index::path_string(&root)],
                    |r| r.get(0),
                )?;
                if !exists {
                    add(
                        db,
                        scan,
                        &root,
                        title,
                        json!({"type":"structure","sample":sample,"confidence":"medium"}),
                        true,
                    )?;
                }
            }
            let strong: bool = db.query_row(
                "SELECT EXISTS(SELECT 1 FROM tools WHERE scan=? AND path=? AND uncertain=1)",
                params![scan, index::path_string(&root)],
                |r| r.get(0),
            )?;
            if identity.is_none() && component.is_none() && !runtime && !strong {
                continue;
            }
            let mut proof = evidence.clone();
            if strong {
                let previous: String = db.query_row(
                    "SELECT evidence FROM tools WHERE scan=? AND path=? AND uncertain=1 LIMIT 1",
                    params![scan, index::path_string(&root)],
                    |r| r.get(0),
                )?;
                if let Ok(previous) = serde_json::from_str::<Value>(&previous) {
                    proof["sample"] = previous["sample"].clone();
                }
            }
            proof["installation"] = json!(false);
            if strong {
                db.execute("UPDATE tools SET name=?,evidence=?,uncertain=? WHERE scan=? AND path=? AND uncertain=1",params![title,proof.to_string(),identity.is_none(),scan,index::path_string(&root)])?;
            } else {
                add(db, scan, &root, title, proof, identity.is_none())?;
            }
        }
    }
    Ok(())
}
pub fn asar_metadata(path: &Path) -> Result<Vec<(String, Value)>> {
    let m = fs::symlink_metadata(path)?;
    if !m.is_file() || m.file_type().is_symlink() || fs::canonicalize(path)? != path {
        return Ok(vec![]);
    }
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let identity = index::Identity::of(&file.metadata()?);
    let mut header = [0u8; 16];
    file.read_exact(&mut header)?;
    let len = u32::from_le_bytes(header[12..16].try_into().unwrap()) as u64;
    let base = 8 + u32::from_le_bytes(header[4..8].try_into().unwrap()) as u64;
    if len == 0 || len > 32 * 1024 * 1024 || base < 16 + len || base > m.len() {
        return Ok(vec![]);
    }
    let mut bytes = vec![0; len as usize];
    file.read_exact(&mut bytes)?;
    let tree: Value = serde_json::from_slice(&bytes)?;
    let mut stack = vec![(String::new(), tree)];
    let mut result = Vec::new();
    let mut visited = 0;
    while let Some((name, entry)) = stack.pop() {
        visited += 1;
        if visited > 250000 || result.len() >= 512 {
            break;
        }
        if let Some(children) = entry["files"].as_object() {
            for (k, v) in children {
                if k != ".." && !k.contains(['/', '\\']) {
                    stack.push((
                        if name.is_empty() {
                            k.clone()
                        } else {
                            format!("{name}/{k}")
                        },
                        v.clone(),
                    ));
                }
            }
            continue;
        }
        if !(name == "package.json"
            || name.ends_with("/package.json") && name.contains("node_modules/"))
            || entry["unpacked"] == true
            || entry.get("link").is_some()
        {
            continue;
        }
        let Some(offset) = entry["offset"]
            .as_str()
            .and_then(|s| s.parse::<u64>().ok())
            .or_else(|| entry["offset"].as_u64())
        else {
            continue;
        };
        let size = entry["size"].as_u64().unwrap_or(u64::MAX);
        let Some(start) = base.checked_add(offset) else {
            continue;
        };
        if size > 1048576 || start.checked_add(size).is_none_or(|end| end > m.len()) {
            continue;
        }
        file.seek(SeekFrom::Start(start))?;
        let mut data = vec![0; size as usize];
        file.read_exact(&mut data)?;
        if let Ok(v) = serde_json::from_slice(&data) {
            result.push((name, v));
        }
    }
    if identity != index::Identity::of(&file.metadata()?)
        || identity != index::Identity::of(&fs::symlink_metadata(path)?)
    {
        return Ok(vec![]);
    }
    Ok(result)
}
pub fn installations(db: &Connection, scan: &str, home: &Path) -> Result<()> {
    installations_with_progress(db, scan, home, |_| Ok(()))
}

const INSTALLATION_PATHS: &str =
    "SELECT path FROM entries WHERE scan=? AND directory=0 AND installation=1 ORDER BY path";

fn installation_paths(db: &Connection, scan: &str) -> Result<Vec<String>> {
    let legacy="SELECT path FROM entries WHERE scan=? AND directory=0 AND (name IN ('package.json','app.asar','Info.plist','METADATA','PKG-INFO') OR parent LIKE '%/bin') ORDER BY path";
    let mut stmt = db.prepare(if crate::compact::is_legacy(db)? {
        legacy
    } else {
        INSTALLATION_PATHS
    })?;
    let rows = stmt.query_map([scan], |r| r.get(0))?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

#[cfg(test)]
mod installation_tests {
    use super::*;

    #[test]
    fn candidate_index_preserves_every_matching_path_in_path_order() {
        let temp = tempfile::tempdir().unwrap();
        let path = index::initialize(temp.path()).unwrap();
        let db = index::connect(&path).unwrap();
        index::prepare_scan_indexes(&db, |_| Ok(())).unwrap();
        db.execute_batch("BEGIN").unwrap();
        for (scan, path, directory) in [
            ("scan", "/z/package.json", false),
            ("scan", "/a/app.asar", false),
            ("scan", "/b/Info.plist", false),
            ("scan", "/c/METADATA", false),
            ("scan", "/d/PKG-INFO", false),
            ("scan", "/cli/bin/unknown-command", false),
            ("scan", "/cli/BIN/upper-case", false),
            ("scan", "/ignored/package.json", true),
            ("other-scan", "/other/package.json", false),
        ] {
            let path = Path::new(path);
            db.execute("INSERT INTO entries(scan,path,parent,name,directory,bytes,logical,unique_bytes,identity,category) VALUES(?,?,?,?,?,0,0,0,'{}','other')", params![scan,index::path_string(path),index::path_string(path.parent().unwrap()),path.file_name().unwrap().to_str().unwrap(),directory]).unwrap();
        }
        for i in 0..5000 {
            db.prepare_cached("INSERT INTO entries(scan,path,parent,name,directory,bytes,logical,unique_bytes,identity,category) VALUES('scan',?,'/data',?,0,0,0,0,'{}','other')").unwrap().execute(params![format!("/data/{i}.bin"),format!("{i}.bin")]).unwrap();
        }
        db.execute_batch("COMMIT; ANALYZE").unwrap();
        let expected = {
            let sql =
                INSTALLATION_PATHS.replace("FROM entries WHERE", "FROM entries NOT INDEXED WHERE");
            let mut stmt = db.prepare(&sql).unwrap();
            let rows = stmt.query_map(["scan"], |r| r.get::<_, String>(0)).unwrap();
            rows.collect::<std::result::Result<Vec<_>, _>>().unwrap()
        };
        let actual = installation_paths(&db, "scan").unwrap();
        assert_eq!(actual.len(), 7);
        assert_eq!(actual, expected);
        let mut stmt = db
            .prepare(&format!("EXPLAIN QUERY PLAN {INSTALLATION_PATHS}"))
            .unwrap();
        let plan = stmt
            .query_map(["scan"], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        assert!(
            plan.iter()
                .any(|line| line.contains("entries_installations")),
            "{plan:?}"
        );
    }
}

pub fn installations_with_progress(
    db: &Connection,
    scan: &str,
    home: &Path,
    mut progress: impl FnMut(&Path) -> Result<()>,
) -> Result<()> {
    progress(home)?;
    for (tool, key) in [
        ("OpenCode", "OPENCODE_CONFIG_DIR"),
        ("OpenCode", "OPENCODE_CONFIG"),
        ("Pi", "PI_CODING_AGENT_DIR"),
        ("Pi", "PI_CODING_AGENT_SESSION_DIR"),
        ("Hermes", "HERMES_HOME"),
        ("Codex", "CODEX_HOME"),
        ("Claude Code", "CLAUDE_CONFIG_DIR"),
    ] {
        if let Ok(value) = std::env::var(key) {
            let p = if value.starts_with("~/") {
                home.join(&value[2..])
            } else {
                PathBuf::from(value)
            };
            if p.starts_with(home) && present(db, scan, &p) {
                add(
                    db,
                    scan,
                    &p,
                    tool,
                    json!({"type":"environment","key":key,"confidence":"high"}),
                    false,
                )?;
            }
        }
    }
    for key in [
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
        "XDG_CACHE_HOME",
    ] {
        if let Some(p) = std::env::var_os(key) {
            let root = PathBuf::from(p).join("opencode");
            if root.starts_with(home) && present(db, scan, &root) {
                add(
                    db,
                    scan,
                    &root,
                    "OpenCode",
                    json!({"type":"environment","key":key,"confidence":"high"}),
                    false,
                )?;
            }
        }
    }
    let paths = installation_paths(db, scan)?;
    let mut manifests = Vec::new();
    let mut apps = HashSet::new();
    for raw in paths {
        let p = PathBuf::from(&raw);
        progress(&p)?;
        let name = p.file_name().unwrap_or_default().to_string_lossy();
        if name == "app.asar" {
            apps.insert(p.parent().unwrap().to_owned());
        } else if name == "package.json"
            && (!excluded(p.strip_prefix(home).unwrap_or(&p)) || raw.contains("/node_modules/"))
        {
            let install = raw.contains("/node_modules/")
                && (raw.starts_with("/opt/")
                    || raw.starts_with("/usr/local/")
                    || raw.contains("/.local/")
                    || raw.contains("/.nvm/")
                    || raw.contains("/.volta/"))
                || raw.contains("/.vscode/")
                || raw.contains("/.cursor/")
                || raw.contains(".app/Contents/Resources/")
                || [
                    "/.workbuddy/app/node_modules/",
                    "/.hermes/hermes-agent/node_modules/",
                    "/.local/share/hermes-workspace/node_modules/",
                    "/.comate/",
                ]
                .iter()
                .any(|p| raw.contains(p));
            if install {
                manifests.push(p);
            }
        } else if ["METADATA", "PKG-INFO"].contains(&name.as_ref())
            && raw.contains("site-packages/")
        {
            if let Ok((bytes, _)) = files::read_prefix(&p, p.parent().unwrap(), 65536, false) {
                let t = String::from_utf8_lossy(&bytes);
                let package_name = t
                    .lines()
                    .find_map(|l| l.strip_prefix("Name: "))
                    .unwrap_or("");
                let version = t
                    .lines()
                    .find_map(|l| l.strip_prefix("Version: "))
                    .unwrap_or("");
                if let Some(row) = catalog()["pythonIdentities"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|r| r[0] == package_name)
                {
                    add(
                        db,
                        scan,
                        p.parent().unwrap(),
                        row[1].as_str().unwrap(),
                        json!({"type":"python-package","package":package_name,"version":version,"installation":true,"confidence":"high"}),
                        false,
                    )?;
                }
            }
        } else if p
            .parent()
            .and_then(|p| p.file_name())
            .is_some_and(|n| n == "bin")
            && !excluded(p.strip_prefix(home).unwrap_or(&p))
        {
            if let Some(row) = catalog()["commandIdentities"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r[0].as_str() == Some(name.as_ref()))
            {
                add(
                    db,
                    scan,
                    &p,
                    row[1].as_str().unwrap(),
                    json!({"type":"command","confidence":"medium","installation":true}),
                    false,
                )?;
            }
        }
    }
    let mut app_bases = vec![home.join("Applications")];
    if dirs::home_dir().as_deref() == Some(home) {
        app_bases.push(PathBuf::from("/Applications"));
    }
    for base in app_bases {
        progress(&base)?;
        if let Ok(entries) = fs::read_dir(base) {
            for e in entries.flatten() {
                let root = e.path();
                if root.extension().and_then(|s| s.to_str()) == Some("app")
                    && fs::canonicalize(&root).ok().as_ref() == Some(&root)
                {
                    apps.insert(root.join("Contents/Resources"));
                }
            }
        }
    }
    if dirs::home_dir().as_deref() == Some(home) {
        for base in [
            PathBuf::from("/opt/homebrew/lib/node_modules"),
            PathBuf::from("/usr/local/lib/node_modules"),
        ] {
            if let Ok(entries) = fs::read_dir(&base) {
                for e in entries.flatten() {
                    let root = e.path();
                    let packages = if e.file_name().to_string_lossy().starts_with('@') {
                        fs::read_dir(&root)
                            .ok()
                            .into_iter()
                            .flatten()
                            .flatten()
                            .map(|e| e.path())
                            .collect::<Vec<_>>()
                    } else {
                        vec![root]
                    };
                    for root in packages {
                        if let Ok(p) = fs::canonicalize(root.join("package.json")) {
                            manifests.push(p);
                        }
                    }
                }
            }
        }
        let mut directories: Vec<PathBuf> =
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect();
        for leaf in [
            ".local/bin",
            ".comate/bin",
            ".grok/bin",
            ".qoder-cn/bin",
            ".qoder-cn/entry",
        ] {
            directories.push(home.join(leaf));
        }
        directories.sort();
        directories.dedup();
        for directory in directories {
            progress(&directory)?;
            if let Ok(entries) = fs::read_dir(directory) {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if let Some(row) = catalog()["commandIdentities"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|r| r[0].as_str() == Some(&name))
                    {
                        if let Ok(path) = fs::canonicalize(entry.path()) {
                            if fs::metadata(&path).is_ok_and(|m| m.is_file()) {
                                add(
                                    db,
                                    scan,
                                    &path,
                                    row[1].as_str().unwrap(),
                                    json!({"type":"command","installation":true,"confidence":"medium"}),
                                    false,
                                )?;
                            }
                        }
                    }
                }
            }
        }
    }
    for rule in catalog()["embeddedCommands"].as_array().unwrap() {
        let p = home.join(rule[2].as_str().unwrap());
        if fs::symlink_metadata(&p).is_ok_and(|m| m.is_file() && !m.file_type().is_symlink()) {
            add(
                db,
                scan,
                &p,
                rule[1].as_str().unwrap(),
                json!({"type":"embedded-command","host":rule[0],"confidence":"medium","installation":true}),
                false,
            )?;
        }
    }
    for resource in apps {
        progress(&resource)?;
        let bundle = plist::Value::from_file(resource.parent().unwrap().join("Info.plist"))
            .ok()
            .and_then(|v| {
                v.as_dictionary()
                    .and_then(|d| d.get("CFBundleIdentifier"))
                    .and_then(|v| v.as_string())
                    .map(str::to_owned)
            });
        if let Some(app) = resource.parent().and_then(|p| p.parent()) {
            let name = app
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let plist = resource.parent().unwrap().join("Info.plist");
            for leaf in [Some(name.as_str()), bundle.as_deref()]
                .into_iter()
                .flatten()
            {
                for parent in [
                    "Library/Application Support",
                    "Library/Caches",
                    "Library/Logs",
                ] {
                    let target = home.join(parent).join(leaf);
                    let previous:Option<String>=db.query_row("SELECT evidence FROM tools WHERE scan=? AND path=? AND uncertain=1 LIMIT 1",params![scan,index::path_string(&target)],|r|r.get(0)).ok();
                    if let Some(previous) = previous {
                        let mut proof: Value = serde_json::from_str(&previous)?;
                        proof["installationIdentity"] =
                            json!({"path":plist,"name":name,"bundleId":bundle});
                        proof["confidence"] = json!("high");
                        db.execute("UPDATE tools SET name=?,evidence=? WHERE scan=? AND path=? AND uncertain=1",params![name,proof.to_string(),scan,index::path_string(&target)])?;
                    }
                }
            }
        }
        let mut product = Value::Null;
        for p in [
            resource.join("app/product.json"),
            resource.join("product.json"),
        ] {
            if let Ok((bytes, _)) = files::read_prefix(&p, &resource, 1048576, false) {
                if let Ok(value) = serde_json::from_slice(&bytes) {
                    product = value;
                    break;
                }
            }
        }
        for file in [
            resource.join("app/package.json"),
            resource.join("package.json"),
        ] {
            if file.is_file() {
                if let Ok((bytes, _)) = files::read_prefix(&file, &resource, 1048576, false) {
                    if let Ok(mut v) = serde_json::from_slice::<Value>(&bytes) {
                        if v.get("dataFolderName").is_none() && !product["dataFolderName"].is_null()
                        {
                            v["dataFolderName"] = product["dataFolderName"].clone();
                        }
                        package(db, scan, home, &v, &file, bundle.as_deref(), false)?;
                    }
                }
            }
        }
        if let Ok(rows) = asar_metadata(&resource.join("app.asar")) {
            for (name, mut value) in rows {
                if name == "package.json"
                    && value.get("dataFolderName").is_none()
                    && !product["dataFolderName"].is_null()
                {
                    value["dataFolderName"] = product["dataFolderName"].clone();
                }
                package(
                    db,
                    scan,
                    home,
                    &value,
                    &resource.join("app.asar").join(&name),
                    bundle.as_deref(),
                    name != "package.json",
                )?;
            }
        }
    }
    for p in manifests.into_iter().take(4096) {
        progress(&p)?;
        if let Ok((bytes, _)) = files::read_prefix(&p, p.parent().unwrap(), 1048576, false) {
            if let Ok(value) = serde_json::from_slice(&bytes) {
                package(db, scan, home, &value, &p, None, false)?;
            }
        }
    }
    Ok(())
}

pub fn candidates(db: &Connection, scan: &str) -> Result<Value> {
    use std::hash::{Hash, Hasher};
    let tools = discovery::tools(db, scan)?;
    let mut rows = Vec::new();
    for tool in tools["rows"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["uncertain"] == true)
    {
        let proof = &tool["evidence"];
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        format!("{}:{}", tool["path"], proof).hash(&mut hash);
        rows.push(json!({"id":tool["id"],"name":tool["name"],"path":tool["path"],"fingerprint":format!("{:016x}",hash.finish()),"metadata":{"id":tool["id"],"name":tool["name"],"installationOnly":proof["installation"]==true,"confidence":proof["confidence"],"features":proof["sample"]["features"],"keys":proof["sample"]["keys"],"tables":proof["sample"]["tables"],"markers":proof["markers"],"package":proof["package"],"version":proof["version"],"application":{"name":proof["installationIdentity"]["name"],"bundleId":proof["installationIdentity"]["bundleId"]}}}));
    }
    Ok(
        json!({"rows":rows,"coverage":index::setting(db,&format!("discovery-coverage-{scan}"))?.unwrap_or(json!({})),"knowledge":index::setting(db,"harness-observations")?.unwrap_or(json!([]))}),
    )
}
pub fn analyze_candidates(
    db: &Connection,
    scan: &str,
    config: &crate::ai::Config,
    offset: usize,
    all: bool,
) -> Result<Value> {
    let input = candidates(db, scan)?;
    let candidates = input["rows"].as_array().unwrap();
    let mut knowledge = index::setting(db, "harness-observations")?
        .unwrap_or(json!([]))
        .as_array()
        .cloned()
        .unwrap_or_default();
    knowledge.retain(|r| {
        chrono::DateTime::parse_from_rfc3339(r["createdAt"].as_str().unwrap_or(""))
            .is_ok_and(|t| chrono::Utc::now().timestamp() - t.timestamp() < 30 * 86400)
    });
    let endpoint = format!("{}:{}:{}", config.provider, config.base_url, config.model);
    let end = if all {
        (offset + 96).min(candidates.len())
    } else {
        (offset + 12).min(candidates.len())
    };
    let mut answers = Vec::new();
    let mut batch = Vec::new();
    for candidate in candidates
        .iter()
        .skip(offset)
        .take(end.saturating_sub(offset))
    {
        if let Some(old) = knowledge.iter().find(|k| {
            k["path"] == candidate["path"]
                && k["fingerprint"] == candidate["fingerprint"]
                && k["endpoint"] == endpoint
        }) {
            let mut answer = old["analysis"].clone();
            answer["id"] = candidate["id"].clone();
            answer["cached"] = json!(true);
            answers.push(answer);
        } else {
            batch.push(candidate.clone());
        }
    }
    for chunk in batch.chunks(12) {
        let metadata: Vec<Value> = chunk.iter().map(|r| r["metadata"].clone()).collect();
        let results = crate::advice::harness(config, &metadata)?;
        for answer in results {
            let candidate = chunk.iter().find(|r| r["id"] == answer["id"]).unwrap();
            knowledge.retain(|k| k["path"] != candidate["path"] || k["endpoint"] != endpoint);
            knowledge.push(json!({"path":candidate["path"],"fingerprint":candidate["fingerprint"],"endpoint":endpoint,"analysis":answer,"createdAt":crate::now()}));
            answers.push(answer);
        }
        println!(
            "{}",
            json!({"event":"progress","stats":{"completed":answers.len(),"total":end.saturating_sub(offset),"stage":"harness-analysis"}})
        );
        let _ = std::io::Write::flush(&mut std::io::stdout());
    }
    if knowledge.len() > 2000 {
        knowledge.drain(..knowledge.len() - 2000);
    }
    index::save_setting(db, "harness-observations", &json!(knowledge))?;
    Ok(
        json!({"scanId":scan,"items":answers,"offset":offset,"total":candidates.len(),"nextOffset":if end<candidates.len(){Some(end)}else{None},"analysisDocumentRead":false,"createdAt":crate::now()}),
    )
}

fn probe_structure(root: &Path, home: &Path) -> Option<Value> {
    if root == home || !root.starts_with(home) || scan::protected(root) {
        return None;
    }
    let mut pending = vec![(root.to_owned(), 0)];
    let mut sampled = 0;
    while let Some((directory, depth)) = pending.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten().take(160) {
            let path = entry.path();
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.file_type().is_symlink() || scan::protected(&path) {
                continue;
            }
            if meta.is_dir() && depth < 3 {
                pending.push((path, depth + 1));
            } else if meta.is_file()
                && ["json", "jsonl", "ndjson"]
                    .contains(&path.extension().and_then(|s| s.to_str()).unwrap_or(""))
            {
                sampled += 1;
                if sampled > 32 {
                    return None;
                }
                if let Ok((bytes, _)) = files::read_prefix(&path, home, 16384, false) {
                    let text = String::from_utf8_lossy(&bytes);
                    let values = serde_json::from_str::<Value>(&text)
                        .ok()
                        .map(|v| vec![v])
                        .unwrap_or_else(|| {
                            text.lines()
                                .take(40)
                                .filter_map(|l| serde_json::from_str(l).ok())
                                .collect()
                        });
                    let sample = structural(&json!(values));
                    if sample["features"].as_array().is_some_and(|a| a.len() >= 2) {
                        return Some(sample);
                    }
                }
            }
        }
    }
    None
}
