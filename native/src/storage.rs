use crate::{fail, index, Result};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::{fs, path::Path};

pub const RESERVE: u64 = 256 * 1024 * 1024;
pub fn free_bytes(path: &Path) -> Result<u64> {
    let path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let stat = unsafe { stat.assume_init() };
    Ok(stat.f_bavail as u64 * stat.f_frsize as u64)
}
pub fn check_capacity(free: u64, reserve: u64) -> Result<()> {
    if free < reserve {
        return Err(fail(format!(
            "索引磁盘空间不足：剩余 {} MB，需保留 {} MB。已停止写入，上次完整扫描结果保留。",
            free / 1_048_576,
            reserve / 1_048_576
        )));
    }
    Ok(())
}
pub fn guard(db: &Connection) -> Result<()> {
    if let Some(path) = db.path().filter(|s| !s.is_empty()) {
        check_capacity(free_bytes(Path::new(path))?, RESERVE)?;
    }
    Ok(())
}
pub fn checkpoint(db: &Connection) -> Result<()> {
    if db.is_autocommit() {
        let _: (i64, i64, i64) = db.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?;
    }
    Ok(())
}
pub fn status(db: &Connection) -> Result<Value> {
    let path = Path::new(db.path().unwrap_or(""));
    let size = |p: &Path| fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    let pages: u64 = db.query_row("PRAGMA page_count", [], |r| r.get(0))?;
    let unused: u64 = db.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
    let page_size: u64 = db.query_row("PRAGMA page_size", [], |r| r.get(0))?;
    let mode: i64 = db.query_row("PRAGMA auto_vacuum", [], |r| r.get(0))?;
    Ok(
        json!({"path":path,"bytes":size(path)+size(Path::new(&format!("{}-wal",path.display()))),"reusableBytes":unused*page_size,"liveBytes":pages.saturating_sub(unused)*page_size,"freeBytes":free_bytes(path).ok(),"reserveBytes":RESERVE,"incremental":mode==2,"compactLayout":!crate::compact::is_legacy(db)?}),
    )
}
// Only rebuildable scan tables are removed. Receipts and user settings stay intact.
pub fn recover(
    db: &Connection,
    reset: bool,
    mut progress: impl FnMut(&str, u64) -> Result<()>,
) -> Result<Value> {
    let before = status(db)?;
    checkpoint(db)?;
    if reset {
        db.execute("UPDATE scans SET status='discarded'", [])?;
    }
    index::prune_with_progress(db, |table, removed| progress(table, removed))?;
    crate::compact::migrate(db, &mut progress)?;
    // Invalidate cached views/authorizations, never operation history or model configuration.
    if reset {
        db.execute("DELETE FROM review_nodes", [])?;
        db.execute("DELETE FROM reviews", [])?;
        db.execute("DELETE FROM settings WHERE key IN ('models-snapshot','dirty','last-job','ai-advice','mole-preview') OR key LIKE 'advice-input-%' OR key LIKE 'discovery-coverage-%' OR key LIKE 'project-%'", [])?;
        let info = status(db)?;
        let needed = info["liveBytes"]
            .as_u64()
            .unwrap_or(0)
            .saturating_mul(2)
            .saturating_add(RESERVE);
        check_capacity(free_bytes(Path::new(db.path().unwrap()))?, needed)?;
        // An empty scan cache needs only enough scratch space for retained settings/history.
        progress("compact", 0)?;
        db.execute_batch(
            "PRAGMA wal_checkpoint(TRUNCATE); PRAGMA auto_vacuum=INCREMENTAL; VACUUM;",
        )?;
    } else if db.query_row("PRAGMA auto_vacuum", [], |r| r.get::<_, i64>(0))? == 2 {
        loop {
            let remaining: u64 = db.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
            if remaining == 0 {
                break;
            }
            db.execute_batch("PRAGMA incremental_vacuum(512)")?;
            checkpoint(db)?;
            progress("compact", remaining)?;
        }
    }
    let _: (i64, i64, i64) = db.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?))
    })?;
    let after = status(db)?;
    Ok(json!({"before":before,"after":after,"reset":reset}))
}

pub fn repack_path(data: &Path) -> std::path::PathBuf {
    data.join(".index-compact.sqlite3")
}

// VACUUM INTO writes only the live pages, requiring one compact copy rather than
// the two-copy scratch space of an in-place VACUUM. Publication happens in the
// parent after the worker exits and all API database readers have drained.
pub fn prepare_repack(
    db: &Connection,
    mut progress: impl FnMut(&str, u64) -> Result<()>,
) -> Result<bool> {
    let info = status(db)?;
    if info["incremental"] == true || info["reusableBytes"].as_u64().unwrap_or(0) < 1_048_576 {
        return Ok(false);
    }
    let path = Path::new(db.path().ok_or_else(|| fail("索引路径不可用"))?);
    let target = repack_path(path.parent().unwrap());
    match fs::remove_file(&target) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    };
    let needed = info["liveBytes"].as_u64().unwrap_or(0).saturating_mul(11) / 10 + RESERVE;
    check_capacity(free_bytes(path)?, needed)?;
    use std::os::unix::fs::OpenOptionsExt;
    let file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&target)?;
    drop(file);
    let result = (|| -> Result<()> {
        progress("compact", 0)?;
        // The result is already in incremental mode, so future reclamation is online.
        db.execute_batch("PRAGMA auto_vacuum=INCREMENTAL")?;
        db.execute("VACUUM INTO ?", [index::path_string(&target)])?;
        let copy = index::connect(&target)?;
        let check: String = copy.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
        if check != "ok" {
            return Err(fail("压缩索引校验失败，原索引已保留"));
        }
        for table in [
            "entries",
            "scans",
            "settings",
            "receipts",
            "projects",
            "tools",
            "associations",
            "errors",
        ] {
            let sql = format!("SELECT count(*) FROM {table}");
            let before: u64 = db.query_row(&sql, [], |r| r.get(0))?;
            let after: u64 = copy.query_row(&sql, [], |r| r.get(0))?;
            if before != after {
                return Err(fail("压缩索引数量核对失败，原索引已保留"));
            }
        }
        copy.close().map_err(|(_, e)| e)?;
        fs::File::open(&target)?.sync_all()?;
        progress("compact", 1)?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = fs::remove_file(target);
        return Err(error);
    }
    Ok(true)
}
pub fn install_repack(data: &Path) -> Result<Value> {
    let path = data.join("index.sqlite3");
    let target = repack_path(data);
    if fs::symlink_metadata(&target)?.file_type().is_symlink() {
        return Err(fail("压缩索引路径异常"));
    }
    let db = index::connect(&path)?;
    let (busy, _, _): (u64, u64, u64) =
        db.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?;
    if busy != 0 {
        return Err(fail("索引仍被占用，原索引已保留"));
    }
    db.close().map_err(|(_, e)| e)?;
    // Never unlink a WAL belonging to an open database.
    if Path::new(&format!("{}-wal", path.display())).exists() {
        return Err(fail("索引仍被占用，稍后重试"));
    }
    fs::rename(&target, &path)?;
    fs::File::open(data)?.sync_all()?;
    status(&index::connect(&path)?)
}

pub fn discard_repack(data: &Path) {
    let path = repack_path(data);
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let _ = fs::remove_file(format!("{}{suffix}", path.display()));
    }
}
