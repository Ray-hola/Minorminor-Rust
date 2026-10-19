//! Fresh scan indexes are built privately, verified, then atomically published.
use crate::{fail, index, storage, Result};
use rusqlite::{Connection, OptionalExtension};
use serde_json::{json, Value};
use std::{
    fs,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

fn paths(data: &Path, id: &str) -> Result<(PathBuf, PathBuf)> {
    if id.is_empty()
        || id.len() > 96
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(fail("索引任务 ID 无效"));
    }
    Ok((
        data.join(format!(".scan-{id}.sqlite3")),
        data.join(format!(".scan-{id}-compressed.sqlite3")),
    ))
}
pub struct Work {
    pub path: PathBuf,
    pub compressed: PathBuf,
    id: String,
    keep: bool,
    manifest: PathBuf,
    pub method: &'static str,
}
impl Drop for Work {
    fn drop(&mut self) {
        if !self.keep {
            remove(&self.path);
            remove(&self.compressed);
            let _ = fs::remove_file(&self.manifest);
        }
    }
}
fn remove(path: &Path) {
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let _ = fs::remove_file(format!("{}{suffix}", path.display()));
    }
}
pub fn discard(data: &Path, id: &str) {
    if let Ok((raw, compressed)) = paths(data, id) {
        remove(&raw);
        remove(&compressed);
        let _ = fs::remove_file(data.join(format!(".scan-{id}.owner.json")));
    }
}

impl Work {
    pub fn prepare(data: &Path, id: &str, copy_current: bool) -> Result<Self> {
        index::secure_directory(data)?;
        cleanup_orphans(data)?;
        let (path, compressed) = paths(data, id)?;
        let mut work = Self {
            path,
            compressed,
            id: id.into(),
            manifest: data.join(format!(".scan-{id}.owner.json")),
            keep: true,
            method: "prepared",
        };
        if !work.manifest.try_exists()? {
            use std::io::Write;
            let mut owner = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&work.manifest)?;
            owner.write_all(
                serde_json::to_string(
                    &json!({"format":"minorminor-scan-work-v1","job":id,"pid":std::process::id()}),
                )?
                .as_bytes(),
            )?;
            owner.sync_all()?;
        } else {
            let owner = read_owner(&work.manifest)?;
            if owner["job"] != id || owner["pid"] != std::process::id() {
                return Err(fail("工作索引由其他进程持有"));
            }
        }
        if work.path.try_exists()? {
            let metadata = fs::symlink_metadata(&work.path)?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(fail("工作索引路径无效"));
            }
            let db = index::connect_readonly(&work.path)?;
            let marker: Option<(String, String, String)> = db
                .query_row("SELECT id,phase,mode FROM _snapshot_job", [], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })
                .optional()?;
            if marker
                != Some((
                    id.into(),
                    "prepared".into(),
                    if copy_current { "copy" } else { "fresh" }.into(),
                ))
            {
                return Err(fail("工作索引已存在且不可复用"));
            }
            work.keep = false;
            return Ok(work);
        }
        if work.compressed.try_exists()? {
            return Err(fail("压缩工作索引已存在"));
        }
        if copy_current {
            let source = index::path(&data);
            if crate::fs_compression::is_compressed(&source) {
                storage::check_capacity(
                    storage::free_bytes(data)?,
                    source.metadata()?.len().saturating_add(storage::RESERVE),
                )?;
            }
            #[cfg(target_os = "macos")]
            let cloned = {
                use std::ffi::CString;
                use std::os::unix::ffi::OsStrExt;
                let compressed = crate::fs_compression::is_compressed(&source);
                let checkpoint = if compressed {
                    None
                } else {
                    let db = Connection::open(&source)?;
                    db.busy_timeout(Duration::from_secs(10))?;
                    db.execute_batch("BEGIN IMMEDIATE")?;
                    Some(db)
                };
                let before = index::Identity::of(&fs::metadata(&source)?);
                let from = CString::new(source.as_os_str().as_bytes())?;
                let to = CString::new(work.path.as_os_str().as_bytes())?;
                let wal = PathBuf::from(format!("{}-wal", source.display()));
                let wal_identity = fs::metadata(&wal)
                    .ok()
                    .filter(|m| m.len() > 0)
                    .map(|m| index::Identity::of(&m));
                let cloned = if unsafe { libc::clonefile(from.as_ptr(), to.as_ptr(), 0) } == 0 {
                    work.keep = false;
                    let wal_copied = if wal_identity.is_some() {
                        let from = CString::new(wal.as_os_str().as_bytes())?;
                        let to = CString::new(format!("{}-wal", work.path.display()))?;
                        (unsafe { libc::clonefile(from.as_ptr(), to.as_ptr(), 0) }) == 0
                    } else {
                        true
                    };
                    let final_wal_identity = fs::metadata(&wal)
                        .ok()
                        .filter(|m| m.len() > 0)
                        .map(|m| index::Identity::of(&m));
                    if index::Identity::of(&fs::metadata(&source)?) != before
                        || crate::fs_compression::is_compressed(&source) != compressed
                        || !wal_copied
                        || wal_identity != final_wal_identity
                    {
                        remove(&work.path);
                        false
                    } else {
                        if compressed {
                            let _expand = fs::OpenOptions::new()
                                .read(true)
                                .write(true)
                                .custom_flags(libc::O_NOFOLLOW)
                                .open(&work.path)?;
                        }
                        true
                    }
                } else {
                    false
                };
                if let Some(db) = checkpoint {
                    db.execute_batch("ROLLBACK")?;
                }
                cloned
            };
            #[cfg(not(target_os = "macos"))]
            let cloned = false;
            if !cloned {
                work.method = "sqlite-backup";
                storage::check_capacity(
                    storage::free_bytes(data)?,
                    source.metadata()?.len().saturating_add(storage::RESERVE),
                )?;
                let _new = fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .mode(0o600)
                    .open(&work.path)?;
                work.keep = false;
                let source = index::connect_readonly(&source)?;
                let mut target = Connection::open(&work.path)?;
                let backup = rusqlite::backup::Backup::new(&source, &mut target)?;
                backup.run_to_completion(2048, Duration::from_millis(10), None)?;
            } else {
                work.method = "apfs-clone";
            }
        } else {
            work.method = "fresh";
            let _new = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(&work.path)?;
        }
        work.keep = false;
        index::initialize_working(data, &work.path)?;
        let db = index::connect(&work.path)?;
        if copy_current {
            crate::overlay::materialize(&db)?;
        }
        db.execute_batch("CREATE TABLE IF NOT EXISTS _snapshot_job(id TEXT NOT NULL,phase TEXT NOT NULL,mode TEXT NOT NULL); DELETE FROM _snapshot_job;")?;
        db.execute(
            "INSERT INTO _snapshot_job VALUES(?,'prepared',?)",
            [id, if copy_current { "copy" } else { "fresh" }],
        )?;
        Ok(work)
    }
    pub fn claim(&self) -> Result<()> {
        let db = index::connect(&self.path)?;
        if db.execute(
            "UPDATE _snapshot_job SET phase='claimed' WHERE id=? AND phase='prepared'",
            [&self.id],
        )? != 1
        {
            return Err(fail("工作索引已被使用"));
        }
        Ok(())
    }
    pub fn finish(
        mut self,
        parent_publishes: bool,
        mut progress: impl FnMut(&str, u64, u64) -> Result<()>,
    ) -> Result<Value> {
        {
            let db = index::connect(&self.path)?;
            db.execute(
                "UPDATE _snapshot_job SET phase='ready' WHERE id=?",
                [&self.id],
            )?;
            db.execute_batch("PRAGMA main.wal_checkpoint(TRUNCATE)")?;
        }
        let mut checked = 0_u64;
        let reserve = if self.path.metadata()?.len() <= 1024 * 1024 {
            8 * 1024 * 1024
        } else {
            storage::RESERVE
        };
        let compressed =
            match crate::fs_compression::copy(&self.path, &self.compressed, |phase, done, total| {
                if done == 0 || done.saturating_sub(checked) >= 8 * 1024 * 1024 {
                    storage::check_capacity(
                        storage::free_bytes(self.path.parent().unwrap())?,
                        reserve,
                    )?;
                    checked = done;
                }
                progress(phase, done, total)
            }) {
                Ok(Some(info)) => Some(info),
                Ok(None) => None,
                Err(e) => {
                    // Unsupported filesystem compression must never make a valid
                    // full scan unusable. Cancellation/progress errors still fail.
                    if fs::symlink_metadata(&self.compressed).is_ok() {
                        return Err(e);
                    }
                    let message = e.to_string();
                    if message.contains("Operation not supported")
                        || message.contains("Not supported")
                    {
                        None
                    } else {
                        return Err(e);
                    }
                }
            };
        let ready = if compressed.is_some() {
            &self.compressed
        } else {
            &self.path
        };
        fs::File::open(ready)?.sync_all()?;
        fs::File::open(ready.parent().unwrap())?.sync_all()?;
        let result = json!({"indexReady":parent_publishes,"compressed":compressed.is_some(),"compression":compressed,"indexCopyMethod":self.method});
        if parent_publishes {
            self.keep = true;
        } else {
            install(ready.parent().unwrap(), &self.id)?;
        }
        Ok(result)
    }
}

pub fn install(data: &Path, id: &str) -> Result<Value> {
    let (raw, compressed) = paths(data, id)?;
    let ready = if compressed.try_exists()? {
        &compressed
    } else {
        &raw
    };
    let m = fs::symlink_metadata(ready)?;
    if !m.is_file() || m.file_type().is_symlink() || m.uid() != unsafe { libc::geteuid() } {
        return Err(fail("已生成索引路径无效"));
    }
    let mode;
    {
        let db = index::connect_readonly(ready)?;
        let marker: (String, String, String) =
            db.query_row("SELECT id,phase,mode FROM _snapshot_job", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?;
        mode = marker.2.clone();
        if (marker.0, marker.1) != (id.into(), "ready".into()) {
            return Err(fail("已生成索引不属于当前任务"));
        }
        let incomplete: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM scans WHERE status IN ('running','index-complete'))",
            [],
            |r| r.get(0),
        )?;
        if incomplete {
            return Err(fail("索引尚未完成，原结果已保留"));
        }
    }
    let current = index::path(&data);
    if !crate::fs_compression::is_compressed(&current) {
        let db = index::connect(&current)?;
        let (busy, _, _): (i64, i64, i64) =
            db.query_row("PRAGMA main.wal_checkpoint(TRUNCATE)", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?;
        if busy != 0 {
            return Err(fail("索引仍被占用，原结果已保留"));
        }
    }
    if Path::new(&format!("{}-wal", current.display())).exists() {
        return Err(fail("索引仍被占用，原结果已保留"));
    }
    fs::rename(ready, &current)?;
    fs::File::open(data)?.sync_all()?;
    remove(&raw);
    remove(&compressed);
    let _ = fs::remove_file(data.join(format!(".scan-{id}.owner.json")));
    let db = index::connect_readonly(&current)?;
    if mode == "copy" {
        crate::overlay::clear_applied(&db)?;
    } else {
        crate::overlay::retain_scan(&db, index::latest(&db)?.as_deref().unwrap_or(""))?;
    }
    storage::status(&db)
}

pub(crate) fn read_owner(path: &Path) -> Result<Value> {
    use std::io::Read;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.len() > 4096
    {
        return Err(fail("工作索引所有者记录无效"));
    }
    let mut text = String::new();
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?
        .take(4097)
        .read_to_string(&mut text)?;
    let value: Value = serde_json::from_str(&text)?;
    if value["format"] != "minorminor-scan-work-v1" {
        return Err(fail("工作索引所有者记录格式无效"));
    }
    Ok(value)
}
// Only our reserved, validated namespace is reclaimed, and only after its owner
// has exited. A reused/live PID causes retention; no process is ever signalled.
pub fn cleanup_orphans(data: &Path) -> Result<u64> {
    let mut removed = 0;
    for entry in fs::read_dir(data)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(id) = name
            .strip_prefix(".scan-")
            .and_then(|n| n.strip_suffix(".owner.json"))
        else {
            continue;
        };
        let Ok((raw, compressed)) = paths(data, id) else {
            continue;
        };
        let Ok(owner) = read_owner(&entry.path()) else {
            continue;
        };
        if owner["job"] != id {
            continue;
        }
        let Some(pid) = owner["pid"]
            .as_u64()
            .filter(|p| *p > 0 && *p < i32::MAX as u64)
        else {
            continue;
        };
        if unsafe { libc::kill(pid as i32, 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
        {
            continue;
        }
        let safe = [&raw, &compressed]
            .iter()
            .all(|p| match fs::symlink_metadata(p) {
                Ok(m) => {
                    m.is_file()
                        && !m.file_type().is_symlink()
                        && m.uid() == unsafe { libc::geteuid() }
                }
                Err(e) => e.kind() == std::io::ErrorKind::NotFound,
            });
        if !safe {
            continue;
        }
        remove(&raw);
        remove(&compressed);
        fs::remove_file(entry.path())?;
        removed += 1;
    }
    Ok(removed)
}
