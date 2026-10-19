use crate::{
    discovery, fail, id,
    index::{self, Identity},
    now, Result,
};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    time::Instant,
};

#[derive(Clone, Serialize, Deserialize)]
pub struct ScanSpec {
    pub id: String,
    pub root: PathBuf,
    pub home: PathBuf,
    pub data: PathBuf,
    pub disk: bool,
    pub publish: bool,
}
#[derive(Default, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Stats {
    pub files: u64,
    pub directories: u64,
    pub bytes: u64,
    pub logical_bytes: u64,
    pub skipped: u64,
    pub links: u64,
    pub mounts: u64,
    pub elapsed_ms: u128,
    pub current_path: String,
    pub stage: String,
    pub rows: std::collections::BTreeMap<String, Value>,
}

pub fn protected(path: &Path) -> bool {
    path.components().any(|c| {
        let part = c.as_os_str().to_string_lossy().to_lowercase();
        [
            ".ssh",
            ".aws",
            ".gnupg",
            "keychains",
            ".git",
            ".hg",
            ".svn",
            "node_modules",
            "vendor",
            ".venv",
            "venv",
            ".runtime",
            ".trash",
            "backups",
            "backup",
            "fixtures",
            "fixture",
            ".private-backups",
            "acceptance-evidence",
            "models",
            "blobs",
            "codex-runtimes",
            "vm-bundle",
            "vm_bundles",
            "vm_instances",
        ]
        .contains(&part.as_str())
            || part.ends_with(".app")
            || part == ".env"
            || part.starts_with(".env.")
            || ["credential", "secret", "token", "keychain", "keyblob"]
                .iter()
                .any(|s| part.contains(s))
            || [
                ".pem",
                ".key",
                ".p12",
                ".pfx",
                ".sqlite",
                ".sqlite3",
                ".db",
                ".vscdb",
                ".sqlite-wal",
                ".sqlite-shm",
                ".sqlite3-wal",
                ".sqlite3-shm",
                ".db-wal",
                ".db-shm",
                ".vscdb-wal",
                ".vscdb-shm",
                ".gguf",
                ".safetensors",
                ".qcow2",
                ".vmdk",
                ".vdi",
                ".pvm",
                ".vmwarevm",
            ]
            .iter()
            .any(|s| part.ends_with(s))
            || [
                "cookies",
                "login data",
                "auth.json",
                "auth.toml",
                ".npmrc",
                ".netrc",
                ".pypirc",
            ]
            .contains(&part.as_str())
    })
}
pub fn category(path: &Path, tool: bool) -> &'static str {
    if protected(path) {
        return "other";
    }
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    if path
        .parent()
        .and_then(|p| p.file_name())
        .is_some_and(|n| n == "Downloads")
        && ["dmg", "pkg"].contains(
            &path
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_lowercase()
                .as_str(),
        )
    {
        return "installer";
    }
    let parts: Vec<String> = path
        .components()
        .map(|p| p.as_os_str().to_string_lossy().into_owned())
        .collect();
    if ["AGENTS.md", "CLAUDE.md", "MEMORY.md", "SOUL.md"].contains(&name.as_ref())
        || tool
            && parts
                .iter()
                .any(|s| ["memory", "memories"].contains(&s.as_str()))
    {
        return "memory";
    }
    if tool
        && parts.iter().any(|s| {
            [
                "session",
                "sessions",
                "conversations",
                "threads",
                "rollouts",
                "transcripts",
                "history",
                "archived_sessions",
            ]
            .contains(&s.as_str())
        })
        && [
            ".jsonl",
            ".json",
            ".ndjson",
            ".jsonl.gz",
            ".jsonl.zst",
            ".ndjson.gz",
            ".ndjson.zst",
        ]
        .iter()
        .any(|s| name.ends_with(s))
    {
        return "session";
    }
    if name.ends_with(".log")
        || name.contains(".log.")
        || parts
            .iter()
            .any(|s| ["Logs", "logs", ".logs"].contains(&s.as_str()))
    {
        return "log";
    }
    if parts
        .windows(2)
        .any(|p| p[0] == "Library" && p[1] == "Caches")
        || parts.iter().any(|s| {
            [
                "__pycache__",
                ".pytest_cache",
                ".mypy_cache",
                ".ruff_cache",
                ".parcel-cache",
                ".turbo",
                ".vite",
                "_cacache",
            ]
            .contains(&s.as_str())
        })
        || parts
            .windows(2)
            .any(|p| [".next", ".nuxt"].contains(&p[0].as_str()) && p[1] == "cache")
        || parts.iter().any(|s| s == ".cache")
        || tool
            && parts.iter().any(|s| {
                [
                    "Cache",
                    "Code Cache",
                    "GPUCache",
                    "DawnCache",
                    "CachedData",
                    "cache",
                    "caches",
                ]
                .contains(&s.as_str())
            })
    {
        return "cache";
    }
    "other"
}
fn project_eligible(path: &Path, home: &Path) -> bool {
    if !path.starts_with(home) || path == home {
        return false;
    }
    let relative = path.strip_prefix(home).unwrap();
    !relative.components().any(|p| {
        let s = p.as_os_str().to_string_lossy();
        [
            "Library",
            "Applications",
            "node_modules",
            "vendor",
            "target",
            "dist",
            "build",
            ".git",
            ".hg",
            ".venv",
            "venv",
            ".runtime",
            ".Trash",
            "fixtures",
            "fixture",
            ".private-backups",
            "acceptance-evidence",
            "backups",
            "backup",
            ".cache",
            ".npm",
            ".next",
            ".nuxt",
            ".build",
        ]
        .contains(&s.as_ref())
            || s.ends_with(".app")
    })
}
fn marker(name: &str) -> bool {
    [
        ".git",
        ".hg",
        "package.json",
        "pyproject.toml",
        "Cargo.toml",
        "go.mod",
        "pom.xml",
        "build.gradle",
        "build.gradle.kts",
        "Gemfile",
        "composer.json",
        "CMakeLists.txt",
        "Package.swift",
    ]
    .contains(&name)
        || name.ends_with(".xcodeproj")
}
fn error(db: &Connection, scan: &str, path: &Path, e: &io::Error, stats: &mut Stats) -> Result<()> {
    stats.skipped += 1;
    let code = match e.raw_os_error() {
        Some(libc::EPERM) => "EPERM",
        Some(libc::EACCES) => "EACCES",
        Some(libc::ENOENT) => "ENOENT",
        Some(libc::ETIMEDOUT) => "ETIMEDOUT",
        _ => "READ_ERROR",
    };
    let reason = match code {
        "EPERM" => "macOS 隐私保护或文件权限阻止读取",
        "EACCES" => "文件权限不允许读取",
        "ENOENT" => "扫描期间路径已消失",
        "ETIMEDOUT" => "云端目录读取超时，未读取部分保留",
        _ => "文件系统读取失败",
    };
    db.execute(
        "INSERT INTO errors(scan,path,code,reason) VALUES(?,?,?,?)",
        params![scan, index::path_string(path), code, reason],
    )?;
    let target = index::path_string(path);
    let before = db.total_changes();
    db.execute(
        "UPDATE entries SET skipped=skipped+1 WHERE scan=? AND path=? AND directory=1",
        params![scan, target],
    )?;
    if db.total_changes() == before {
        if let Some(parent) = path.parent() {
            db.execute(
                "UPDATE entries SET skipped=skipped+1 WHERE scan=? AND path=? AND directory=1",
                params![scan, index::path_string(parent)],
            )?;
        }
    }
    Ok(())
}
fn emit(stats: &Stats) -> Result<()> {
    println!("{}", json!({"event":"progress","stats":stats}));
    io::stdout().flush()?;
    Ok(())
}

pub fn run(spec: ScanSpec) -> Result<()> {
    println!(
        "{}",
        json!({"event":"progress","stats":{"stage":"preparing","currentPath":spec.root,"removed":0}})
    );
    io::stdout().flush()?;
    let db = index::connect(&spec.data.join("index.sqlite3"))?;
    let root = crate::cloud::metadata(&spec.root)?;
    if !root.is_dir() || root.file_type().is_symlink() || fs::canonicalize(&spec.root)? != spec.root
    {
        return Err(fail("扫描入口含符号链接或不是目录"));
    }
    crate::storage::guard(&db)?;
    let mut pruning = Instant::now();
    index::prune_with_progress(&db, |table, removed| {
        if removed == 0 || pruning.elapsed().as_millis() >= 250 {
            println!(
                "{}",
                json!({"event":"progress","stats":{"stage":"preparing","removed":removed,"indexTable":table}})
            );
            io::stdout().flush()?;
            pruning = Instant::now();
        }
        Ok(())
    })?;
    index::prepare_scan_indexes(&db, |name| {
        println!(
            "{}",
            json!({"event":"progress","stats":{"stage":"preparing","indexTable":name}})
        );
        io::stdout().flush()?;
        Ok(())
    })?;
    let device = root.dev();
    let started = Instant::now();
    db.execute(
        "INSERT INTO scans(id,root,created,status) VALUES(?,?,?,'running')",
        params![spec.id, index::path_string(&spec.root), now()],
    )?;
    let logical = |path: &Path| -> PathBuf {
        if spec.disk && spec.root == Path::new("/System/Volumes/Data") {
            Path::new("/").join(path.strip_prefix(&spec.root).unwrap_or(path))
        } else {
            path.to_path_buf()
        }
    };
    let logical_root = logical(&spec.root);
    let mut extra = crate::discovery_extra::Collector::new();
    let mut pending = vec![spec.root.clone()];
    let mut seen_dirs = HashSet::new();
    let mut hardlinks = HashSet::new();
    let mut projects: HashMap<PathBuf, Vec<String>> = HashMap::new();
    let mut stats = Stats {
        stage: "disk".into(),
        ..Default::default()
    };
    let mut published = Instant::now();
    let mut count = 0;
    db.execute_batch("BEGIN IMMEDIATE")?;
    while let Some(current) = pending.pop() {
        let m = match crate::cloud::metadata(&current) {
            Ok(v) => v,
            Err(e) => {
                error(&db, &spec.id, &logical(&current), &e, &mut stats)?;
                continue;
            }
        };
        if m.file_type().is_symlink() {
            stats.links += 1;
            continue;
        }
        if m.dev() != device {
            stats.mounts += 1;
            continue;
        }
        if !m.is_dir() || !seen_dirs.insert((m.dev(), m.ino())) {
            continue;
        }
        if !crate::cloud::is_cloud(&current)
            && fs::canonicalize(&current).ok().as_ref() != Some(&current)
        {
            stats.links += 1;
            continue;
        }
        let path = logical(&current);
        stats.directories += 1;
        stats.current_path = index::path_string(&path);
        if stats.directories == 1 || published.elapsed().as_millis() > 250 {
            stats.elapsed_ms = started.elapsed().as_millis();
            emit(&stats)?;
            published = Instant::now();
        }
        insert(&db, &spec.id, &path, &m, 0, "other", None)?;
        let entries = match crate::cloud::read_dir(&current) {
            Ok(v) => v,
            Err(e) => {
                error(&db, &spec.id, &path, &e, &mut stats)?;
                continue;
            }
        };
        let mut names = Vec::new();
        for child in entries {
            let child = match child {
                Ok(v) => v,
                Err(e) => {
                    error(&db, &spec.id, &path, &e, &mut stats)?;
                    continue;
                }
            };
            let physical = child.path();
            let entry = logical(&physical);
            let name = child.file_name();
            let Some(name) = name.to_str() else {
                error(
                    &db,
                    &spec.id,
                    &entry,
                    &io::Error::other("文件名编码无法表示"),
                    &mut stats,
                )?;
                continue;
            };
            if names.len() < 4096 {
                names.push(name.to_owned());
            }
            let m = match crate::cloud::metadata(&physical) {
                Ok(v) => v,
                Err(e) => {
                    error(&db, &spec.id, &entry, &e, &mut stats)?;
                    continue;
                }
            };
            if m.file_type().is_symlink() {
                stats.links += 1;
                continue;
            }
            if m.dev() != device {
                stats.mounts += 1;
                continue;
            }
            if marker(name) && project_eligible(&path, &spec.home) {
                projects
                    .entry(path.clone())
                    .or_default()
                    .push(name.to_owned());
            }
            if m.is_dir() {
                pending.push(physical);
                continue;
            }
            if !m.is_file() {
                continue;
            }
            let bytes = m.blocks() * 512;
            let unique = if m.nlink() > 1 && !hardlinks.insert((m.dev(), m.ino())) {
                0
            } else {
                bytes
            };
            stats.files += 1;
            stats.bytes += unique;
            stats.logical_bytes += m.len();
            if let Ok(relative) = entry.strip_prefix(&logical_root) {
                if let Some(first) = relative.components().next() {
                    let name = first.as_os_str().to_string_lossy().into_owned();
                    let target = logical_root.join(&name);
                    let row=stats.rows.entry(name.clone()).or_insert_with(||json!({"name":name,"path":target,"bytes":0,"files":0,"directory":entry!=target,"skipped":0}));
                    row["bytes"] = json!(row["bytes"].as_u64().unwrap_or(0) + unique);
                    row["files"] = json!(row["files"].as_u64().unwrap_or(0) + 1);
                }
            }
            insert(
                &db,
                &spec.id,
                &entry,
                &m,
                unique,
                category(&entry, false),
                None,
            )?;
            count += 1;
            if count % 1000 == 0 {
                db.execute_batch("COMMIT")?;
                crate::storage::guard(&db)?;
                db.execute_batch("BEGIN IMMEDIATE")?;
            }
            if published.elapsed().as_millis() > 250 {
                stats.elapsed_ms = started.elapsed().as_millis();
                emit(&stats)?;
                published = Instant::now();
            }
        }
        discovery::observe_directory(&db, &spec.id, &path, &names, &spec.home)?;
        extra.observe(&db, &spec.id, &path, &names, &spec.home)?;
    }
    db.execute_batch("COMMIT")?;
    crate::storage::guard(&db)?;
    stats.stage = "projects".into();
    stats.elapsed_ms = started.elapsed().as_millis();
    emit(&stats)?;
    db.execute_batch("PRAGMA optimize=0x10002")?;
    extra.finish(&db, &spec.id)?;
    crate::discovery_extra::installations_with_progress(&db, &spec.id, &spec.home, |path| {
        if published.elapsed().as_millis() >= 250 {
            crate::storage::guard(&db)?;
            println!(
                "{}",
                json!({"event":"progress","stats":{"stage":"projects","currentPath":path}})
            );
            io::stdout().flush()?;
            published = Instant::now();
        }
        Ok(())
    })?;
    db.execute_batch("BEGIN IMMEDIATE")?;
    for (path, markers) in &projects {
        db.execute(
            "INSERT INTO projects(scan,id,path,name,markers) VALUES(?,?,?,?,?)",
            params![
                spec.id,
                id(),
                index::path_string(path),
                path.file_name().unwrap_or_default().to_string_lossy(),
                serde_json::to_string(markers)?
            ],
        )?;
    }
    discovery::finish_with_progress(&db, &spec.id, &spec.home, |path, completed, total| {
        if completed == 0 || published.elapsed().as_millis() >= 250 {
            println!(
                "{}",
                json!({"event":"progress","stats":{"stage":"projects","currentPath":path,"completed":completed,"total":total}})
            );
            io::stdout().flush()?;
            published = Instant::now();
        }
        Ok(())
    })?;
    crate::project_info::enrich_with_progress(&db, &spec.id, &spec.home, |path| {
        if published.elapsed().as_millis() >= 250 {
            crate::storage::guard(&db)?;
            println!(
                "{}",
                json!({"event":"progress","stats":{"stage":"projects","currentPath":path,"completed":0,"total":0}})
            );
            io::stdout().flush()?;
            published = Instant::now();
        }
        Ok(())
    })?;
    stats.skipped = db.query_row(
        "SELECT count(*) FROM errors WHERE scan=?",
        [&spec.id],
        |r| r.get(0),
    )?;
    let roots: Vec<(String, PathBuf)> = {
        let mut statement =
            db.prepare("SELECT id,path FROM projects WHERE scan=? ORDER BY length(path) DESC")?;
        let mapped = statement.query_map([&spec.id], |r| {
            Ok((r.get(0)?, PathBuf::from(r.get::<_, String>(1)?)))
        })?;
        mapped.collect::<std::result::Result<_, _>>()?
    };
    db.execute_batch("COMMIT")?;
    for (i, (project, path)) in roots.iter().enumerate() {
        if i == 0 || published.elapsed().as_millis() >= 250 {
            println!(
                "{}",
                json!({"event":"progress","stats":{"stage":"projects","currentPath":path,"completed":i,"total":roots.len()}})
            );
            io::stdout().flush()?;
            published = Instant::now();
        }
        let prefix = format!("{}/", path.display());
        let upper = format!("{}0", path.display());
        loop {
            crate::storage::guard(&db)?;
            let before = db.total_changes();
            db.execute(
                "UPDATE entries SET project=?1 WHERE scan=?2 AND path IN (SELECT path FROM entries WHERE scan=?2 AND project IS NULL AND path>=?3 AND path<?4 LIMIT 2000)",
                params![project, spec.id, prefix, upper],
            )?;
            if published.elapsed().as_millis() >= 250 {
                println!(
                    "{}",
                    json!({"event":"progress","stats":{"stage":"projects","currentPath":path,"completed":i,"total":roots.len()}})
                );
                io::stdout().flush()?;
                published = Instant::now();
            }
            if db.total_changes() - before < 2000 {
                break;
            }
        }
    }
    assign_associated_projects(&db, &spec.id)?;
    stats.stage = "storage".into();
    emit(&stats)?;
    // Descending binary paths put every descendant before its ancestor. The
    // directory index supplies this order without sorting a million long paths.
    let dirs: Vec<String> = {
        let sql = if crate::compact::is_legacy(&db)? {
            "SELECT path FROM entries WHERE scan=? AND directory=1 ORDER BY path DESC"
        } else {
            crate::compact::DIRECTORIES
        };
        let mut stmt = db.prepare(sql)?;
        let rows = stmt.query_map([&spec.id], |r| r.get(0))?;
        rows.collect::<std::result::Result<_, _>>()?
    };
    db.execute_batch("BEGIN IMMEDIATE")?;
    for (i, path) in dirs.iter().enumerate() {
        let (bytes,logical,unique,files,children,skipped):(i64,i64,i64,i64,i64,i64)=db.prepare_cached("SELECT coalesce(sum(bytes),0),coalesce(sum(logical),0),coalesce(sum(unique_bytes),0),coalesce(sum(files),0),coalesce(sum(dirs+directory),0),coalesce(sum(skipped),0) FROM entries WHERE scan=? AND parent=? AND path<>?")?.query_row(params![spec.id,path,path],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?)))?;
        db.prepare_cached("UPDATE entries SET bytes=?,logical=?,unique_bytes=?,files=?,dirs=?,skipped=skipped+? WHERE scan=? AND path=?")?.execute(params![bytes,logical,unique,files,children,skipped,spec.id,path])?;
        if i % 1000 == 0 {
            db.execute_batch("COMMIT")?;
            crate::storage::guard(&db)?;
            db.execute_batch("BEGIN IMMEDIATE")?;
        }
        if i == 0 || published.elapsed().as_millis() >= 250 || i + 1 == dirs.len() {
            stats.current_path = path.clone();
            stats.elapsed_ms = started.elapsed().as_millis();
            println!(
                "{}",
                json!({"event":"progress","stats":{"stage":"storage","completed":i+1,"total":dirs.len(),"currentPath":path,"elapsedMs":stats.elapsed_ms}})
            );
            io::stdout().flush()?;
            published = Instant::now();
        }
    }
    // Folder sizes de-duplicate a hard link inside that folder, while siblings
    // each retain their own footprint. The disk total de-duplicates globally.
    let linked: Vec<(String, String, i64)> = {
        let mut stmt=db.prepare("SELECT path,identity,bytes FROM entries WHERE scan=? AND directory=0 AND json_extract(identity,'$.nlink')>1")?;
        let rows = stmt.query_map([&spec.id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect::<std::result::Result<_, _>>()?
    };
    let mut groups: HashMap<(u64, u64), (i64, Vec<PathBuf>)> = HashMap::new();
    for (path, identity, bytes) in linked {
        let identity: Identity = serde_json::from_str(&identity)?;
        groups
            .entry((identity.dev, identity.ino))
            .or_insert_with(|| (bytes, Vec::new()))
            .1
            .push(PathBuf::from(path));
    }
    for (_, (bytes, paths)) in groups {
        if paths.len() < 2 {
            continue;
        }
        let mut ancestors: HashMap<PathBuf, i64> = HashMap::new();
        for path in paths {
            for ancestor in path
                .ancestors()
                .skip(1)
                .take_while(|p| p.starts_with(&logical_root))
            {
                *ancestors.entry(ancestor.to_owned()).or_default() += 1;
            }
        }
        for (path, count) in ancestors {
            if count > 1 {
                db.execute("UPDATE entries SET bytes=max(0,bytes-?) WHERE scan=? AND path=? AND directory=1",params![(count-1)*bytes,spec.id,index::path_string(&path)])?;
            }
        }
    }
    db.execute_batch("COMMIT")?;
    stats.elapsed_ms = started.elapsed().as_millis();
    stats.stage = "complete".into();
    db.execute_batch("BEGIN IMMEDIATE")?;
    db.execute(
        "UPDATE scans SET status=?,stats=? WHERE id=?",
        params![
            if spec.publish {
                "complete"
            } else {
                "index-complete"
            },
            serde_json::to_string(&stats)?,
            spec.id
        ],
    )?;
    if spec.publish {
        index::save_setting(&db, "dirty", &Value::Null)?;
    }
    db.execute_batch("COMMIT")?;
    if spec.publish {
        println!(
            "{}",
            json!({"event":"complete","scanId":spec.id,"stats":stats})
        );
    }
    Ok(())
}
fn insert(
    db: &Connection,
    scan: &str,
    path: &Path,
    m: &crate::cloud::Metadata,
    unique: u64,
    category: &str,
    project: Option<&str>,
) -> Result<()> {
    db.prepare_cached("INSERT OR REPLACE INTO entries(scan,path,parent,name,directory,bytes,logical,unique_bytes,files,identity,category,project) VALUES(?,?,?,?,?,?,?,?,?,?,?,?)")?.execute(params![scan,index::path_string(path),index::path_string(path.parent().unwrap_or(path)),path.file_name().unwrap_or_default().to_string_lossy(),m.is_dir(),if m.is_file(){m.blocks()*512}else{0},if m.is_file(){m.len()}else{0},unique,if m.is_file(){1}else{0},serde_json::to_string(&m.identity)?,category,project])?;
    Ok(())
}

fn assign_associated_projects(db: &Connection, scan: &str) -> Result<()> {
    // Resolve each associated path once, then look up only that entry through
    // its primary key. Unassociated files require no association queries.
    let mut associations = db.prepare(
        "SELECT path,min(project) FROM associations WHERE scan=? AND status IN ('readable','recovered') GROUP BY path HAVING count(DISTINCT project)=1",
    )?;
    let mut update = db.prepare(
        "UPDATE entries SET project=? WHERE scan=? AND path=? AND project IS NULL AND directory=0",
    )?;
    let rows = associations.query_map([scan], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (path, project) = row?;
        update.execute(params![project, scan, path])?;
    }
    Ok(())
}

pub fn overview(db: &Connection, scan: &str) -> Result<Value> {
    let info = index::scan_info(db, scan)?;
    let root = info["root"].as_str().unwrap_or("");
    let display = if root == "/System/Volumes/Data" {
        "/"
    } else {
        root
    };
    let mut stmt=db.prepare("SELECT name,path,bytes,logical,files,dirs,directory,skipped FROM entries WHERE scan=? AND parent=? AND path<>? ORDER BY bytes DESC")?;
    let rows=stmt.query_map(params![scan,display,display],|r|Ok(json!({"name":r.get::<_,String>(0)?,"path":r.get::<_,String>(1)?,"bytes":r.get::<_,i64>(2)?,"logicalBytes":r.get::<_,i64>(3)?,"files":r.get::<_,i64>(4)?,"directories":r.get::<_,i64>(5)?,"directory":r.get::<_,bool>(6)?,"skipped":r.get::<_,i64>(7)?})))?.collect::<std::result::Result<Vec<_>,_>>()?;
    Ok(json!({"scan":info,"rows":rows}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn association_assignment_retains_ambiguity_status_scope_and_existing_ownership() {
        let temp = tempfile::tempdir().unwrap();
        let file = index::initialize(temp.path()).unwrap();
        let db = index::connect(&file).unwrap();
        let cases = [
            ("unique", false, None, Some("alpha")),
            ("ambiguous", false, None, None),
            ("missing", false, None, None),
            ("recovered", false, None, Some("beta")),
            ("owned", false, Some("local"), Some("local")),
            ("directory", true, None, None),
            ("unassociated", false, None, None),
            ("null-only", false, None, None),
        ];
        for (path, directory, initial, _) in cases {
            db.execute("INSERT INTO entries(scan,path,parent,name,directory,bytes,logical,unique_bytes,identity,category,project) VALUES('current',?,'/',?,?,0,0,0,'{}','other',?)",params![path,path,directory,initial]).unwrap();
        }
        for (scan, path, project, status) in [
            ("current", "unique", Some("alpha"), "readable"),
            ("current", "unique", Some("alpha"), "recovered"),
            ("current", "unique", None, "readable"),
            ("other-scan", "unique", Some("other"), "readable"),
            ("current", "ambiguous", Some("alpha"), "readable"),
            ("current", "ambiguous", Some("beta"), "recovered"),
            ("current", "missing", Some("alpha"), "missing"),
            ("current", "missing", Some("alpha"), "permission"),
            ("current", "recovered", Some("beta"), "recovered"),
            ("current", "recovered", Some("alpha"), "unsafe"),
            ("current", "owned", Some("alpha"), "readable"),
            ("current", "directory", Some("alpha"), "readable"),
            ("current", "null-only", None, "readable"),
        ] {
            db.execute("INSERT INTO associations(scan,id,path,indexed_path,project,tool,kind,title,status,reason,managed) VALUES(?,?,?,?,?,'tool','session','',?,'',0)",params![scan,id(),path,path,project,status]).unwrap();
        }
        assign_associated_projects(&db, "current").unwrap();
        for (path, _, _, expected) in cases {
            let actual: Option<String> = db
                .query_row(
                    "SELECT project FROM entries WHERE scan='current' AND path=?",
                    [path],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(actual.as_deref(), expected, "{path}");
        }
    }
}
