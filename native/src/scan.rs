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
    #[serde(default)]
    pub index_path: Option<PathBuf>,
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

fn protected_component(name: &str) -> bool {
    let part = name.to_lowercase();
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
}
pub fn protected(path: &Path) -> bool {
    path.components()
        .any(|c| protected_component(&c.as_os_str().to_string_lossy()))
}
// Carry ancestor features once through traversal instead of reparsing every path.
#[derive(Clone, Default)]
struct PathPolicy {
    protected: bool,
    memory: bool,
    session: bool,
    log: bool,
    cache: bool,
    tool_cache: bool,
    library: bool,
    next: bool,
    downloads: bool,
}
impl PathPolicy {
    fn at(path: &Path) -> Self {
        path.components().fold(Self::default(), |p, c| {
            p.child(&c.as_os_str().to_string_lossy())
        })
    }
    fn child(&self, name: &str) -> Self {
        Self {
            protected: self.protected || protected_component(name),
            memory: self.memory || ["memory", "memories"].contains(&name),
            session: self.session
                || [
                    "session",
                    "sessions",
                    "conversations",
                    "threads",
                    "rollouts",
                    "transcripts",
                    "history",
                    "archived_sessions",
                ]
                .contains(&name),
            log: self.log || ["Logs", "logs", ".logs"].contains(&name),
            cache: self.cache
                || self.library && name == "Caches"
                || self.next && name == "cache"
                || [
                    "__pycache__",
                    ".pytest_cache",
                    ".mypy_cache",
                    ".ruff_cache",
                    ".parcel-cache",
                    ".turbo",
                    ".vite",
                    "_cacache",
                    ".cache",
                ]
                .contains(&name),
            tool_cache: self.tool_cache
                || [
                    "Cache",
                    "Code Cache",
                    "GPUCache",
                    "DawnCache",
                    "CachedData",
                    "cache",
                    "caches",
                ]
                .contains(&name),
            library: name == "Library",
            next: [".next", ".nuxt"].contains(&name),
            downloads: name == "Downloads",
        }
    }
    fn category(&self, name: &str, tool: bool) -> &'static str {
        if self.protected {
            return "other";
        }
        let child = self.child(name);
        if child.protected {
            return "other";
        }
        if self.downloads
            && Path::new(name)
                .extension()
                .and_then(|s| s.to_str())
                .is_some_and(|s| ["dmg", "pkg"].contains(&s.to_lowercase().as_str()))
        {
            return "installer";
        }
        if ["AGENTS.md", "CLAUDE.md", "MEMORY.md", "SOUL.md"].contains(&name)
            || tool && child.memory
        {
            return "memory";
        }
        if tool
            && child.session
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
        if name.ends_with(".log") || name.contains(".log.") || child.log {
            return "log";
        }
        if child.cache || tool && child.tool_cache {
            return "cache";
        }
        "other"
    }
}
#[derive(Default)]
pub(crate) struct Classifier {
    parent: Option<PathBuf>,
    policy: PathPolicy,
}
impl Classifier {
    fn policy(&mut self, path: &Path) -> &PathPolicy {
        let parent = path.parent().unwrap_or(Path::new(""));
        if self.parent.as_deref() != Some(parent) {
            self.policy = PathPolicy::at(parent);
            self.parent = Some(parent.to_owned());
        }
        &self.policy
    }
    pub fn category(&mut self, path: &Path, tool: bool) -> &'static str {
        self.policy(path).category(
            &path.file_name().unwrap_or_default().to_string_lossy(),
            tool,
        )
    }
    pub fn protected(&mut self, path: &Path) -> bool {
        let p = self.policy(path);
        p.protected || protected_component(&path.file_name().unwrap_or_default().to_string_lossy())
    }
}
pub fn category(path: &Path, tool: bool) -> &'static str {
    PathPolicy::at(path.parent().unwrap_or(Path::new(""))).category(
        &path.file_name().unwrap_or_default().to_string_lossy(),
        tool,
    )
}
#[cfg(test)]
fn reference_category(path: &Path, tool: bool) -> &'static str {
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
    crate::scan::print_event(json!({"event":"progress","stats":stats}));
    io::stdout().flush()?;
    Ok(())
}

pub fn run(spec: ScanSpec) -> Result<()> {
    crate::scan::print_event(
        json!({"event":"progress","stats":{"stage":"preparing","currentPath":spec.root,"removed":0}}),
    );
    io::stdout().flush()?;
    let db = index::connect(
        spec.index_path
            .as_deref()
            .unwrap_or(&index::path(&spec.data)),
    )?;
    let root = crate::cloud::metadata(&spec.root)?;
    if !root.is_dir() || root.file_type().is_symlink() || fs::canonicalize(&spec.root)? != spec.root
    {
        return Err(fail("扫描入口含符号链接或不是目录"));
    }
    crate::storage::guard(&db)?;
    let mut pruning = Instant::now();
    if spec.index_path.is_none() {
        index::prune_with_progress(&db, |table, removed| {
            if removed == 0 || pruning.elapsed().as_millis() >= 250 {
                crate::scan::print_event(
                    json!({"event":"progress","stats":{"stage":"preparing","removed":removed,"indexTable":table}}),
                );
                io::stdout().flush()?;
                pruning = Instant::now();
            }
            Ok(())
        })?;
    }
    index::prepare_scan_indexes(&db, |name| {
        crate::scan::print_event(
            json!({"event":"progress","stats":{"stage":"preparing","indexTable":name}}),
        );
        io::stdout().flush()?;
        Ok(())
    })?;
    // This private, rebuildable file is never published while being written.
    // Mutable user state keeps FULL/WAL durability on its separate database.
    if spec.index_path.is_some() {
        db.execute_batch("PRAGMA main.journal_mode=OFF; PRAGMA main.synchronous=OFF")?;
    }
    let deferred_indexes = spec.index_path.is_some() && !crate::compact::is_legacy(&db)?;
    if deferred_indexes {
        // Keep sparse indexes while walking. Rebuilding each one later would
        // reread the entire multi-GB table, even for an almost-empty index.
        // Bulk-build dense indexes to avoid fragmented random writes.
        for name in ["entries_parent", "entries_directories"] {
            db.execute_batch(&format!("DROP INDEX IF EXISTS {name}"))?;
        }
    }
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
    let mut pending = vec![(
        spec.root.clone(),
        PathPolicy::at(&logical_root),
        root.clone(),
        None::<crate::directory::Handle>,
    )];
    let mut seen_dirs = HashSet::new();
    let mut hardlinks = HashSet::new();
    let mut projects: HashMap<PathBuf, Vec<String>> = HashMap::new();
    let mut stats = Stats {
        stage: "disk".into(),
        ..Default::default()
    };
    let mut published = Instant::now();
    let mut count = 0;
    let mut writer = EntryWriter::new(&db, &spec.id)?;
    db.execute_batch("BEGIN")?;
    while let Some((current, policy, m, parent_handle)) = pending.pop() {
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
        let use_handle = cfg!(target_os = "macos")
            && current.components().count() <= 128
            && !crate::cloud::is_cloud(&current);
        if !use_handle && !crate::cloud::is_cloud(&current) {
            match fs::canonicalize(&current) {
                Ok(actual) if actual == current => {}
                Ok(_) => {
                    stats.links += 1;
                    continue;
                }
                Err(e) => {
                    error(&db, &spec.id, &logical(&current), &e, &mut stats)?;
                    continue;
                }
            }
        }
        let path = logical(&current);
        stats.directories += 1;
        stats.current_path = index::path_string(&path);
        if stats.directories == 1 || published.elapsed().as_millis() > 250 {
            stats.elapsed_ms = started.elapsed().as_millis();
            emit(&stats)?;
            published = Instant::now();
        }
        writer.insert(&path, &m, 0, "other", None)?;
        let handle = if use_handle {
            let opened = match &parent_handle {
                Some(parent) => parent.child(current.file_name().unwrap_or_default(), &m),
                None => crate::directory::Handle::root(&current, &m),
            };
            match opened {
                Ok(handle) => Some(handle),
                Err(e) => {
                    error(&db, &spec.id, &logical(&current), &e, &mut stats)?;
                    continue;
                }
            }
        } else {
            None
        };

        let entries = match handle
            .as_ref()
            .map(|h| h.entries(&current))
            .unwrap_or_else(|| crate::cloud::scan_dir(&current, &m))
        {
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
            let m = match child.metadata() {
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
                pending.push((physical, policy.child(name), m, handle.clone()));
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
            writer.insert(&entry, &m, unique, policy.category(name, false), None)?;
            count += 1;
            if count % 1000 == 0 {
                db.execute_batch("COMMIT")?;
                crate::storage::guard(&db)?;
                db.execute_batch("BEGIN")?;
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
    if deferred_indexes {
        index::prepare_scan_indexes(&db, |name| {
            crate::scan::print_event(
                json!({"event":"progress","stats":{"stage":"projects","indexTable":name}}),
            );
            io::stdout().flush()?;
            Ok(())
        })?;
    }
    let planner_started = Instant::now();
    index::optimize(&db)?;
    crate::scan::print_event(
        json!({"event":"progress","stats":{"stage":"projects","indexTable":"planner","plannerMs":planner_started.elapsed().as_millis()}}),
    );
    io::stdout().flush()?;
    extra.finish(&db, &spec.id)?;
    crate::discovery_extra::installations_with_progress(&db, &spec.id, &spec.home, |path| {
        if path == spec.home || published.elapsed().as_millis() >= 250 {
            crate::storage::guard(&db)?;
            crate::scan::print_event(
                json!({"event":"progress","stats":{"stage":"projects","currentPath":path}}),
            );
            io::stdout().flush()?;
            published = Instant::now();
        }
        Ok(())
    })?;
    db.execute_batch("BEGIN")?;
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
            crate::scan::print_event(
                json!({"event":"progress","stats":{"stage":"projects","currentPath":path,"completed":completed,"total":total}}),
            );
            io::stdout().flush()?;
            published = Instant::now();
        }
        Ok(())
    })?;
    crate::project_info::enrich_with_progress(&db, &spec.id, &spec.home, |path| {
        if published.elapsed().as_millis() >= 250 {
            crate::storage::guard(&db)?;
            crate::scan::print_event(
                json!({"event":"progress","stats":{"stage":"projects","currentPath":path,"completed":0,"total":0}}),
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
            crate::scan::print_event(
                json!({"event":"progress","stats":{"stage":"projects","currentPath":path,"completed":i,"total":roots.len(),"assignedEntries":0}}),
            );
            io::stdout().flush()?;
            published = Instant::now();
        }
        let mut cursor = format!("{}/", path.display());
        let upper = format!("{}0", path.display());
        let mut assigned = 0;
        loop {
            crate::storage::guard(&db)?;
            let Some((last, changed)) =
                index::assign_project_batch(&db, &spec.id, project, &cursor, &upper)?
            else {
                break;
            };
            cursor = last;
            assigned += changed;
            if published.elapsed().as_millis() >= 250 {
                crate::scan::print_event(
                    json!({"event":"progress","stats":{"stage":"projects","currentPath":path,"completed":i,"total":roots.len(),"assignedEntries":assigned}}),
                );
                io::stdout().flush()?;
                published = Instant::now();
            }
        }
    }
    assign_associated_projects(&db, &spec.id)?;
    stats.stage = "storage".into();
    emit(&stats)?;
    // Descending binary paths put every descendant before its ancestor. The
    // directory index supplies this order without sorting a million long paths.
    let legacy = crate::compact::is_legacy(&db)?;
    let total_dirs: u64 = if legacy {
        db.query_row("SELECT count(*) FROM entries INDEXED BY entries_directories WHERE scan=? AND directory=1",[&spec.id],|r|r.get(0))?
    } else {
        db.query_row("SELECT count(*) FROM _entries INDEXED BY entries_directories WHERE scan=(SELECT id FROM entry_scans WHERE value=?) AND directory=1",[&spec.id],|r|r.get(0))?
    };
    // This cursor reads only immutable directory-index keys. Counter updates do
    // not alter those keys, so it remains stable across the bounded commits.
    let sql = if legacy {
        "SELECT path,NULL,NULL FROM entries INDEXED BY entries_directories WHERE scan=? AND directory=1 ORDER BY path DESC"
    } else {
        "SELECT e.path,e.rowid,p.id FROM _entries e INDEXED BY entries_directories LEFT JOIN entry_parents p ON p.path=e.path WHERE e.scan=(SELECT id FROM entry_scans WHERE value=?) AND e.directory=1 ORDER BY e.path DESC"
    };
    let mut dir_statement = db.prepare(sql)?;
    let mut directories = dir_statement.query([&spec.id])?;
    let token: Option<i64> = if legacy {
        None
    } else {
        Some(db.query_row(
            "SELECT id FROM entry_scans WHERE value=?",
            [&spec.id],
            |r| r.get(0),
        )?)
    };
    db.execute_batch("BEGIN")?;
    let mut i = 0_u64;
    while let Some(row) = directories.next()? {
        let path: String = row.get(0)?;
        let rowid: Option<i64> = row.get(1)?;
        let parent: Option<i64> = row.get(2)?;
        let read = |r: &rusqlite::Row<'_>| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
            ))
        };
        let (bytes, logical, unique, files, children, skipped) = if legacy {
            db.prepare_cached("SELECT coalesce(sum(bytes),0),coalesce(sum(logical),0),coalesce(sum(unique_bytes),0),coalesce(sum(files),0),coalesce(sum(dirs+directory),0),coalesce(sum(skipped),0) FROM entries WHERE scan=? AND parent=? AND path<>?")?.query_row(params![spec.id,path,path],read)?
        } else if let Some(parent) = parent {
            db.prepare_cached("SELECT coalesce(sum(bytes),0),coalesce(sum(logical),0),coalesce(sum(unique_bytes),0),coalesce(sum(files),0),coalesce(sum(dirs+directory),0),coalesce(sum(skipped),0) FROM _entries WHERE parent=? AND scan=? AND path<>?")?.query_row(params![parent,token,path],read)?
        } else {
            (0, 0, 0, 0, 0, 0)
        };
        if let Some(rowid) = rowid {
            db.prepare_cached("UPDATE _entries SET bytes=?,logical=?,unique_bytes=?,files=?,dirs=?,skipped=skipped+? WHERE rowid=?")?.execute(params![bytes,logical,unique,files,children,skipped,rowid])?;
        } else {
            db.prepare_cached("UPDATE entries SET bytes=?,logical=?,unique_bytes=?,files=?,dirs=?,skipped=skipped+? WHERE scan=? AND path=?")?.execute(params![bytes,logical,unique,files,children,skipped,spec.id,path])?;
        }
        if i % 1000 == 0 {
            db.execute_batch("COMMIT")?;
            crate::storage::guard(&db)?;
            db.execute_batch("BEGIN")?;
        }
        if i == 0 || published.elapsed().as_millis() >= 250 || i + 1 == total_dirs {
            stats.current_path = path.clone();
            stats.elapsed_ms = started.elapsed().as_millis();
            crate::scan::print_event(
                json!({"event":"progress","stats":{"stage":"storage","completed":i+1,"total":total_dirs,"currentPath":path,"elapsedMs":stats.elapsed_ms}}),
            );
            io::stdout().flush()?;
            published = Instant::now();
        }
        i += 1;
    }
    drop(directories);
    drop(dir_statement);
    stats.directories = total_dirs;
    // Folder sizes de-duplicate a hard link inside that folder, while siblings
    // each retain their own footprint. The disk total de-duplicates globally.
    let linked: Vec<(String, String, i64)> = {
        let mut stmt = db.prepare(crate::compact::hardlinks_query(&db)?)?;
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
    let final_root = crate::cloud::metadata(&spec.root)
        .map_err(|e| fail(format!("扫描入口在扫描期间不可访问，原结果已保留：{e}")))?;
    if !final_root.is_dir()
        || final_root.file_type().is_symlink()
        || final_root.dev() != root.dev()
        || final_root.ino() != root.ino()
    {
        return Err(fail("扫描入口在扫描期间被替换，原结果已保留"));
    }
    stats.elapsed_ms = started.elapsed().as_millis();
    stats.stage = "complete".into();
    db.execute_batch("BEGIN")?;
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
        crate::scan::print_event(json!({"event":"complete","scanId":spec.id,"stats":stats}));
    }
    Ok(())
}
struct EntryWriter<'a> {
    db: &'a Connection,
    scan: &'a str,
    token: Option<i64>,
    parent: Option<(PathBuf, i64)>,
}
impl<'a> EntryWriter<'a> {
    fn new(db: &'a Connection, scan: &'a str) -> Result<Self> {
        let token = if crate::compact::is_legacy(db)? {
            None
        } else {
            db.execute("INSERT INTO entry_scans(value) SELECT ?1 WHERE NOT EXISTS(SELECT 1 FROM entry_scans WHERE value=?1)", [scan])?;
            Some(
                db.query_row("SELECT id FROM entry_scans WHERE value=?", [scan], |r| {
                    r.get(0)
                })?,
            )
        };
        Ok(Self {
            db,
            scan,
            token,
            parent: None,
        })
    }
    fn insert(
        &mut self,
        path: &Path,
        m: &crate::cloud::Metadata,
        unique: u64,
        category: &str,
        project: Option<&str>,
    ) -> Result<()> {
        let parent = path.parent().unwrap_or(path);
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let bytes = if m.is_file() { m.blocks() * 512 } else { 0 };
        let logical = if m.is_file() { m.len() } else { 0 };
        let Some(token) = self.token else {
            self.db.prepare_cached("INSERT OR REPLACE INTO entries(scan,path,parent,name,directory,bytes,logical,unique_bytes,files,identity,category,project) VALUES(?,?,?,?,?,?,?,?,?,?,?,?)")?.execute(params![self.scan,index::path_string(path),index::path_string(parent),name,m.is_dir(),bytes,logical,unique,m.is_file(),serde_json::to_string(&m.identity)?,category,project])?;
            return Ok(());
        };
        let parent_id = match &self.parent {
            Some((previous, id)) if previous == parent => *id,
            _ => {
                let text = index::path_string(parent);
                self.db.prepare_cached("INSERT INTO entry_parents(path) SELECT ?1 WHERE NOT EXISTS(SELECT 1 FROM entry_parents WHERE path=?1)")?.execute([&text])?;
                let id = self
                    .db
                    .prepare_cached("SELECT id FROM entry_parents WHERE path=?")?
                    .query_row([&text], |r| r.get::<_, i64>(0))?;
                self.parent = Some((parent.to_path_buf(), id));
                id
            }
        };
        let identity = &m.identity;
        let bits = serde_json::to_string(&(
            identity.dev,
            identity.ino,
            identity.size,
            identity.mtime,
            identity.mtime_ns,
            identity.ctime,
            identity.ctime_ns,
            identity.nlink,
        ))?;
        let parent_text = parent.as_os_str().to_string_lossy();
        let bin = parent_text
            .get(parent_text.len().saturating_sub(4)..)
            .is_some_and(|p| p.eq_ignore_ascii_case("/bin"));
        let installation = m.is_file()
            && (bin
                || [
                    "package.json",
                    "app.asar",
                    "Info.plist",
                    "METADATA",
                    "PKG-INFO",
                ]
                .contains(&name.as_ref()));
        self.db.prepare_cached("INSERT OR REPLACE INTO _entries(scan,path,parent,name,directory,bytes,logical,unique_bytes,files,identity_bits,category,project,installation) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?)")?.execute(params![token,index::path_string(path),parent_id,name,m.is_dir(),bytes,logical,unique,m.is_file(),bits,category,project,installation])?;
        Ok(())
    }
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
    let query = if crate::compact::is_legacy(db)? || crate::overlay::active(db)? {
        "SELECT name,path,bytes,logical,files,dirs,directory,skipped FROM entries WHERE scan=? AND parent=? AND path<>? ORDER BY bytes DESC"
    } else {
        crate::compact::CHILDREN
    };
    let mut stmt = db.prepare(query)?;
    let rows=stmt.query_map(params![scan,display,display],|r|Ok(json!({"name":r.get::<_,String>(0)?,"path":r.get::<_,String>(1)?,"bytes":r.get::<_,i64>(2)?,"logicalBytes":r.get::<_,i64>(3)?,"files":r.get::<_,i64>(4)?,"directories":r.get::<_,i64>(5)?,"directory":r.get::<_,bool>(6)?,"skipped":r.get::<_,i64>(7)?})))?.collect::<std::result::Result<Vec<_>,_>>()?;
    Ok(json!({"scan":info,"rows":rows}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_writer_matches_compatibility_inserts_including_identity_and_installations() {
        let temp = tempfile::tempdir().unwrap();
        let db = index::connect(&index::initialize(temp.path()).unwrap()).unwrap();
        let paths = [
            "/",
            "/fixture/项目/one",
            "/fixture/项目/two",
            "/fixture/other/Info.plist",
            "/fixture/BIN/executable",
            "/fixture/bin/tool",
            "/fixture/binoculars/ordinary",
            "/fixture/other/package.json",
            "/fixture/项目/one",
        ];
        let mut writer = EntryWriter::new(&db, "direct").unwrap();
        for (i, path) in paths.iter().enumerate() {
            let path = Path::new(path);
            let directory = i == 0 || i == 3;
            let metadata = crate::cloud::Metadata {
                identity: Identity {
                    dev: u64::MAX,
                    ino: u64::MAX - i as u64,
                    size: 123,
                    mtime: -123,
                    mtime_ns: 999999999,
                    ctime: -321,
                    ctime_ns: 456,
                    nlink: if i % 2 == 0 { 2 } else { 1 },
                },
                allocated_blocks: 8,
                directory,
                file: !directory,
                link: false,
            };
            writer
                .insert(
                    path,
                    &metadata,
                    if directory { 0 } else { 4096 },
                    "cache",
                    Some("project"),
                )
                .unwrap();
            db.execute("INSERT OR REPLACE INTO entries(scan,path,parent,name,directory,bytes,logical,unique_bytes,files,identity,category,project) VALUES('view',?,?,?,?,?,?,?,?,?,'cache','project')",params![index::path_string(path),index::path_string(path.parent().unwrap_or(path)),path.file_name().unwrap_or_default().to_string_lossy(),directory,if directory{0}else{4096},if directory{0}else{123},if directory{0}else{4096},!directory,serde_json::to_string(&metadata.identity).unwrap()]).unwrap();
        }
        let read = |scan| {
            db.prepare("SELECT path,parent,name,directory,bytes,logical,unique_bytes,files,dirs,identity,category,project,tool,skipped,installation FROM entries WHERE scan=? ORDER BY path").unwrap()
            .query_map([scan],|r| Ok((0..15).map(|i| format!("{:?}",r.get_ref(i).unwrap())).collect::<Vec<_>>())).unwrap().collect::<std::result::Result<Vec<_>,_>>().unwrap()
        };
        assert_eq!(read("direct"), read("view"));
    }

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

#[cfg(test)]
mod policy_tests {
    use super::*;
    #[test]
    fn inherited_policy_matches_original_classification_for_nested_and_sensitive_paths() {
        let parts = [
            "a",
            "Library",
            "Caches",
            "Downloads",
            ".next",
            "cache",
            "logs",
            "memory",
            "sessions",
            "node_modules",
            ".ENV.local",
            "x.secret",
            "秘密",
            "Demo.app",
            ".git",
            "models",
            "__pycache__",
            "Cache",
        ];
        let names = [
            "a.bin",
            "run.jsonl",
            "run.jsonl.gz",
            "run.ndjson.zst",
            "x.log",
            "x.log.1",
            "AGENTS.md",
            "SOUL.md",
            "MEMORY.md",
            "disk.DMG",
            "app.PkG",
            "installer.PKG",
            "auth.json",
            "cache",
            "sessions",
            ".env",
            "keyblob",
            "secret.json",
        ];
        for a in parts {
            for b in parts {
                for name in names {
                    for tool in [false, true] {
                        let parent = Path::new("/Users/example").join(a).join(b);
                        let path = parent.join(name);
                        assert_eq!(
                            PathPolicy::at(&parent).category(name, tool),
                            reference_category(&path, tool),
                            "{} tool={tool}",
                            path.display()
                        );
                    }
                }
            }
        }
    }
}

// A nested subtree scan must not replace the outer full-scan counters.
thread_local! {static QUIET_PROGRESS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };}
pub(crate) fn print_event(value: serde_json::Value) {
    if QUIET_PROGRESS.with(|v| v.get() == 0) {
        println!("{value}");
    }
}
pub(crate) fn without_progress<T>(run: impl FnOnce() -> T) -> T {
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            QUIET_PROGRESS.with(|v| v.set(v.get() - 1));
        }
    }
    QUIET_PROGRESS.with(|v| v.set(v.get() + 1));
    let _guard = Guard;
    run()
}
