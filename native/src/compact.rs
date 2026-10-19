//! Compact, lossless scan storage. Public SQL/API paths and identities stay unchanged.
use crate::{index, storage, Result};
use rusqlite::{Connection, OptionalExtension};

pub fn is_legacy(db: &Connection) -> Result<bool> {
    Ok(db
        .query_row(
            "SELECT type FROM sqlite_master WHERE name='entries'",
            [],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .as_deref()
        == Some("table"))
}
pub fn initialize(db: &Connection) -> Result<()> {
    let exists: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='entries')",
        [],
        |r| r.get(0),
    )?;
    if !exists {
        create_tables(db)?;
        create_view(db)?;
        indexes(db, |_| Ok(()))?;
    }
    Ok(())
}
fn create_tables(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS entry_scans(id INTEGER PRIMARY KEY,value TEXT NOT NULL UNIQUE);
CREATE TABLE IF NOT EXISTS entry_parents(id INTEGER PRIMARY KEY,path TEXT NOT NULL UNIQUE);
",
    )?;
    db.execute_batch(&entry_table("_entries"))?;
    Ok(())
}
fn entry_table(name: &str) -> String {
    format!(
        r#"CREATE TABLE {name}(
 scan INTEGER NOT NULL,path TEXT NOT NULL,parent INTEGER NOT NULL,name TEXT NOT NULL,directory INTEGER NOT NULL,
 bytes INTEGER NOT NULL,logical INTEGER NOT NULL,unique_bytes INTEGER NOT NULL,files INTEGER NOT NULL DEFAULT 0,dirs INTEGER NOT NULL DEFAULT 0,
 identity_bits TEXT NOT NULL,
 category TEXT NOT NULL,project TEXT,tool TEXT,skipped INTEGER NOT NULL DEFAULT 0,installation INTEGER NOT NULL,
 PRIMARY KEY(scan,path));"#
    )
}
pub const RAW_COLUMNS: &str = "scan,path,parent,name,directory,bytes,logical,unique_bytes,files,dirs,identity_bits,category,project,tool,skipped,installation";

pub fn needs_upgrade(db: &Connection) -> Result<bool> {
    if is_legacy(db)? {
        return Ok(true);
    }
    let sql: String = db.query_row(
        "SELECT sql FROM sqlite_master WHERE name='_entries'",
        [],
        |r| r.get(0),
    )?;
    Ok(sql.to_ascii_uppercase().contains("WITHOUT ROWID"))
}
const VIEW_SQL: &str = r#"CREATE VIEW entries AS SELECT s.value AS scan,e.path,p.path AS parent,e.name,e.directory,e.bytes,e.logical,e.unique_bytes,e.files,e.dirs,CASE WHEN json_type(e.identity_bits)='array' THEN json_object('dev',json(e.identity_bits -> '$[0]'),'ino',json(e.identity_bits -> '$[1]'),'size',json(e.identity_bits -> '$[2]'),'mtime',json(e.identity_bits -> '$[3]'),'mtime_ns',json(e.identity_bits -> '$[4]'),'ctime',json(e.identity_bits -> '$[5]'),'ctime_ns',json(e.identity_bits -> '$[6]'),'nlink',json(e.identity_bits -> '$[7]')) ELSE e.identity_bits END AS identity,e.category,e.project,e.tool,e.skipped,e.installation FROM _entries e JOIN entry_scans s ON s.id=e.scan JOIN entry_parents p ON p.id=e.parent;CREATE TRIGGER entries_insert INSTEAD OF INSERT ON entries BEGIN
 INSERT INTO entry_scans(value) SELECT NEW.scan WHERE NOT EXISTS(SELECT 1 FROM entry_scans WHERE value=NEW.scan);
 INSERT INTO entry_parents(path) SELECT NEW.parent WHERE NOT EXISTS(SELECT 1 FROM entry_parents WHERE path=NEW.parent);
 INSERT INTO _entries(scan,path,parent,name,directory,bytes,logical,unique_bytes,files,dirs,identity_bits,category,project,tool,skipped,installation) VALUES((SELECT id FROM entry_scans WHERE value=NEW.scan),NEW.path,(SELECT id FROM entry_parents WHERE path=NEW.parent),NEW.name,NEW.directory,NEW.bytes,NEW.logical,NEW.unique_bytes,coalesce(NEW.files,0),coalesce(NEW.dirs,0),CASE WHEN NEW.identity -> '$.dev' IS NOT NULL AND NEW.identity -> '$.ino' IS NOT NULL AND NEW.identity -> '$.size' IS NOT NULL AND NEW.identity -> '$.mtime' IS NOT NULL AND NEW.identity -> '$.mtime_ns' IS NOT NULL AND NEW.identity -> '$.ctime' IS NOT NULL AND NEW.identity -> '$.ctime_ns' IS NOT NULL AND NEW.identity -> '$.nlink' IS NOT NULL AND json_remove(NEW.identity,'$.dev','$.ino','$.size','$.mtime','$.mtime_ns','$.ctime','$.ctime_ns','$.nlink')='{}' THEN json_array(json(NEW.identity -> '$.dev'),json(NEW.identity -> '$.ino'),json(NEW.identity -> '$.size'),json(NEW.identity -> '$.mtime'),json(NEW.identity -> '$.mtime_ns'),json(NEW.identity -> '$.ctime'),json(NEW.identity -> '$.ctime_ns'),json(NEW.identity -> '$.nlink')) ELSE NEW.identity END,NEW.category,NEW.project,NEW.tool,coalesce(NEW.skipped,0),(NEW.directory=0 AND (NEW.name IN ('package.json','app.asar','Info.plist','METADATA','PKG-INFO') OR NEW.parent LIKE '%/bin'))); END;CREATE TRIGGER entries_update INSTEAD OF UPDATE OF scan,path,parent,name,directory,identity ON entries BEGIN
 INSERT INTO entry_scans(value) SELECT NEW.scan WHERE NOT EXISTS(SELECT 1 FROM entry_scans WHERE value=NEW.scan);
 INSERT INTO entry_parents(path) SELECT NEW.parent WHERE NOT EXISTS(SELECT 1 FROM entry_parents WHERE path=NEW.parent);
 UPDATE _entries SET (scan,path,parent,name,directory,bytes,logical,unique_bytes,files,dirs,identity_bits,category,project,tool,skipped,installation)=((SELECT id FROM entry_scans WHERE value=NEW.scan),NEW.path,(SELECT id FROM entry_parents WHERE path=NEW.parent),NEW.name,NEW.directory,NEW.bytes,NEW.logical,NEW.unique_bytes,coalesce(NEW.files,0),coalesce(NEW.dirs,0),CASE WHEN NEW.identity -> '$.dev' IS NOT NULL AND NEW.identity -> '$.ino' IS NOT NULL AND NEW.identity -> '$.size' IS NOT NULL AND NEW.identity -> '$.mtime' IS NOT NULL AND NEW.identity -> '$.mtime_ns' IS NOT NULL AND NEW.identity -> '$.ctime' IS NOT NULL AND NEW.identity -> '$.ctime_ns' IS NOT NULL AND NEW.identity -> '$.nlink' IS NOT NULL AND json_remove(NEW.identity,'$.dev','$.ino','$.size','$.mtime','$.mtime_ns','$.ctime','$.ctime_ns','$.nlink')='{}' THEN json_array(json(NEW.identity -> '$.dev'),json(NEW.identity -> '$.ino'),json(NEW.identity -> '$.size'),json(NEW.identity -> '$.mtime'),json(NEW.identity -> '$.mtime_ns'),json(NEW.identity -> '$.ctime'),json(NEW.identity -> '$.ctime_ns'),json(NEW.identity -> '$.nlink')) ELSE NEW.identity END,NEW.category,NEW.project,NEW.tool,coalesce(NEW.skipped,0),(NEW.directory=0 AND (NEW.name IN ('package.json','app.asar','Info.plist','METADATA','PKG-INFO') OR NEW.parent LIKE '%/bin'))) WHERE scan=(SELECT id FROM entry_scans WHERE value=OLD.scan) AND path=OLD.path; END;
CREATE TRIGGER entries_counts INSTEAD OF UPDATE OF bytes,logical,unique_bytes,files,dirs,skipped ON entries BEGIN
 UPDATE _entries SET bytes=NEW.bytes,logical=NEW.logical,unique_bytes=NEW.unique_bytes,files=NEW.files,dirs=NEW.dirs,skipped=NEW.skipped WHERE scan=(SELECT id FROM entry_scans WHERE value=OLD.scan) AND path=OLD.path; END;
CREATE TRIGGER entries_tags INSTEAD OF UPDATE OF category,project,tool ON entries BEGIN
 UPDATE _entries SET category=NEW.category,project=NEW.project,tool=NEW.tool WHERE scan=(SELECT id FROM entry_scans WHERE value=OLD.scan) AND path=OLD.path; END;
CREATE TRIGGER entries_delete INSTEAD OF DELETE ON entries BEGIN
 DELETE FROM _entries WHERE scan=(SELECT id FROM entry_scans WHERE value=OLD.scan) AND path=OLD.path; END;"#;
fn create_view(db: &Connection) -> Result<()> {
    db.execute_batch(VIEW_SQL)?;
    Ok(())
}
pub fn indexes(db: &Connection, mut progress: impl FnMut(&str) -> Result<()>) -> Result<()> {
    for (name,sql) in [
      ("parents","CREATE INDEX IF NOT EXISTS entries_parent ON _entries(parent,scan)"),
      ("projects","CREATE INDEX IF NOT EXISTS entries_project ON _entries(scan,project,category) WHERE project IS NOT NULL"),
      ("tools","CREATE INDEX IF NOT EXISTS entries_tool ON _entries(scan,tool) WHERE tool IS NOT NULL"),
      ("installations","CREATE INDEX IF NOT EXISTS entries_installations ON _entries(scan,path) WHERE directory=0 AND installation=1"),
      ("memory","CREATE INDEX IF NOT EXISTS entries_memory ON _entries(scan,tool,path) WHERE directory=0 AND category='memory' AND tool IS NOT NULL"),
      ("directories","CREATE INDEX IF NOT EXISTS entries_directories ON _entries(scan,path) WHERE directory=1"),
      ("hardlinks","CREATE INDEX IF NOT EXISTS entries_hardlinks ON _entries(scan,path) WHERE directory=0 AND CASE WHEN json_type(identity_bits)='array' THEN json_extract(identity_bits,'$[7]') ELSE json_extract(identity_bits,'$.nlink') END>1"),
    ] { progress(name)?; db.execute_batch(sql)?; storage::checkpoint(db)?; }
    Ok(())
}

// The legacy table remains authoritative until the final transaction commits.
// Interrupted copies can be discarded and retried without invalidating the last scan.
pub fn migrate(db: &Connection, mut progress: impl FnMut(&str, u64) -> Result<()>) -> Result<()> {
    if !is_legacy(db)? {
        if needs_upgrade(db)? {
            migrate_wide(db, &mut progress)?;
        }
        return indexes(db, |name| progress(name, 0));
    }
    progress("metadata", 0)?;
    db.execute_batch("DROP VIEW IF EXISTS entries_copy; DROP TABLE IF EXISTS _entries; DROP TABLE IF EXISTS entry_parents; DROP TABLE IF EXISTS entry_scans;")?;
    storage::checkpoint(db)?;
    create_tables(db)?;
    // Use the same insertion path as new scans, with a temporary view name.
    create_view_for_copy(db)?;
    let mut cursor = 0_i64;
    let mut copied = 0_u64;
    loop {
        storage::guard(db)?;
        let next:Option<i64>=db.query_row("SELECT max(rowid) FROM (SELECT rowid FROM entries WHERE rowid>? ORDER BY rowid LIMIT 1000)",[cursor],|r|r.get(0))?;
        let Some(next) = next else {
            break;
        };
        db.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<()> {
            db.execute("INSERT INTO entries_copy(scan,path,parent,name,directory,bytes,logical,unique_bytes,files,dirs,identity,category,project,tool,skipped) SELECT scan,path,parent,name,directory,bytes,logical,unique_bytes,files,dirs,identity,category,project,tool,skipped FROM entries WHERE rowid>? AND rowid<=?",[cursor,next])?;
            // Check every copied row while both representations are available.
            let mismatch: bool = db.query_row(VERIFY_BATCH, [cursor, next], |r| r.get(0))?;
            if mismatch {
                return Err(crate::fail("索引迁移核对失败，原扫描结果已保留"));
            }
            Ok(())
        })();
        if let Err(error) = result {
            let _ = db.execute_batch("ROLLBACK");
            return Err(error);
        }
        db.execute_batch("COMMIT")?;
        storage::checkpoint(db)?;
        copied += db.query_row(
            "SELECT count(*) FROM entries WHERE rowid>? AND rowid<=?",
            [cursor, next],
            |r| r.get::<_, u64>(0),
        )?;
        cursor = next;
        progress("metadata", copied)?;
    }
    progress("verification", copied)?;
    db.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| -> Result<()> {
        let count: u64 = db.query_row("SELECT count(*) FROM _entries", [], |r| r.get(0))?;
        let source_count: u64 = db.query_row("SELECT count(*) FROM entries", [], |r| r.get(0))?;
        if count != copied || source_count != copied {
            return Err(crate::fail("索引迁移数量不一致，原扫描结果已保留"));
        }
        db.execute_batch("DROP VIEW entries_copy; DROP TABLE entries;")?;
        create_view(db)?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = db.execute_batch("ROLLBACK");
        return Err(error);
    }
    db.execute_batch("COMMIT")?;
    storage::checkpoint(db)?;
    indexes(db, |name| progress(name, copied))?;
    index::optimize(db)?;
    index::save_setting(db, "index-layout", &serde_json::json!({"version":3}))?;
    Ok(())
}

// Secondary indexes on a WITHOUT ROWID table repeat its full (scan,path) key.
// Keep that unique lookup, but let secondary indexes reference a short rowid.
// The old table/view remain authoritative until the verified copy is published.
fn migrate_wide(db: &Connection, progress: &mut impl FnMut(&str, u64) -> Result<()>) -> Result<()> {
    let interrupted: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='_entries_next')",
        [],
        |r| r.get(0),
    )?;
    db.execute_batch("DROP TABLE IF EXISTS _entries_next")?;
    storage::checkpoint(db)?;
    if interrupted {
        storage::reclaim_unused(db, |remaining| progress("compact", remaining))?;
    }
    storage::guard(db)?;
    if let Some(path) = db.path().filter(|path| !path.is_empty()) {
        let live = storage::status(db)?["liveBytes"].as_u64().unwrap_or(0);
        storage::check_capacity(
            storage::free_bytes(std::path::Path::new(path))?,
            live.saturating_add(storage::RESERVE),
        )?;
    }
    db.execute_batch(&entry_table("_entries_next"))?;
    let mut cursor = (i64::MIN, String::new());
    let mut copied = 0_u64;
    progress("metadata", copied)?;
    loop {
        storage::guard(db)?;
        let next: Option<(i64, String)> = db.query_row(
            "SELECT scan,path FROM (SELECT scan,path FROM _entries WHERE (scan,path)>(?1,?2) ORDER BY scan,path LIMIT 2000) ORDER BY scan DESC,path DESC LIMIT 1",
            rusqlite::params![cursor.0, cursor.1], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
        let Some(next) = next else { break };
        db.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<usize> {
            let changed = db.execute(&format!("INSERT INTO _entries_next({RAW_COLUMNS}) SELECT {RAW_COLUMNS} FROM _entries WHERE (scan,path)>(?1,?2) AND (scan,path)<=(?3,?4)"), rusqlite::params![cursor.0,cursor.1,next.0,next.1])?;
            let mismatch: bool = db.query_row(
                VERIFY_WIDE_BATCH,
                rusqlite::params![cursor.0, cursor.1, next.0, next.1],
                |r| r.get(0),
            )?;
            if mismatch {
                return Err(crate::fail("索引迁移核对失败，原扫描结果已保留"));
            }
            Ok(changed)
        })();
        let changed = match result {
            Ok(changed) => changed,
            Err(error) => {
                let _ = db.execute_batch("ROLLBACK");
                return Err(error);
            }
        };
        db.execute_batch("COMMIT")?;
        storage::checkpoint(db)?;
        copied += changed as u64;
        cursor = next;
        progress("metadata", copied)?;
    }
    progress("verification", copied)?;
    db.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| -> Result<()> {
        let source: u64 = db.query_row("SELECT count(*) FROM _entries", [], |r| r.get(0))?;
        let target: u64 = db.query_row("SELECT count(*) FROM _entries_next", [], |r| r.get(0))?;
        if source != copied || target != copied {
            return Err(crate::fail("索引迁移数量不一致，原扫描结果已保留"));
        }
        progress("publication", copied)?;
        db.execute_batch(
            "DROP VIEW entries; DROP TABLE _entries; ALTER TABLE _entries_next RENAME TO _entries;",
        )?;
        create_view(db)?;
        indexes(db, |name| progress(name, copied))?;
        index::save_setting(db, "index-layout", &serde_json::json!({"version":3}))?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = db.execute_batch("ROLLBACK");
        return Err(error);
    }
    db.execute_batch("COMMIT")?;
    storage::checkpoint(db)?;
    index::optimize(db)?;
    Ok(())
}

const VERIFY_WIDE_BATCH: &str = "SELECT EXISTS(SELECT 1 FROM _entries old WHERE (old.scan,old.path)>(?1,?2) AND (old.scan,old.path)<=(?3,?4) AND NOT EXISTS(SELECT 1 FROM _entries_next new WHERE new.scan=old.scan AND new.path=old.path AND (new.parent,new.name,new.directory,new.bytes,new.logical,new.unique_bytes,new.files,new.dirs,new.identity_bits,new.category,new.project,new.tool,new.skipped,new.installation) IS (old.parent,old.name,old.directory,old.bytes,old.logical,old.unique_bytes,old.files,old.dirs,old.identity_bits,old.category,old.project,old.tool,old.skipped,old.installation)))";
fn create_view_for_copy(db: &Connection) -> Result<()> {
    db.execute_batch(
        &VIEW_SQL
            .replace("VIEW entries AS", "VIEW entries_copy AS")
            .replace(" ON entries BEGIN", " ON entries_copy BEGIN")
            .replace("TRIGGER entries_", "TRIGGER entries_copy_"),
    )?;
    Ok(())
}

pub fn prune_directories(
    db: &Connection,
    progress: &mut impl FnMut(&str, u64) -> Result<()>,
    removed: u64,
) -> Result<()> {
    db.execute_batch("CREATE INDEX IF NOT EXISTS entries_parent ON _entries(parent,scan)")?;
    let mut cursor = 0_i64;
    loop {
        let next: Option<i64> = db.query_row(
            "SELECT max(id) FROM (SELECT id FROM entry_parents WHERE id>? ORDER BY id LIMIT 2000)",
            [cursor],
            |r| r.get(0),
        )?;
        let Some(next) = next else { break };
        db.execute("DELETE FROM entry_parents WHERE id>? AND id<=? AND NOT EXISTS(SELECT 1 FROM _entries WHERE parent=entry_parents.id)",[cursor,next])?;
        storage::checkpoint(db)?;
        cursor = next;
        progress("directories", removed)?;
    }
    db.execute("DELETE FROM entry_scans WHERE NOT EXISTS(SELECT 1 FROM _entries WHERE scan=entry_scans.id)",[])?;
    Ok(())
}

pub fn hardlinks_query(db: &Connection) -> Result<&'static str> {
    if is_legacy(db)? {
        return Ok("SELECT path,identity,bytes FROM main.entries WHERE scan=? AND directory=0 AND json_extract(identity,'$.nlink')>1");
    }
    let sql: String = db.query_row(
        "SELECT sql FROM main.sqlite_master WHERE name='entries_hardlinks'",
        [],
        |r| r.get(0),
    )?;
    Ok(if sql.contains("identity_bits") {
        HARDLINKS
    } else {
        "SELECT path,identity,bytes FROM main._entries INDEXED BY entries_hardlinks WHERE scan=(SELECT id FROM entry_scans WHERE value=?) AND directory=0 AND json_extract(identity,'$.nlink')>1"
    })
}
pub const HARDLINKS:&str="SELECT path,CASE WHEN json_type(identity_bits)='array' THEN json_object('dev',json(identity_bits -> '$[0]'),'ino',json(identity_bits -> '$[1]'),'size',json(identity_bits -> '$[2]'),'mtime',json(identity_bits -> '$[3]'),'mtime_ns',json(identity_bits -> '$[4]'),'ctime',json(identity_bits -> '$[5]'),'ctime_ns',json(identity_bits -> '$[6]'),'nlink',json(identity_bits -> '$[7]')) ELSE identity_bits END AS identity,bytes FROM _entries INDEXED BY entries_hardlinks WHERE scan=(SELECT id FROM entry_scans WHERE value=?) AND directory=0 AND CASE WHEN json_type(identity_bits)='array' THEN json_extract(identity_bits,'$[7]') ELSE json_extract(identity_bits,'$.nlink') END>1";
pub const DIRECTORIES: &str = "SELECT path FROM _entries INDEXED BY entries_directories WHERE scan=(SELECT id FROM entry_scans WHERE value=?) AND directory=1 ORDER BY path DESC";
pub const CHILDREN: &str = "SELECT name,path,bytes,logical,files,dirs,directory,skipped FROM _entries INDEXED BY entries_parent WHERE scan=(SELECT id FROM entry_scans WHERE value=?1) AND parent=(SELECT id FROM entry_parents WHERE path=?2) AND path<>?3 ORDER BY bytes DESC";

/// Keyed verification must never materialize the growing shadow snapshot.
pub const VERIFY_BATCH: &str = "SELECT EXISTS(SELECT 1 FROM entries old WHERE old.rowid>? AND old.rowid<=? AND NOT EXISTS(SELECT 1 FROM _entries new JOIN entry_scans s ON s.id=new.scan JOIN entry_parents p ON p.id=new.parent WHERE s.value=old.scan AND new.path=old.path AND (old.parent,old.name,old.directory,old.bytes,old.logical,old.unique_bytes,old.files,old.dirs,old.category,old.project,old.tool,old.skipped) IS (p.path,new.name,new.directory,new.bytes,new.logical,new.unique_bytes,new.files,new.dirs,new.category,new.project,new.tool,new.skipped) AND new.identity_bits=CASE WHEN json_type(new.identity_bits)='array' THEN json_array(json(old.identity -> '$.dev'),json(old.identity -> '$.ino'),json(old.identity -> '$.size'),json(old.identity -> '$.mtime'),json(old.identity -> '$.mtime_ns'),json(old.identity -> '$.ctime'),json(old.identity -> '$.ctime_ns'),json(old.identity -> '$.nlink')) ELSE old.identity END))";
