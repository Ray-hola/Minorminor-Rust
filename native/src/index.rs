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
    db.execute_batch("PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")?;
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
      CREATE TABLE IF NOT EXISTS entries(scan TEXT NOT NULL, path TEXT NOT NULL, parent TEXT NOT NULL, name TEXT NOT NULL, directory INTEGER NOT NULL,
        bytes INTEGER NOT NULL, logical INTEGER NOT NULL, unique_bytes INTEGER NOT NULL, files INTEGER NOT NULL DEFAULT 0, dirs INTEGER NOT NULL DEFAULT 0,
        identity TEXT NOT NULL, category TEXT NOT NULL, project TEXT, tool TEXT, skipped INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(scan,path));
      CREATE INDEX IF NOT EXISTS entries_parent ON entries(scan,parent);
      CREATE INDEX IF NOT EXISTS entries_project ON entries(scan,project,category);
      CREATE INDEX IF NOT EXISTS entries_tool ON entries(scan,tool);
      CREATE TABLE IF NOT EXISTS projects(scan TEXT NOT NULL,id TEXT NOT NULL,path TEXT NOT NULL,name TEXT NOT NULL,markers TEXT NOT NULL, PRIMARY KEY(scan,id));
      CREATE TABLE IF NOT EXISTS tools(scan TEXT NOT NULL,id TEXT NOT NULL,path TEXT NOT NULL,name TEXT NOT NULL,evidence TEXT NOT NULL, uncertain INTEGER NOT NULL, PRIMARY KEY(scan,id));
      CREATE TABLE IF NOT EXISTS associations(scan TEXT NOT NULL,id TEXT NOT NULL,path TEXT NOT NULL,indexed_path TEXT NOT NULL,project TEXT,tool TEXT NOT NULL,kind TEXT NOT NULL,title TEXT NOT NULL,status TEXT NOT NULL,reason TEXT NOT NULL,managed INTEGER NOT NULL, PRIMARY KEY(scan,id));
      CREATE TABLE IF NOT EXISTS errors(scan TEXT NOT NULL,path TEXT NOT NULL,code TEXT NOT NULL,reason TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS plans(id TEXT PRIMARY KEY,scan TEXT NOT NULL,created TEXT NOT NULL,expires INTEGER NOT NULL,used INTEGER NOT NULL DEFAULT 0,personal INTEGER NOT NULL);
      CREATE TABLE IF NOT EXISTS plan_files(plan TEXT NOT NULL,id TEXT NOT NULL,path TEXT NOT NULL,root TEXT NOT NULL,category TEXT NOT NULL,bytes INTEGER NOT NULL,identity TEXT NOT NULL,parent_identity TEXT NOT NULL, PRIMARY KEY(plan,id));
      CREATE TABLE IF NOT EXISTS receipts(job TEXT NOT NULL,created TEXT NOT NULL,path TEXT NOT NULL,target TEXT,status TEXT NOT NULL,bytes INTEGER NOT NULL,reason TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS settings(key TEXT PRIMARY KEY,value TEXT NOT NULL);
    ")?;
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
pub fn prune(db: &Connection) -> Result<()> {
    let keep = latest(db)?.unwrap_or_default();
    db.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| -> Result<()> {
        for table in ["entries", "projects", "tools", "associations", "errors"] {
            db.execute(&format!("DELETE FROM {table} WHERE scan<>?"), [&keep])?;
        }
        db.execute("DELETE FROM scans WHERE id<>?", [&keep])?;
        db.execute("DELETE FROM plan_files", [])?;
        db.execute("DELETE FROM plans", [])?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            db.execute_batch("COMMIT")?;
            Ok(())
        }
        Err(error) => {
            let _ = db.execute_batch("ROLLBACK");
            Err(error)
        }
    }
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
