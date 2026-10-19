use crate::{fail, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

pub const FILE: &str = "index-v4.sqlite3";
pub fn path(data: &Path) -> PathBuf {
    data.join(FILE)
}
// Keep the new split store separate from files an older binary may recreate.
// In particular an old empty main.settings must never replace retained history.
fn migrate_filename(data: &Path) -> Result<()> {
    let current = path(data);
    let legacy = data.join("index.sqlite3");
    if current.try_exists()? || !legacy.try_exists()? {
        return Ok(());
    }
    let metadata = fs::symlink_metadata(&legacy)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(fail("旧索引路径无效，原文件已保留"));
    }
    let compressed = crate::fs_compression::is_compressed(&legacy);
    if compressed {
        if fs::metadata(data.join("index.sqlite3-wal")).is_ok_and(|m| m.len() > 0) {
            return Err(fail("旧索引仍在使用，请关闭旧版后重试"));
        }
    } else {
        let db = Connection::open(&legacy)?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        let tables: u64 = db.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get(0))?;
        let recognized: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='scans' AND type='table')",
            [],
            |r| r.get(0),
        )?;
        if tables > 0 && !recognized {
            return Err(fail("旧文件不是可识别的扫描索引，原文件已保留"));
        }
        let (busy, _, _): (i64, i64, i64) =
            db.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?;
        let mode: String = db.query_row("PRAGMA journal_mode=DELETE", [], |r| r.get(0))?;
        if busy != 0 || mode != "delete" {
            return Err(fail("旧索引仍在使用，请关闭旧版后重试"));
        }
    }
    fs::rename(&legacy, &current)?;
    fs::File::open(data)?.sync_all()?;
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Identity {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub mtime: i64,
    pub mtime_ns: i64,
    pub ctime: i64,
    pub ctime_ns: i64,
    pub nlink: u64,
}
impl Identity {
    pub fn of(m: &fs::Metadata) -> Self {
        Self {
            dev: m.dev(),
            ino: m.ino(),
            size: m.len(),
            mtime: m.mtime(),
            mtime_ns: m.mtime_nsec(),
            ctime: m.ctime(),
            ctime_ns: m.ctime_nsec(),
            nlink: m.nlink(),
        }
    }
}
pub fn secure_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    if fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(fail("数据目录不能是符号链接"));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}
pub fn connect(path: &Path) -> Result<Connection> {
    connect_mode(path, false)
}
pub fn connect_readonly(path: &Path) -> Result<Connection> {
    connect_mode(path, true)
}
fn connect_mode(path: &Path, readonly: bool) -> Result<Connection> {
    let compressed = crate::fs_compression::is_compressed(path);
    let readonly = readonly || compressed;
    // Keep the connection's attach capability read/write while selecting a
    // genuinely read-only main file through its URI. Attached state stays writable.
    let db = if readonly {
        let mut uri = reqwest::Url::from_file_path(fs::canonicalize(path)?)
            .map_err(|_| fail("索引路径无效"))?;
        let wal_active = fs::metadata(format!("{}-wal", path.display())).is_ok_and(|m| m.len() > 0);
        uri.set_query(Some(if compressed && !wal_active {
            "mode=ro&immutable=1"
        } else {
            "mode=ro"
        }));
        Connection::open_with_flags(
            uri.as_str(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
                | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )?
    } else {
        Connection::open(path)?
    };
    db.busy_timeout(std::time::Duration::from_secs(10))?;
    if !readonly && db.query_row("PRAGMA page_count", [], |r| r.get::<_, u64>(0))? == 0 {
        db.execute_batch("PRAGMA auto_vacuum=INCREMENTAL")?;
    }
    db.execute_batch("PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL; PRAGMA wal_autocheckpoint=512; PRAGMA journal_size_limit=8388608; PRAGMA cache_size=-16384;")?;
    if !readonly {
        db.execute_batch("PRAGMA journal_mode=WAL")?;
    }
    crate::user_state::attach(&db, path)?;
    if compressed {
        crate::overlay::install_if_needed(&db)?;
    }
    Ok(db)
}
pub fn initialize(data: &Path) -> Result<PathBuf> {
    secure_directory(data)?;
    migrate_filename(data)?;
    initialize_at(data, &path(data), true)
}
pub fn initialize_working(data: &Path, path: &Path) -> Result<PathBuf> {
    initialize_at(data, path, false)
}
fn initialize_at(data: &Path, index_path: &Path, startup: bool) -> Result<PathBuf> {
    secure_directory(data)?;
    let path = index_path.to_path_buf();
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {}
        Ok(_) => return Err(fail("索引不是普通文件或路径是链接")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(&path)?;
        }
        Err(e) => return Err(e.into()),
    }
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    let db = connect(&path)?;
    db.execute_batch("
      CREATE TABLE IF NOT EXISTS scans(id TEXT PRIMARY KEY, root TEXT NOT NULL, created TEXT NOT NULL, status TEXT NOT NULL, stats TEXT NOT NULL DEFAULT '{}');
      CREATE TABLE IF NOT EXISTS projects(scan TEXT NOT NULL,id TEXT NOT NULL,path TEXT NOT NULL,name TEXT NOT NULL,markers TEXT NOT NULL, PRIMARY KEY(scan,id));
      CREATE TABLE IF NOT EXISTS tools(scan TEXT NOT NULL,id TEXT NOT NULL,path TEXT NOT NULL,name TEXT NOT NULL,evidence TEXT NOT NULL, uncertain INTEGER NOT NULL, PRIMARY KEY(scan,id));
      CREATE TABLE IF NOT EXISTS associations(scan TEXT NOT NULL,id TEXT NOT NULL,path TEXT NOT NULL,indexed_path TEXT NOT NULL,project TEXT,tool TEXT NOT NULL,kind TEXT NOT NULL,title TEXT NOT NULL,status TEXT NOT NULL,reason TEXT NOT NULL,managed INTEGER NOT NULL, PRIMARY KEY(scan,id));
      CREATE INDEX IF NOT EXISTS associations_path ON associations(scan,path);
      CREATE INDEX IF NOT EXISTS associations_tool_path ON associations(scan,tool,path);
      CREATE INDEX IF NOT EXISTS associations_project ON associations(scan,project,kind);
      CREATE TABLE IF NOT EXISTS errors(scan TEXT NOT NULL,path TEXT NOT NULL,code TEXT NOT NULL,reason TEXT NOT NULL);
    ")?;
    crate::user_state::initialize(&db)?;
    crate::compact::initialize(&db)?;
    let has_skipped: bool = {
        let mut stmt = db.prepare("PRAGMA table_info(entries)")?;
        let columns = stmt
            .query_map([], |r| r.get::<_, String>(1))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        columns.iter().any(|s| s == "skipped")
    };
    if !has_skipped {
        db.execute_batch("ALTER TABLE entries ADD COLUMN skipped INTEGER NOT NULL DEFAULT 0")?;
    }
    let unfinished: bool = startup
        && db.query_row(
            "SELECT EXISTS(SELECT 1 FROM scans WHERE status IN ('running','index-complete'))",
            [],
            |r| r.get(0),
        )?;
    if unfinished {
        db.execute(
            "UPDATE scans SET status='interrupted' WHERE status IN ('running','index-complete')",
            [],
        )?;
    }
    crate::review::initialize(&db)?;
    if startup {
        db.execute("DELETE FROM review_nodes", [])?;
        db.execute("DELETE FROM reviews", [])?;
        db.execute(
            "DELETE FROM settings WHERE key LIKE 'advice-input-%' OR key='mole-preview'",
            [],
        )?;
        db.execute("DELETE FROM plan_files", [])?;
        db.execute("DELETE FROM plans", [])?;
    }
    Ok(path)
}
pub fn latest(db: &Connection) -> Result<Option<String>> {
    Ok(db
        .query_row(
            "SELECT id FROM scans WHERE status='complete' ORDER BY created DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()?)
}

// Keep SQLite's bounded-analysis bit (0x10) when asking it to inspect all tables.
// Query-planner sampling never changes the scope or contents of a disk scan.
pub const OPTIMIZE_SQL: &str = "PRAGMA optimize=0x10012";
pub fn optimize(db: &Connection) -> Result<()> {
    // Planner statistics are rebuildable hints. Bound their wall time as well
    // as samples; filesystem traversal and its durable rows remain unbounded.
    let interrupt = db.get_interrupt_handle();
    let (stop, wait) = std::sync::mpsc::sync_channel::<()>(1);
    let timer = std::thread::spawn(move || {
        if matches!(
            wait.recv_timeout(std::time::Duration::from_secs(3)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ) {
            interrupt.interrupt();
        }
    });
    let result = db.execute_batch(OPTIMIZE_SQL);
    let _ = stop.send(());
    let _ = timer.join();
    match result {
        Err(rusqlite::Error::SqliteFailure(error, _))
            if error.code == rusqlite::ErrorCode::OperationInterrupted =>
        {
            Ok(())
        }
        other => {
            other?;
            Ok(())
        }
    }
}

pub fn prepare_scan_indexes(
    db: &Connection,
    mut progress: impl FnMut(&str) -> Result<()>,
) -> Result<()> {
    if !crate::compact::is_legacy(db)? {
        return crate::compact::indexes(db, progress);
    }
    // Migrate large indexes in the visible scan worker, not before its window opens.
    for (name, sql) in [
        ("installations", "CREATE INDEX IF NOT EXISTS entries_installations ON entries(scan,path) WHERE directory=0 AND (name IN ('package.json','app.asar','Info.plist','METADATA','PKG-INFO') OR parent LIKE '%/bin')"),
        ("memory", "CREATE INDEX IF NOT EXISTS entries_memory ON entries(scan,tool,path) WHERE directory=0 AND category='memory' AND tool IS NOT NULL"),
        ("directories", "CREATE INDEX IF NOT EXISTS entries_directories ON entries(scan,path) WHERE directory=1"),
        ("hardlinks", "CREATE INDEX IF NOT EXISTS entries_hardlinks ON entries(scan,path) WHERE directory=0 AND json_extract(identity,'$.nlink')>1"),
    ] {
        progress(name)?;
        db.execute_batch(sql)?;
    }
    Ok(())
}

const PENDING_PROJECTS: &str = "SELECT path FROM _entries WHERE scan=(SELECT id FROM entry_scans WHERE value=?1) AND project IS NULL AND path>?2 AND path<?3 ORDER BY path LIMIT 2000";

// Advance through a bounded path range once. Updating through the compatibility
// view can make SQLite enumerate every parent directory for each batch.
pub fn assign_project_batch(
    db: &Connection,
    scan: &str,
    project: &str,
    cursor: &str,
    upper: &str,
) -> Result<Option<(String, usize)>> {
    let legacy = crate::compact::is_legacy(db)?;
    let select = if legacy {
        "SELECT path FROM entries WHERE scan=?1 AND project IS NULL AND path>?2 AND path<?3 ORDER BY path LIMIT 2000"
    } else {
        PENDING_PROJECTS
    };
    let paths = db
        .prepare_cached(select)?
        .query_map(params![scan, cursor, upper], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let Some(last) = paths.last() else {
        return Ok(None);
    };
    let update = if legacy {
        "UPDATE entries SET project=?1 WHERE scan=?2 AND project IS NULL AND path>?3 AND path<=?4"
    } else {
        "UPDATE _entries SET project=?1 WHERE scan=(SELECT id FROM entry_scans WHERE value=?2) AND project IS NULL AND path>?3 AND path<=?4"
    };
    let changed = db
        .prepare_cached(update)?
        .execute(params![project, scan, cursor, last])?;
    Ok(Some((last.clone(), changed)))
}

#[cfg(test)]
mod scan_index_tests {
    use super::*;

    #[test]
    fn project_batches_keep_nested_owners_boundaries_metadata_and_other_scans() {
        for legacy in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let db = if legacy {
                let db = Connection::open_in_memory().unwrap();
                db.execute_batch("CREATE TABLE entries(scan TEXT,path TEXT,parent TEXT,name TEXT,directory INTEGER,bytes INTEGER,logical INTEGER,unique_bytes INTEGER,identity TEXT,category TEXT,project TEXT,tool TEXT,PRIMARY KEY(scan,path));").unwrap();
                db
            } else {
                connect(&initialize(temp.path()).unwrap()).unwrap()
            };
            db.execute_batch("BEGIN").unwrap();
            for scan in ["current", "previous"] {
                for n in 0..7505 {
                    let path = format!("/scope/project/group-{:02}/file-{n:05}", n % 64);
                    let owner = if scan == "current" && n % 3 == 0 {
                        Some("nested")
                    } else {
                        None
                    };
                    db.execute("INSERT INTO entries(scan,path,parent,name,directory,bytes,logical,unique_bytes,identity,category,project,tool) VALUES(?,?,?,'file',?,8,7,8,?,'cache',?,'tool')",params![scan,path,format!("/scope/project/group-{:02}",n%64),n%4==0,format!("{{\"test\":{n}}}"),owner]).unwrap();
                }
            }
            for path in ["/scope/project", "/scope/project-other/file"] {
                db.execute("INSERT INTO entries(scan,path,parent,name,directory,bytes,logical,unique_bytes,identity,category) VALUES('current',?,'/scope','boundary',0,0,0,0,'{}','other')",[path]).unwrap();
            }
            db.execute_batch("COMMIT").unwrap();
            let snapshot = |db: &Connection| {
                db.prepare("SELECT scan,path,identity,bytes,logical,category,tool,directory FROM entries ORDER BY scan,path").unwrap().query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,i64>(3)?,r.get::<_,i64>(4)?,r.get::<_,String>(5)?,r.get::<_,Option<String>>(6)?,r.get::<_,i64>(7)?))).unwrap().collect::<std::result::Result<Vec<_>,_>>().unwrap()
            };
            let before = snapshot(&db);
            let mut cursor = "/scope/project/".to_owned();
            let mut batches = 0;
            let mut assigned = 0;
            while let Some((last, changed)) =
                assign_project_batch(&db, "current", "parent", &cursor, "/scope/project0").unwrap()
            {
                assert!(last > cursor);
                assert!(changed <= 2000);
                cursor = last;
                assigned += changed;
                batches += 1;
            }
            assert_eq!(batches, 3);
            assert_eq!(assigned, 5003);
            assert_eq!(snapshot(&db), before);
            for (scan, path, owner) in db
                .prepare("SELECT scan,path,project FROM entries")
                .unwrap()
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                    ))
                })
                .unwrap()
                .map(|r| r.unwrap())
            {
                if scan == "previous" || !path.starts_with("/scope/project/") {
                    assert!(owner.is_none());
                } else {
                    let n = path.rsplit('-').next().unwrap().parse::<usize>().unwrap();
                    assert_eq!(
                        owner.as_deref(),
                        Some(if n % 3 == 0 { "nested" } else { "parent" })
                    );
                }
            }
            if !legacy {
                let plan = db
                    .prepare(&format!("EXPLAIN QUERY PLAN {PENDING_PROJECTS}"))
                    .unwrap()
                    .query_map(
                        params!["current", "/scope/project/", "/scope/project0"],
                        |r| r.get::<_, String>(3),
                    )
                    .unwrap()
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .unwrap();
                assert!(
                    plan.iter()
                        .any(|r| r.contains("INDEX sqlite_autoindex__entries_1")
                            && r.contains("scan=? AND path>? AND path<?")),
                    "{plan:?}"
                );
                assert!(
                    !plan
                        .iter()
                        .any(|r| r.contains("entries_parent") || r.contains("SCAN p")),
                    "{plan:?}"
                );
            }
        }
    }

    #[test]
    fn scan_indexes_keep_memory_directories_and_all_hardlinks() {
        let temp = tempfile::tempdir().unwrap();
        let file = initialize(temp.path()).unwrap();
        let db = connect(&file).unwrap();
        for (path, directory, kind, tool, links) in [
            ("/", true, "other", None, 1),
            ("/a", true, "other", None, 1),
            ("/a/long-nested-directory", true, "other", None, 1),
            ("/z", true, "other", None, 1),
            ("/a/memory.md", false, "memory", Some("tool"), 1),
            ("/a/other-memory.md", false, "memory", None, 1),
            ("/a/link-one", false, "cache", None, 2),
            ("/z/link-two", false, "cache", None, 2),
            ("/z/file", false, "other", None, 1),
        ] {
            db.execute("INSERT INTO entries(scan,path,parent,name,directory,bytes,logical,unique_bytes,identity,category,tool) VALUES('scan',?,'/','name',?,0,0,0,?,?,?)", params![path,directory,json!({"nlink":links}).to_string(),kind,tool]).unwrap();
        }
        let mut built = Vec::new();
        prepare_scan_indexes(&db, |name| {
            built.push(name.to_owned());
            Ok(())
        })
        .unwrap();
        assert_eq!(built.len(), 7);
        db.execute_batch("ANALYZE").unwrap();
        for (index, query, count) in [
            ("entries_memory", "SELECT path,tool FROM entries WHERE scan=? AND directory=0 AND category='memory' AND tool IS NOT NULL", 1),
            ("entries_directories", crate::compact::DIRECTORIES, 4),
            ("entries_hardlinks", crate::compact::HARDLINKS, 2),
        ] {
            let read = |sql: &str| {
                let mut stmt = db.prepare(sql).unwrap();
                let rows = stmt.query_map(["scan"], |r| r.get::<_,String>(0)).unwrap();
                rows.collect::<std::result::Result<Vec<_>,_>>().unwrap()
            };
            let mut actual = read(query);
            let mut expected = read(&query.replace("FROM entries WHERE", "FROM entries NOT INDEXED WHERE").replace("INDEXED BY entries_directories", "NOT INDEXED").replace("INDEXED BY entries_hardlinks", "NOT INDEXED"));
            assert_eq!(actual.len(),count);
            actual.sort();expected.sort();assert_eq!(actual,expected);
            let mut stmt = db.prepare(&format!("EXPLAIN QUERY PLAN {query}")).unwrap();
            let plan = stmt.query_map(["scan"], |r| r.get::<_,String>(3)).unwrap().collect::<std::result::Result<Vec<_>,_>>().unwrap();
            assert!(plan.iter().any(|line| line.contains(index)), "{index}: {plan:?}");
        }
    }
}
pub fn prune(db: &Connection) -> Result<()> {
    prune_with_progress(db, |_, _| Ok(()))
}
pub fn prune_with_progress(
    db: &Connection,
    mut progress: impl FnMut(&str, u64) -> Result<()>,
) -> Result<()> {
    let keep = latest(db)?.unwrap_or_default();
    let mut removed = 0;
    // Commit bounded batches so interrupted scans cannot require a second
    // multi-gigabyte rollback journal before a new scan can even begin.
    for table in ["entries", "projects", "tools", "associations", "errors"] {
        progress(table, removed)?;
        loop {
            let changed = if table == "entries" && !crate::compact::is_legacy(db)? {
                db.execute("DELETE FROM _entries WHERE (scan,path) IN (SELECT e.scan,e.path FROM _entries e JOIN entry_scans s ON s.id=e.scan WHERE s.value IN (SELECT id FROM scans WHERE id<>?) LIMIT 1000)",[&keep])?
            } else {
                db.execute(&format!("DELETE FROM {table} WHERE rowid IN (SELECT rowid FROM {table} WHERE scan IN (SELECT id FROM scans WHERE id<>?) LIMIT 1000)"),[&keep])?
            };
            crate::storage::checkpoint(db)?;
            removed += changed as u64;
            progress(table, removed)?;
            if changed < 1000 {
                break;
            }
        }
    }
    db.execute("DELETE FROM scans WHERE id<>?", [&keep])?;
    if !crate::compact::is_legacy(db)? {
        crate::compact::prune_directories(db, &mut progress, removed)?;
    }
    db.execute("DELETE FROM plan_files", [])?;
    db.execute("DELETE FROM plans", [])?;
    Ok(())
}
pub fn assert_scan(db: &Connection, scan: &str) -> Result<()> {
    if latest(db)?.as_deref() != Some(scan) {
        return Err(fail("扫描快照已变化，请刷新后重新预览"));
    }
    Ok(())
}
pub fn setting(db: &Connection, key: &str) -> Result<Option<Value>> {
    let data: Option<String> = db
        .query_row("SELECT value FROM settings WHERE key=?", [key], |r| {
            r.get(0)
        })
        .optional()?;
    Ok(data.and_then(|s| serde_json::from_str(&s).ok()))
}
pub fn save_setting(db: &Connection, key: &str, value: &Value) -> Result<()> {
    db.execute(
        "INSERT OR REPLACE INTO settings(key,value) VALUES(?,?)",
        params![key, value.to_string()],
    )?;
    Ok(())
}
pub fn invalidate_storage_summaries(db: &Connection) -> Result<()> {
    let revision = setting(db, "storage-summary-revision")?
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
        .saturating_add(1);
    save_setting(db, "storage-summary-revision", &serde_json::json!(revision))?;
    db.execute(
        "DELETE FROM settings WHERE key LIKE 'project-storage-cache-%' OR key LIKE 'tool-storage-cache-%'",
        [],
    )?;
    Ok(())
}
pub fn scan_info(db: &Connection, scan: &str) -> Result<Value> {
    let (root, created, status, stats): (String, String, String, String) = db.query_row(
        "SELECT root,created,status,stats FROM scans WHERE id=?",
        [scan],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    let mut stats: Value = serde_json::from_str(&stats)?;
    if let Some(duration) =
        setting(db, "scan-duration")?.filter(|v| v["id"] == scan && v["elapsedMs"].is_u64())
    {
        stats["elapsedMs"] = duration["elapsedMs"].clone();
    }
    Ok(json!({"id":scan,"root":root,"createdAt":created,"status":status,"stats":stats}))
}
pub fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}
pub fn checked(path: &Path, root: &Path, device: u64) -> Result<fs::Metadata> {
    if !path.starts_with(root) {
        return Err(fail("路径超出已授权范围"));
    }
    let m = fs::symlink_metadata(path)?;
    if m.file_type().is_symlink() || m.dev() != device || fs::canonicalize(path)? != path {
        return Err(fail("符号链接、跨卷或路径变化，已保留"));
    }
    if !m.is_file() && !m.is_dir() {
        return Err(fail("特殊文件不可操作"));
    }
    Ok(m)
}

// Cache keys contain a scan ID; publishing a new snapshot must retain its
// already computed summaries instead of invalidating every result again.
pub fn prune_scan_caches(db: &Connection, scan: &str) -> Result<()> {
    for base in [
        "project-storage-cache",
        "tool-storage-cache",
        "project-info",
        "memory-info",
    ] {
        let prefix = format!("{base}-{scan}-");
        db.execute(
            "DELETE FROM settings WHERE substr(key,1,?1)=?2 AND substr(key,1,?3)<>?4",
            rusqlite::params![base.len() + 1, format!("{base}-"), prefix.len(), prefix],
        )?;
    }
    db.execute(
        "DELETE FROM settings WHERE key LIKE 'read-recovery-%' AND key<>?",
        [format!("read-recovery-{scan}")],
    )?;
    let keep = format!("discovery-coverage-{scan}");
    db.execute(
        "DELETE FROM settings WHERE key LIKE 'discovery-coverage-%' AND key<>?",
        [keep],
    )?;
    Ok(())
}
