use crate::{fail, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

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
    let db = Connection::open(path)?;
    db.busy_timeout(std::time::Duration::from_secs(10))?;
    if db.query_row("PRAGMA page_count", [], |r| r.get::<_, u64>(0))? == 0 {
        db.execute_batch("PRAGMA auto_vacuum=INCREMENTAL")?;
    }
    db.execute_batch("PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA wal_autocheckpoint=512; PRAGMA journal_size_limit=8388608; PRAGMA cache_size=-16384;")?;
    Ok(db)
}
pub fn initialize(data: &Path) -> Result<PathBuf> {
    secure_directory(data)?;
    let path = data.join("index.sqlite3");
    let _ = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&path)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    let db = connect(&path)?;
    db.execute_batch("
      CREATE TABLE IF NOT EXISTS scans(id TEXT PRIMARY KEY, root TEXT NOT NULL, created TEXT NOT NULL, status TEXT NOT NULL, stats TEXT NOT NULL DEFAULT '{}');
      CREATE TABLE IF NOT EXISTS projects(scan TEXT NOT NULL,id TEXT NOT NULL,path TEXT NOT NULL,name TEXT NOT NULL,markers TEXT NOT NULL, PRIMARY KEY(scan,id));
      CREATE TABLE IF NOT EXISTS tools(scan TEXT NOT NULL,id TEXT NOT NULL,path TEXT NOT NULL,name TEXT NOT NULL,evidence TEXT NOT NULL, uncertain INTEGER NOT NULL, PRIMARY KEY(scan,id));
      CREATE TABLE IF NOT EXISTS associations(scan TEXT NOT NULL,id TEXT NOT NULL,path TEXT NOT NULL,indexed_path TEXT NOT NULL,project TEXT,tool TEXT NOT NULL,kind TEXT NOT NULL,title TEXT NOT NULL,status TEXT NOT NULL,reason TEXT NOT NULL,managed INTEGER NOT NULL, PRIMARY KEY(scan,id));
      CREATE INDEX IF NOT EXISTS associations_path ON associations(scan,path);
      CREATE INDEX IF NOT EXISTS associations_tool_path ON associations(scan,tool,path);
      CREATE TABLE IF NOT EXISTS errors(scan TEXT NOT NULL,path TEXT NOT NULL,code TEXT NOT NULL,reason TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS plans(id TEXT PRIMARY KEY,scan TEXT NOT NULL,created TEXT NOT NULL,expires INTEGER NOT NULL,used INTEGER NOT NULL DEFAULT 0,personal INTEGER NOT NULL);
      CREATE TABLE IF NOT EXISTS plan_files(plan TEXT NOT NULL,id TEXT NOT NULL,path TEXT NOT NULL,root TEXT NOT NULL,category TEXT NOT NULL,bytes INTEGER NOT NULL,identity TEXT NOT NULL,parent_identity TEXT NOT NULL, PRIMARY KEY(plan,id));
      CREATE TABLE IF NOT EXISTS receipts(job TEXT NOT NULL,created TEXT NOT NULL,path TEXT NOT NULL,target TEXT,status TEXT NOT NULL,bytes INTEGER NOT NULL,reason TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS settings(key TEXT PRIMARY KEY,value TEXT NOT NULL);
    ")?;
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
    db.execute(
        "UPDATE scans SET status='interrupted' WHERE status IN ('running','index-complete')",
        [],
    )?;
    crate::review::initialize(&db)?;
    db.execute("DELETE FROM review_nodes", [])?;
    db.execute("DELETE FROM reviews", [])?;
    db.execute(
        "DELETE FROM settings WHERE key LIKE 'advice-input-%' OR key='mole-preview'",
        [],
    )?;
    db.execute("DELETE FROM plan_files", [])?;
    db.execute("DELETE FROM plans", [])?;
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

#[cfg(test)]
mod scan_index_tests {
    use super::*;

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
            ("entries_hardlinks", "SELECT path,identity,bytes FROM entries WHERE scan=? AND directory=0 AND json_extract(identity,'$.nlink')>1", 2),
        ] {
            let read = |sql: &str| {
                let mut stmt = db.prepare(sql).unwrap();
                let rows = stmt.query_map(["scan"], |r| r.get::<_,String>(0)).unwrap();
                rows.collect::<std::result::Result<Vec<_>,_>>().unwrap()
            };
            let mut actual = read(query);
            let mut expected = read(&query.replace("FROM entries WHERE", "FROM entries NOT INDEXED WHERE").replace("INDEXED BY entries_directories", "NOT INDEXED"));
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
pub fn scan_info(db: &Connection, scan: &str) -> Result<Value> {
    let (root, created, status, stats): (String, String, String, String) = db.query_row(
        "SELECT root,created,status,stats FROM scans WHERE id=?",
        [scan],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    Ok(
        json!({"id":scan,"root":root,"createdAt":created,"status":status,"stats":serde_json::from_str::<Value>(&stats)?}),
    )
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
