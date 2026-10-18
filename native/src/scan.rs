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
    os::unix::fs::MetadataExt,
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
        && parts
            .iter()
            .any(|s| ["sessions", "conversations", "threads", "rollouts"].contains(&s.as_str()))
        && [".jsonl", ".json", ".jsonl.gz", ".jsonl.zst"]
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
        _ => "READ_ERROR",
    };
    let reason = match code {
        "EPERM" => "macOS 隐私保护或文件权限阻止读取",
        "EACCES" => "文件权限不允许读取",
        "ENOENT" => "扫描期间路径已消失",
        _ => "文件系统读取失败",
    };
    db.execute(
        "INSERT INTO errors(scan,path,code,reason) VALUES(?,?,?,?)",
        params![scan, index::path_string(path), code, reason],
    )?;
    let target = index::path_string(path);
    let changed = db.execute(
        "UPDATE entries SET skipped=skipped+1 WHERE scan=? AND path=? AND directory=1",
        params![scan, target],
    )?;
    if changed == 0 {
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
    let db = index::connect(&spec.data.join("index.sqlite3"))?;
    let root = fs::symlink_metadata(&spec.root)?;
    if !root.is_dir() || root.file_type().is_symlink() || fs::canonicalize(&spec.root)? != spec.root
    {
        return Err(fail("扫描入口含符号链接或不是目录"));
    }
    index::prune(&db)?;
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
        let m = match fs::symlink_metadata(&current) {
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
        if fs::canonicalize(&current).ok().as_ref() != Some(&current) {
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
        let entries = match fs::read_dir(&current) {
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
            let m = match fs::symlink_metadata(&physical) {
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
                db.execute_batch("COMMIT; BEGIN IMMEDIATE")?;
            }
            if published.elapsed().as_millis() > 250 {
                stats.elapsed_ms = started.elapsed().as_millis();
                emit(&stats)?;
                published = Instant::now();
            }
        }
        discovery::observe_directory(&db, &spec.id, &path, &names, &spec.home)?;
    }
    db.execute_batch("COMMIT")?;
    stats.stage = "projects".into();
    stats.elapsed_ms = started.elapsed().as_millis();
    emit(&stats)?;
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
    discovery::finish(&db, &spec.id, &spec.home)?;
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
    for (project, path) in &roots {
        let prefix = format!("{}/", path.display());
        let upper = format!("{}0", path.display());
        db.execute(
            "UPDATE entries SET project=? WHERE scan=? AND project IS NULL AND path>=? AND path<?",
            params![project, spec.id, prefix, upper],
        )?;
    }
    db.execute("UPDATE entries SET project=(SELECT min(project) FROM associations a WHERE a.scan=entries.scan AND a.path=entries.path AND a.status IN ('readable','recovered') GROUP BY a.path HAVING count(DISTINCT project)=1) WHERE scan=? AND project IS NULL AND directory=0",[&spec.id])?;
    db.execute_batch("COMMIT")?;
    stats.stage = "storage".into();
    emit(&stats)?;
    // Each directory visits only its immediate children; a parent index keeps
    // this pass linear in the number of entries, without a recursive re-scan.
    let dirs: Vec<String> = {
        let mut stmt = db.prepare(
            "SELECT path FROM entries WHERE scan=? AND directory=1 ORDER BY length(path) DESC",
        )?;
        let rows = stmt.query_map([&spec.id], |r| r.get(0))?;
        rows.collect::<std::result::Result<_, _>>()?
    };
    db.execute_batch("BEGIN IMMEDIATE")?;
    for (i, path) in dirs.iter().enumerate() {
        let (bytes,logical,unique,files,children):(i64,i64,i64,i64,i64)=db.query_row("SELECT coalesce(sum(bytes),0),coalesce(sum(logical),0),coalesce(sum(unique_bytes),0),coalesce(sum(files),0),coalesce(sum(dirs+directory),0) FROM entries WHERE scan=? AND parent=? AND path<>?",params![spec.id,path,path],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?;
        let skipped: i64 = db.query_row(
            "SELECT coalesce(sum(skipped),0) FROM entries WHERE scan=? AND parent=? AND path<>?",
            params![spec.id, path, path],
            |r| r.get(0),
        )?;
        db.execute("UPDATE entries SET bytes=?,logical=?,unique_bytes=?,files=?,dirs=?,skipped=skipped+? WHERE scan=? AND path=?",params![bytes,logical,unique,files,children,skipped,spec.id,path])?;
        if i % 1000 == 0 {
            db.execute_batch("COMMIT; BEGIN IMMEDIATE")?;
            stats.current_path = path.clone();
            stats.elapsed_ms = started.elapsed().as_millis();
            emit(&stats)?;
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
    let logical_root = logical(&spec.root);
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
    m: &fs::Metadata,
    unique: u64,
    category: &str,
    project: Option<&str>,
) -> Result<()> {
    db.execute("INSERT OR REPLACE INTO entries(scan,path,parent,name,directory,bytes,logical,unique_bytes,files,identity,category,project) VALUES(?,?,?,?,?,?,?,?,?,?,?,?)",params![scan,index::path_string(path),index::path_string(path.parent().unwrap_or(path)),path.file_name().unwrap_or_default().to_string_lossy(),m.is_dir(),if m.is_file(){m.blocks()*512}else{0},if m.is_file(){m.len()}else{0},unique,if m.is_file(){1}else{0},serde_json::to_string(&Identity::of(m))?,category,project])?;
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
