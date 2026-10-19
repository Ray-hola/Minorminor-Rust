//! Mutable preferences and cleanup authority live apart from immutable indexes.
use crate::{fail, Result};
use rusqlite::{Connection, OptionalExtension};
use std::{
    fs,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
};

pub const FILE: &str = "user-state.sqlite3";
pub const TABLES: [&str; 6] = [
    "settings",
    "receipts",
    "plans",
    "plan_files",
    "reviews",
    "review_nodes",
];

pub fn attach(db: &Connection, path: &Path) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    if db.path().is_none_or(|p| p.is_empty()) {
        return Ok(());
    }
    let state = parent.join(FILE);
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&state)?;
    if !file.metadata()?.is_file() {
        return Err(fail("设置库不是普通文件"));
    }
    fs::set_permissions(&state, fs::Permissions::from_mode(0o600))?;
    let mut uri = reqwest::Url::from_file_path(fs::canonicalize(&state)?)
        .map_err(|_| fail("设置库路径无效"))?;
    uri.set_query(Some("mode=rwc"));
    db.execute("ATTACH DATABASE ? AS user_state", [uri.as_str()])?;
    if db.query_row("PRAGMA user_state.page_count", [], |r| r.get::<_, u64>(0))? == 0 {
        db.execute_batch("PRAGMA user_state.auto_vacuum=INCREMENTAL")?;
    }
    db.execute_batch("PRAGMA user_state.journal_mode=WAL; PRAGMA user_state.synchronous=FULL; PRAGMA user_state.cache_size=-4096; PRAGMA user_state.journal_size_limit=8388608;")?;
    Ok(())
}

pub fn schema(db: &Connection) -> Result<&'static str> {
    let attached: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_database_list WHERE name='user_state')",
        [],
        |r| r.get(0),
    )?;
    Ok(if attached { "user_state" } else { "main" })
}

pub fn initialize(db: &Connection) -> Result<()> {
    let s = schema(db)?;
    db.execute_batch(&format!(r#"
      CREATE TABLE IF NOT EXISTS {s}.settings(key TEXT PRIMARY KEY,value TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS {s}.receipts(job TEXT NOT NULL,created TEXT NOT NULL,path TEXT NOT NULL,target TEXT,status TEXT NOT NULL,bytes INTEGER NOT NULL,reason TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS {s}.plans(id TEXT PRIMARY KEY,scan TEXT NOT NULL,created TEXT NOT NULL,expires INTEGER NOT NULL,used INTEGER NOT NULL DEFAULT 0,personal INTEGER NOT NULL);
      CREATE TABLE IF NOT EXISTS {s}.plan_files(plan TEXT NOT NULL,id TEXT NOT NULL,path TEXT NOT NULL,root TEXT NOT NULL,category TEXT NOT NULL,bytes INTEGER NOT NULL,identity TEXT NOT NULL,parent_identity TEXT NOT NULL,PRIMARY KEY(plan,id));
      CREATE TABLE IF NOT EXISTS {s}.reviews(id TEXT PRIMARY KEY,created TEXT NOT NULL,expires INTEGER NOT NULL,used INTEGER NOT NULL DEFAULT 0,preview TEXT NOT NULL,groups_json TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS {s}.review_nodes(plan TEXT NOT NULL,id TEXT NOT NULL,group_id TEXT NOT NULL,data TEXT NOT NULL,PRIMARY KEY(plan,id));
    "#))?;
    crate::overlay::initialize(db)?;
    if s == "user_state" {
        migrate(db, || Ok(()))?;
    }
    Ok(())
}

// Commit the new copy first. A crash before the main-table transaction commits
// leaves the old tables authoritative; startup can repeat the copy safely.
pub fn migrate(db: &Connection, mut copied: impl FnMut() -> Result<()>) -> Result<()> {
    let mut tables = Vec::new();
    for table in TABLES {
        let kind: Option<String> = db
            .query_row(
                "SELECT type FROM main.sqlite_master WHERE name=?",
                [table],
                |r| r.get(0),
            )
            .optional()?;
        match kind.as_deref() {
            Some("table") => tables.push(table),
            None => {}
            _ => return Err(fail("旧设置库结构不受支持，原数据已保留")),
        }
    }
    if tables.is_empty() {
        return Ok(());
    }
    db.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| -> Result<()> {
        for table in &tables {
            let columns = db
                .prepare(&format!("PRAGMA main.table_info({table})"))?
                .query_map([], |r| r.get::<_, String>(1))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let columns = columns
                .iter()
                .map(|n| format!("\"{}\"", n.replace('"', "\"\"")))
                .collect::<Vec<_>>()
                .join(",");
            db.execute(&format!("DELETE FROM user_state.{table}"), [])?;
            db.execute(&format!("INSERT INTO user_state.{table}(rowid,{columns}) SELECT rowid,{columns} FROM main.{table}"),[])?;
            let different:bool=db.query_row(&format!("SELECT EXISTS(SELECT rowid,{columns} FROM main.{table} EXCEPT SELECT rowid,{columns} FROM user_state.{table}) OR EXISTS(SELECT rowid,{columns} FROM user_state.{table} EXCEPT SELECT rowid,{columns} FROM main.{table})"),[],|r|r.get(0))?;
            if different {
                return Err(fail("设置库迁移核对失败，原数据已保留"));
            }
        }
        Ok(())
    })();
    if let Err(e) = result {
        let _ = db.execute_batch("ROLLBACK");
        return Err(e);
    }
    db.execute_batch("COMMIT")?;
    let (busy, _, _): (i64, i64, i64) =
        db.query_row("PRAGMA user_state.wal_checkpoint(TRUNCATE)", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?;
    if busy != 0 {
        return Err(fail("设置库仍被占用，原数据已保留"));
    }
    copied()?;
    db.execute_batch("PRAGMA main.secure_delete=ON; BEGIN IMMEDIATE")?;
    let result = (|| -> Result<()> {
        for table in &tables {
            db.execute_batch(&format!("DROP TABLE main.{table}"))?;
        }
        Ok(())
    })();
    if let Err(e) = result {
        let _ = db.execute_batch("ROLLBACK");
        return Err(e);
    }
    db.execute_batch("COMMIT")?;
    Ok(())
}
