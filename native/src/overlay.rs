//! Small partial rescans update a transactional delta, keeping the compressed
//! full snapshot intact. A full scan supersedes the delta atomically by scan ID.
use crate::{user_state, Result};
use rusqlite::Connection;
pub fn initialize(db: &Connection) -> Result<()> {
    let s = user_state::schema(db)?;
    db.execute_batch(&format!("CREATE TABLE IF NOT EXISTS {s}.entry_changes(scan TEXT NOT NULL,path TEXT NOT NULL,parent TEXT,name TEXT,directory INTEGER,bytes INTEGER,logical INTEGER,unique_bytes INTEGER,files INTEGER,dirs INTEGER,identity TEXT,category TEXT,project TEXT,tool TEXT,skipped INTEGER,installation INTEGER,deleted INTEGER NOT NULL DEFAULT 0,PRIMARY KEY(scan,path)) WITHOUT ROWID;
 CREATE INDEX IF NOT EXISTS {s}.entry_changes_parent ON entry_changes(scan,parent);
 CREATE INDEX IF NOT EXISTS {s}.entry_changes_project ON entry_changes(scan,project);
 CREATE TABLE IF NOT EXISTS {s}.project_changes(scan TEXT NOT NULL,id TEXT NOT NULL,path TEXT NOT NULL,name TEXT NOT NULL,markers TEXT NOT NULL,PRIMARY KEY(scan,id)) WITHOUT ROWID;
 CREATE TABLE IF NOT EXISTS {s}.scan_changes(id TEXT PRIMARY KEY,stats TEXT NOT NULL);
 CREATE TABLE IF NOT EXISTS {s}.entry_aliases(scan TEXT NOT NULL,path TEXT NOT NULL,dev TEXT NOT NULL,ino TEXT NOT NULL,PRIMARY KEY(scan,path)) WITHOUT ROWID;
 CREATE TABLE IF NOT EXISTS {s}.error_changes(scan TEXT NOT NULL,path TEXT NOT NULL,code TEXT NOT NULL,reason TEXT NOT NULL DEFAULT '',deleted INTEGER NOT NULL DEFAULT 0,PRIMARY KEY(scan,path,code)) WITHOUT ROWID;"))?;
    Ok(())
}
pub fn active(db: &Connection) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_temp_master WHERE name='entries')",
        [],
        |r| r.get(0),
    )?)
}
pub fn install_if_needed(db: &Connection) -> Result<()> {
    if user_state::schema(db)? != "user_state" {
        return Ok(());
    }
    let exists: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM user_state.sqlite_master WHERE name='entry_changes')",
        [],
        |r| r.get(0),
    )?;
    if exists {
        initialize(db)?;
        if db.query_row("SELECT EXISTS(SELECT 1 FROM user_state.entry_changes) OR EXISTS(SELECT 1 FROM user_state.project_changes) OR EXISTS(SELECT 1 FROM user_state.scan_changes) OR EXISTS(SELECT 1 FROM user_state.error_changes)",[],|r|r.get::<_,bool>(0))? {enable(db)?;}
    }
    Ok(())
}
pub fn enable(db: &Connection) -> Result<()> {
    if !active(db)? {
        db.execute_batch(include_str!("overlay.sql"))?;
    }
    Ok(())
}
pub fn retain_scan(db: &Connection, id: &str) -> Result<()> {
    for (table, col) in [
        ("entry_changes", "scan"),
        ("project_changes", "scan"),
        ("scan_changes", "id"),
        ("entry_aliases", "scan"),
        ("error_changes", "scan"),
    ] {
        db.execute(&format!("DELETE FROM {table} WHERE {col}<>?"), [id])?;
    }
    Ok(())
}

pub fn materialize(db: &Connection) -> Result<()> {
    // Called only on a private writable snapshot; the published main stays intact.
    db.execute_batch("BEGIN")?;
    let result = (|| -> Result<()> {
        db.execute_batch("DELETE FROM main.entries WHERE EXISTS(SELECT 1 FROM user_state.entry_changes d WHERE d.scan=entries.scan AND d.path=entries.path);
   INSERT INTO main.entries(scan,path,parent,name,directory,bytes,logical,unique_bytes,files,dirs,identity,category,project,tool,skipped) SELECT scan,path,parent,name,directory,bytes,logical,unique_bytes,files,dirs,identity,category,project,tool,skipped FROM user_state.entry_changes WHERE deleted=0;
   INSERT OR REPLACE INTO main.projects SELECT * FROM user_state.project_changes;
   UPDATE main.scans SET stats=(SELECT stats FROM user_state.scan_changes d WHERE d.id=scans.id) WHERE id IN(SELECT id FROM user_state.scan_changes);
   DELETE FROM main.errors WHERE EXISTS(SELECT 1 FROM user_state.error_changes d WHERE d.scan=errors.scan AND d.path=errors.path AND d.code=errors.code);
   INSERT INTO main.errors SELECT scan,path,code,reason FROM user_state.error_changes WHERE deleted=0;")?;
        Ok(())
    })();
    if let Err(e) = result {
        let _ = db.execute_batch("ROLLBACK");
        return Err(e);
    }
    db.execute_batch("COMMIT")?;
    Ok(())
}

pub fn clear_applied(db: &Connection) -> Result<()> {
    db.execute_batch("BEGIN IMMEDIATE; DELETE FROM entry_changes;DELETE FROM project_changes;DELETE FROM scan_changes;DELETE FROM error_changes;COMMIT")?;
    Ok(())
}
