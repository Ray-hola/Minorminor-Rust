use crate::{fail, index, Result};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::{
    fs,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

pub fn bundle() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    bundle_for(&exe)
}
pub fn bundle_for(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let app = contents.parent()?;
    if macos.file_name()? != "MacOS"
        || contents.file_name()? != "Contents"
        || app.extension()? != "app"
    {
        return None;
    }
    Some(app.to_path_buf())
}
pub fn status(db: &Connection) -> Result<Value> {
    let app = bundle();
    let shown = index::setting(db, "access-guidance")?.is_some_and(|v| v["appPath"] == json!(app));
    Ok(json!({"appPath":app,"setupShown":shown,"authorization":"system-managed"}))
}
pub fn acknowledge(db: &Connection) -> Result<()> {
    index::save_setting(db, "access-guidance", &json!({"appPath":bundle()}))
}
pub fn open_settings() -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        if let Some(app) = bundle() {
            std::process::Command::new("/usr/bin/open")
                .arg("-R")
                .arg(app)
                .spawn()?;
        }
        std::process::Command::new("/usr/bin/open")
            .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles")
            .spawn()?;
    }
    Ok(())
}
// The handle stays alive in main; a second GUI launch must not reset an active scan.
pub fn instance_lock(data: &Path) -> Result<Option<fs::File>> {
    index::secure_directory(data)?;
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(data.join("app.lock"))?;
    use std::os::fd::AsRawFd;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::WouldBlock {
            return Ok(None);
        }
        return Err(fail(format!("无法锁定应用记录：{err}")));
    }
    Ok(Some(file))
}
