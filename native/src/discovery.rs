use crate::{files, id, index, scan, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        OnceLock,
    },
};
static SAMPLES: AtomicUsize = AtomicUsize::new(0);
pub(crate) fn catalog() -> &'static Value {
    static CATALOG: OnceLock<Value> = OnceLock::new();
    CATALOG.get_or_init(|| {
        serde_json::from_str(include_str!("../resources/agent-catalog.json"))
            .expect("bundled catalog")
    })
}
fn excluded(path: &Path) -> bool {
    path.components().any(|c| {
        [
            "fixtures",
            "backups",
            "backup",
            ".private-backups",
            "acceptance-evidence",
            ".runtime",
            ".git",
            ".Trash",
            "target",
        ]
        .contains(&c.as_os_str().to_string_lossy().as_ref())
    })
}
pub(crate) fn add(
    db: &Connection,
    scan: &str,
    path: &Path,
    name: &str,
    evidence: Value,
    uncertain: bool,
) -> Result<()> {
    let p = index::path_string(path);
    let exists: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM tools WHERE scan=? AND path=? AND name=?)",
        params![scan, p, name],
        |r| r.get(0),
    )?;
    if exists {
        let previous: String = db.query_row(
            "SELECT evidence FROM tools WHERE scan=? AND path=? AND name=?",
            params![scan, p, name],
            |r| r.get(0),
        )?;
        let mut proof: Value = serde_json::from_str(&previous)?;
        let mut sources = proof["sources"]
            .as_array()
            .cloned()
            .unwrap_or_else(|| vec![proof.clone()]);
        if !sources.contains(&evidence) && sources.len() < 32 {
            sources.push(evidence.clone());
        }
        if evidence["confidence"] == "high" {
            let sources = sources.clone();
            proof = evidence.clone();
            proof["sources"] = json!(sources);
        } else {
            proof["sources"] = json!(sources);
        }
        db.execute("UPDATE tools SET evidence=?,uncertain=uncertain AND ? WHERE scan=? AND path=? AND name=?",params![proof.to_string(),uncertain,scan,p,name])?;
    } else {
        db.execute(
            "INSERT INTO tools(scan,id,path,name,evidence,uncertain) VALUES(?,?,?,?,?,?)",
            params![scan, id(), p, name, evidence.to_string(), uncertain],
        )?;
    }
    Ok(())
}
pub fn observe_directory(
    db: &Connection,
    scan: &str,
    path: &Path,
    names: &[String],
    home: &Path,
) -> Result<()> {
    if excluded(path) {
        return Ok(());
    }
    let cat = catalog();
    for rule in cat["signatureRules"].as_array().into_iter().flatten() {
        if rule[1]
            .as_array()
            .unwrap()
            .iter()
            .all(|n| names.iter().any(|s| Some(s.as_str()) == n.as_str()))
        {
            add(
                db,
                scan,
                path,
                rule[0].as_str().unwrap(),
                json!({"type":"signature","markers":rule[1],"confidence":"high"}),
                false,
            )?;
        }
    }
    // A package must be an executable installation, never just an SDK dependency.
    let global = path.starts_with("/opt/homebrew/lib/node_modules")
        || path.starts_with("/usr/local/lib/node_modules")
        || path.starts_with(home.join(".local/share"))
        || path
            .components()
            .any(|c| c.as_os_str() == ".vscode" || c.as_os_str() == ".cursor");
    if global
        && names.iter().any(|n| n == "package.json")
        && SAMPLES.fetch_add(1, Ordering::Relaxed) < 4096
    {
        if let Ok((bytes, _)) = files::read_prefix(&path.join("package.json"), path, 65536, false) {
            if let Ok(package) = serde_json::from_slice::<Value>(&bytes) {
                let package_name = package["name"].as_str().unwrap_or("");
                let identity = cat["packageIdentities"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|row| row[0].as_str() == Some(package_name));
                let extension = cat["extensionIdentities"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|row| {
                        row[0].as_str()
                            == Some(&format!(
                                "{}.{}",
                                package["publisher"].as_str().unwrap_or(""),
                                package_name
                            ))
                    });
                if let Some(row) = identity.or(extension) {
                    add(
                        db,
                        scan,
                        path,
                        row[1].as_str().unwrap(),
                        json!({"type":"installation","package":package_name,"version":package["version"],"confidence":"high"}),
                        false,
                    )?;
                } else if package.get("bin").is_some()
                    && looks_agent(package["description"].as_str().unwrap_or(""))
                {
                    let label = if package_name.is_empty() {
                        path.file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned()
                    } else {
                        package_name.to_owned()
                    };
                    add(
                        db,
                        scan,
                        path,
                        &label,
                        json!({"type":"installation","package":package_name,"version":package["version"],"confidence":"medium"}),
                        true,
                    )?;
                }
            }
        }
    }
    let under_dependencies = path.components().any(|c| {
        ["node_modules", "site-packages", "vendor", ".venv"]
            .contains(&c.as_os_str().to_string_lossy().as_ref())
    });
    if !under_dependencies && path.starts_with(home) && !names.iter().any(|s| s == ".git") {
        let sessions = names
            .iter()
            .any(|s| ["sessions", "conversations", "threads"].contains(&s.as_str()));
        let memory = names
            .iter()
            .any(|s| ["memory", "memories", "SOUL.md", "MEMORY.md"].contains(&s.as_str()));
        let config = names
            .iter()
            .any(|s| s.starts_with("config.") || s == "settings.json" || s == "agents.json");
        if sessions && (memory || config) {
            let label = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .trim_start_matches('.')
                .to_owned();
            add(
                db,
                scan,
                path,
                &label,
                json!({"type":"layout","markers":names.iter().filter(|s|["sessions","conversations","threads","memory","memories","config.json","config.yaml","config.toml","SOUL.md"].contains(&s.as_str())).collect::<Vec<_>>(),"confidence":"medium"}),
                true,
            )?;
        }
    }
    Ok(())
}
fn looks_agent(text: &str) -> bool {
    let t = text.to_lowercase();
    [
        "coding agent",
        "ai agent",
        "llm agent",
        "agentic",
        "ai assistant",
    ]
    .iter()
    .any(|s| t.contains(s))
}

pub fn finish(db: &Connection, scan_id: &str, home: &Path) -> Result<()> {
    finish_with_progress(db, scan_id, home, |_, _, _| Ok(()))
}
pub fn finish_with_progress(
    db: &Connection,
    scan_id: &str,
    home: &Path,
    mut progress: impl FnMut(&Path, usize, usize) -> Result<()>,
) -> Result<()> {
    let cat = catalog();
    for rule in cat["agentRules"].as_array().unwrap() {
        let name = rule[0].as_str().unwrap();
        for alias in rule[1].as_array().into_iter().flatten() {
            let path = home.join(alias.as_str().unwrap());
            let present: bool = db.query_row(
                "SELECT EXISTS(SELECT 1 FROM entries WHERE scan=? AND path=?)",
                params![scan_id, index::path_string(&path)],
                |r| r.get(0),
            )?;
            if present {
                add(
                    db,
                    scan_id,
                    &path,
                    name,
                    json!({"type":"known-path","confidence":"medium"}),
                    false,
                )?;
            }
        }
        for app in rule[2].as_array().into_iter().flatten() {
            for base in [
                "Library/Caches",
                "Library/Logs",
                "Library/Application Support",
            ] {
                let path = home.join(base).join(app.as_str().unwrap());
                let present: bool = db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM entries WHERE scan=? AND path=?)",
                    params![scan_id, index::path_string(&path)],
                    |r| r.get(0),
                )?;
                if present {
                    add(
                        db,
                        scan_id,
                        &path,
                        name,
                        json!({"type":"known-path","confidence":"medium"}),
                        false,
                    )?;
                }
            }
        }
    }
    for rule in cat["additionalCaches"].as_array().into_iter().flatten() {
        for alias in rule[1].as_array().into_iter().flatten() {
            let path = home.join(alias.as_str().unwrap_or(""));
            let present: bool = db.query_row(
                "SELECT EXISTS(SELECT 1 FROM entries WHERE scan=? AND path=?)",
                params![scan_id, index::path_string(&path)],
                |r| r.get(0),
            )?;
            if present {
                add(
                    db,
                    scan_id,
                    &path,
                    rule[0].as_str().unwrap_or("Tool"),
                    json!({"type":"known-path","confidence":"medium"}),
                    false,
                )?;
            }
        }
    }
    let mut application_bases = vec![home.join("Applications")];
    if dirs::home_dir().as_deref() == Some(home) {
        application_bases.push(PathBuf::from("/Applications"));
    }
    for base in application_bases {
        if let Ok(apps) = fs::read_dir(&base) {
            for app in apps.flatten() {
                let root = app.path();
                if root.extension().and_then(|s| s.to_str()) != Some("app") {
                    continue;
                }
                if let Some(stem) = root.file_stem().and_then(|s| s.to_str()) {
                    if let Some(rule) = cat["agentRules"].as_array().unwrap().iter().find(|r| {
                        r[2].as_array()
                            .is_some_and(|names| names.iter().any(|n| n.as_str() == Some(stem)))
                    }) {
                        add(
                            db,
                            scan_id,
                            &root,
                            rule[0].as_str().unwrap(),
                            json!({"type":"application-name","confidence":"medium","installation":true}),
                            false,
                        )?;
                    }
                }
                let file = root.join("Contents/Info.plist");
                if fs::symlink_metadata(&file)
                    .map(|m| m.file_type().is_symlink() || m.len() > 1048576)
                    .unwrap_or(true)
                {
                    continue;
                }
                if let Ok(plist) = plist::Value::from_file(&file) {
                    let Some(dict) = plist.as_dictionary() else {
                        continue;
                    };
                    let bundle = dict
                        .get("CFBundleIdentifier")
                        .and_then(|s| s.as_string())
                        .unwrap_or("");
                    if let Some(rule) = cat["agentRules"].as_array().unwrap().iter().find(|r| {
                        r[3].as_array()
                            .map(|a| a.iter().any(|s| s.as_str() == Some(bundle)))
                            .unwrap_or(false)
                    }) {
                        add(
                            db,
                            scan_id,
                            &root,
                            rule[0].as_str().unwrap(),
                            json!({"type":"application","bundleId":bundle,"confidence":"high","installation":true}),
                            false,
                        )?;
                    }
                }
            }
        }
    }
    let tools: Vec<(String, PathBuf, String)> = {
        let mut stmt =
            db.prepare("SELECT id,path,name FROM tools WHERE scan=? ORDER BY length(path) DESC")?;
        let rows = stmt.query_map([scan_id], |r| {
            Ok((r.get(0)?, PathBuf::from(r.get::<_, String>(1)?), r.get(2)?))
        })?;
        rows.collect::<std::result::Result<_, _>>()?
    };
    for (i, (tool, path, _)) in tools.iter().enumerate() {
        progress(path, i, tools.len())?;
        let p = index::path_string(path);
        let lower = format!("{p}/");
        let upper = format!("{p}0");
        db.execute("UPDATE entries SET tool=? WHERE scan=? AND tool IS NULL AND (path=? OR (path>=? AND path<?))",params![tool,scan_id,p,lower,upper])?;
    }
    let rows: Vec<(String, String, String)> = {
        let mut stmt=db.prepare("SELECT path,tool,category FROM entries WHERE scan=? AND directory=0 AND tool IS NOT NULL")?;
        let mapped = stmt.query_map([scan_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        mapped.collect::<std::result::Result<_, _>>()?
    };
    for (path, _tool, _) in &rows {
        let kind = scan::category(Path::new(path), true);
        db.prepare_cached("UPDATE entries SET category=? WHERE scan=? AND path=?")?
            .execute(params![kind, scan_id, path])?;
    }
    for (i, (path, tool, _)) in rows.iter().enumerate() {
        progress(Path::new(path), i, rows.len())?;
        let filename = Path::new(path);
        if scan::protected(filename)
            && !["db", "sqlite", "sqlite3"]
                .contains(&filename.extension().and_then(|s| s.to_str()).unwrap_or(""))
        {
            continue;
        }
        let kind = scan::category(filename, true);
        let name = filename.file_name().unwrap_or_default().to_string_lossy();
        let header_candidate = name.ends_with(".jsonl")
            || name.ends_with(".jsonl.gz")
            || name.ends_with(".jsonl.zst")
            || name.ends_with(".ndjson")
            || name.ends_with(".ndjson.gz")
            || name.ends_with(".ndjson.zst");
        if kind == "session" || header_candidate {
            match files::read_prefix(filename, home, 262144, true) {
                Ok((bytes, _)) => {
                    let text = String::from_utf8_lossy(&bytes);
                    let mut references = HashSet::new();
                    let mut title = String::new();
                    for line in text.lines().take(128) {
                        if let Ok(value) = serde_json::from_str::<Value>(line) {
                            metadata(&value, &mut references, &mut title, 0);
                        }
                    }
                    if references.len() == 1 {
                        let cwd = references.into_iter().next().unwrap();
                        db.execute(
                            "UPDATE entries SET category='session' WHERE scan=? AND path=?",
                            params![scan_id, path],
                        )?;
                        association(
                            db, scan_id, filename, filename, &cwd, tool, &title, false, home,
                            "session",
                        )?;
                    }
                }
                Err(error) => {
                    db.execute(
                        "INSERT INTO errors(scan,path,code,reason) VALUES(?,?,?,?)",
                        params![scan_id, path, "SESSION_READ_ERROR", error.to_string()],
                    )?;
                }
            }
        } else if ["db", "sqlite", "sqlite3"]
            .contains(&filename.extension().and_then(|s| s.to_str()).unwrap_or(""))
            && !scan::protected(filename.parent().unwrap_or(filename))
        {
            if let Err(error) = read_database(db, scan_id, filename, tool, home) {
                db.execute(
                    "INSERT INTO errors(scan,path,code,reason) VALUES(?,?,?,?)",
                    params![scan_id, path, "DB_READ_ERROR", error.to_string()],
                )?;
            }
        }
    }
    // Memory in a session's own subdirectory can share that project's association.
    // Never infer ownership from the whole tool root or an ambiguous container.
    for (i, (path, tool, _)) in rows.iter().enumerate() {
        progress(Path::new(path), i, rows.len())?;
        let memory = Path::new(path);
        if scan::category(memory, true) != "memory" {
            continue;
        }
        let tool_root: Option<&PathBuf> = tools
            .iter()
            .find(|(id, _, _)| id == tool)
            .map(|(_, root, _)| root);
        let Some(tool_root) = tool_root else { continue };
        let Some(container) = memory.parent().and_then(|p| {
            if ["memory", "memories"]
                .contains(&p.file_name().unwrap_or_default().to_string_lossy().as_ref())
            {
                p.parent()
            } else {
                Some(p)
            }
        }) else {
            continue;
        };
        if container == tool_root || !container.starts_with(tool_root) {
            continue;
        }
        let lower = format!("{}/", container.display());
        let upper = format!("{}0", container.display());
        let (project,count):(Option<String>,i64)=db.query_row("SELECT min(project),count(DISTINCT project) FROM associations WHERE scan=? AND tool=? AND kind='session' AND status IN ('readable','recovered') AND path>=? AND path<?",params![scan_id,tool,lower,upper],|r|Ok((r.get(0)?,r.get(1)?)))?;
        if count == 1 {
            db.execute("INSERT INTO associations(scan,id,path,indexed_path,project,tool,kind,title,status,reason,managed) VALUES(?,?,?,?,?,?,'memory','','readable',?,0)",params![scan_id,id(),path,path,project,tool,"同一工具子目录中唯一项目的记忆文件"])?;
        }
    }
    Ok(())
}
fn metadata(value: &Value, refs: &mut HashSet<String>, title: &mut String, depth: usize) {
    if depth > 3 {
        return;
    }
    if let Some(object) = value.as_object() {
        for (key, v) in object {
            if [
                "cwd",
                "workingDirectory",
                "working_directory",
                "worktree",
                "project_path",
                "project_root",
                "projectRoot",
                "workspacePath",
                "workingDir",
                "working_dir",
                "workspace_path",
                "workdir",
                "directory",
            ]
            .contains(&key.as_str())
            {
                if let Some(s) = v.as_str() {
                    if (s.starts_with('/') || s.starts_with("file://")) && s.len() < 4096 {
                        refs.insert(s.into());
                    }
                }
            }
            if ["title", "name"].contains(&key.as_str()) && title.is_empty() {
                if let Some(s) = v.as_str() {
                    *title = files::redact(s).chars().take(200).collect();
                }
            }
            if ["payload", "metadata", "session", "context"].contains(&key.as_str()) {
                metadata(v, refs, title, depth + 1);
            }
        }
    }
}
fn read_database(
    index_db: &Connection,
    scan: &str,
    path: &Path,
    tool: &str,
    home: &Path,
) -> Result<()> {
    // Live WAL is read from a verified private pair; the source is never opened writable.
    let snapshot = crate::database::Snapshot::open(path, index_db)?;
    let Ok(mut url) = reqwest::Url::from_file_path(&snapshot.path) else {
        return Ok(());
    };
    url.set_query(Some(if snapshot.immutable {
        "mode=ro&immutable=1"
    } else {
        "mode=ro"
    }));
    let db = Connection::open_with_flags(
        url.as_str(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
            | rusqlite::OpenFlags::SQLITE_OPEN_URI
            | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    db.execute_batch("PRAGMA trusted_schema=OFF; PRAGMA query_only=ON;")?;
    let tables: Vec<String> = {
        let mut stmt = db.prepare(
            "SELECT name FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%'",
        )?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        rows.collect::<std::result::Result<_, _>>()?
    };
    for table in tables {
        if !table.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            || !["session", "thread", "project"]
                .iter()
                .any(|s| table.to_lowercase().contains(s))
        {
            continue;
        }
        let fields: Vec<String> = {
            let mut stmt = db.prepare(&format!("PRAGMA table_info(\"{table}\")"))?;
            let rows = stmt.query_map([], |r| r.get(1))?;
            rows.collect::<std::result::Result<_, _>>()?
        };
        let Some(cwd) = fields.iter().find(|s| {
            [
                "cwd",
                "directory",
                "worktree",
                "working_directory",
                "working_dir",
                "project_path",
                "project_root",
                "workspace_path",
                "path",
                "workspace",
            ]
            .contains(&s.as_str())
        }) else {
            continue;
        };
        let title = fields
            .iter()
            .find(|s| ["title", "name"].contains(&s.as_str()))
            .map(|s| format!("\"{s}\""))
            .unwrap_or("''".into());
        let physical = fields
            .iter()
            .find(|s| ["rollout_path", "session_path", "file_path"].contains(&s.as_str()))
            .map(|s| format!("\"{s}\""))
            .unwrap_or("''".into());
        let query = format!("SELECT \"{cwd}\",{title},{physical} FROM \"{table}\"");
        let mut stmt = db.prepare(&query)?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                r.get::<_, Option<String>>(2)?.unwrap_or_default(),
            ))
        })?;
        for row in rows.flatten() {
            if row.0.starts_with('/') || row.0.starts_with("file://") {
                let file = if row.2.starts_with("file://") {
                    reqwest::Url::parse(&row.2)
                        .ok()
                        .and_then(|u| u.to_file_path().ok())
                        .unwrap_or_else(|| path.to_owned())
                } else if row.2.starts_with('/') {
                    PathBuf::from(&row.2)
                } else if !row.2.is_empty()
                    && !Path::new(&row.2)
                        .components()
                        .any(|c| matches!(c, std::path::Component::ParentDir))
                {
                    path.parent().unwrap_or(home).join(&row.2)
                } else {
                    path.to_owned()
                };
                association(
                    index_db,
                    scan,
                    path,
                    &file,
                    &row.0,
                    tool,
                    &row.1,
                    true,
                    home,
                    if table.to_lowercase().contains("project") {
                        "workspace"
                    } else {
                        "session"
                    },
                )?;
            }
        }
    }
    Ok(())
}
fn association(
    db: &Connection,
    scan: &str,
    source: &Path,
    physical: &Path,
    cwd: &str,
    tool: &str,
    title: &str,
    managed: bool,
    home: &Path,
    kind: &str,
) -> Result<()> {
    let converted = if cwd.starts_with("file://") {
        reqwest::Url::parse(cwd)
            .ok()
            .and_then(|u| u.to_file_path().ok())
            .map(|p| index::path_string(&p))
            .unwrap_or_else(|| cwd.to_owned())
    } else {
        cwd.to_owned()
    };
    let cwd = converted.as_str();
    let directory = Path::new(cwd);
    if !directory.starts_with(home) || directory == home {
        return Ok(());
    }
    let existing: Option<String> = db
        .query_row(
            "SELECT id FROM projects WHERE scan=? AND path=?",
            params![scan, cwd],
            |r| r.get(0),
        )
        .optional()?;
    let nearest: Option<String> = if directory.is_dir() {
        db.query_row("SELECT id FROM projects WHERE scan=? AND markers<>'[]' AND (path=? OR substr(?,1,length(path)+1)=path||'/') ORDER BY length(path) DESC LIMIT 1",params![scan,cwd,cwd],|r|r.get(0)).optional()?
    } else {
        None
    };
    let project = if let Some(id) = nearest.or(existing) {
        id
    } else {
        let pid = id();
        db.execute(
            "INSERT INTO projects(scan,id,path,name,markers) VALUES(?,?,?,?,?)",
            params![
                scan,
                pid,
                cwd,
                directory.file_name().unwrap_or_default().to_string_lossy(),
                "[]"
            ],
        )?;
        pid
    };
    let mut actual = physical.to_path_buf();
    let mut status = "readable";
    let mut reason = if managed {
        "数据库管理的记录，保留；如有WAL则读取独立的一致性副本"
    } else {
        "会话头中的显式工作目录"
    };
    if !physical.starts_with(home) {
        status = "unsafe";
        reason = "关联路径位于用户目录外，未读取";
    } else if !physical.exists() {
        status = "missing";
        reason = "索引中的路径已失效";
        if let Some(name) = physical.file_name().and_then(|s| s.to_str()) {
            let mut stmt=db.prepare("SELECT path FROM entries WHERE scan=? AND tool=? AND directory=0 AND name IN (?,?,?) LIMIT 3")?;
            let matches = stmt
                .query_map(
                    params![
                        scan,
                        tool,
                        name,
                        format!("{name}.gz"),
                        format!("{name}.zst")
                    ],
                    |r| r.get::<_, String>(0),
                )?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            if matches.len() == 1 {
                actual = PathBuf::from(&matches[0]);
                status = "recovered";
                reason = "同一工具范围找到唯一同名或压缩文件";
            } else if matches.len() > 1 {
                status = "ambiguous";
                reason = "匹配多个文件，未猜测归属";
            }
        }
    }
    if ["readable", "recovered"].contains(&status) {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        match fs::symlink_metadata(&actual) {
            Ok(m)
                if m.is_file()
                    && !m.file_type().is_symlink()
                    && m.dev() == fs::metadata(home)?.dev()
                    && fs::canonicalize(&actual).ok().as_ref() == Some(&actual) =>
            {
                if let Err(error) = fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                    .open(&actual)
                {
                    status = if error.kind() == std::io::ErrorKind::PermissionDenied {
                        "permission"
                    } else {
                        "error"
                    };
                    reason = "关联文件无法读取";
                }
            }
            Ok(_) => {
                status = "unsafe";
                reason = "关联文件为链接、特殊文件或跨卷路径，保留";
            }
            Err(error) => {
                status = if error.kind() == std::io::ErrorKind::PermissionDenied {
                    "permission"
                } else {
                    "missing"
                };
                reason = "关联路径已失效或受限";
            }
        }
    }
    db.execute("INSERT INTO associations(scan,id,path,indexed_path,project,tool,kind,title,status,reason,managed) VALUES(?,?,?,?,?,?,?,?,?,?,?)",params![scan,id(),index::path_string(&actual),index::path_string(physical),project,tool,kind,files::redact(title).chars().take(200).collect::<String>(),status,reason,managed])?;
    let _ = source;
    Ok(())
}

pub fn tools(db: &Connection, scan: &str) -> Result<Value> {
    let mut stmt=db.prepare("SELECT t.id,t.name,t.path,t.evidence,t.uncertain,coalesce(sum(CASE WHEN e.directory=0 THEN e.unique_bytes ELSE 0 END),0),coalesce(sum(CASE WHEN e.directory=0 THEN 1 ELSE 0 END),0) FROM tools t LEFT JOIN entries e ON e.scan=t.scan AND e.tool=t.id WHERE t.scan=? GROUP BY t.id ORDER BY 6 DESC")?;
    let mut rows=stmt.query_map([scan],|r|{let evidence:String=r.get(3)?;Ok(json!({"id":r.get::<_,String>(0)?,"name":r.get::<_,String>(1)?,"path":r.get::<_,String>(2)?,"evidence":serde_json::from_str::<Value>(&evidence).unwrap_or(Value::Null),"uncertain":r.get::<_,bool>(4)?,"bytes":r.get::<_,i64>(5)?,"files":r.get::<_,i64>(6)?}))})?.collect::<std::result::Result<Vec<_>,_>>()?;
    for row in &mut rows {
        let mut categories = serde_json::Map::new();
        let mut stmt=db.prepare("SELECT category,sum(unique_bytes),count(*) FROM entries WHERE scan=? AND tool=? AND directory=0 GROUP BY category")?;
        for item in stmt.query_map(params![scan, row["id"].as_str().unwrap_or("")], |r| {
            Ok((
                r.get::<_, String>(0)?,
                json!({"bytes":r.get::<_,i64>(1)?,"files":r.get::<_,i64>(2)?}),
            ))
        })? {
            let (key, value) = item?;
            categories.insert(key, value);
        }
        row["categories"] = json!(categories);
    }
    Ok(json!({"rows":rows,"sampleLimit":4096,"samples":SAMPLES.load(Ordering::Relaxed)}))
}
