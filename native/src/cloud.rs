use crate::{fail, index::Identity, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    cell::RefCell,
    collections::HashMap,
    fs,
    io::{self, BufRead, BufReader, Read, Write},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};
#[derive(Clone, Serialize, Deserialize)]
pub struct Metadata {
    pub identity: Identity,
    pub allocated_blocks: u64,
    pub directory: bool,
    pub file: bool,
    pub link: bool,
}
impl Metadata {
    pub fn dev(&self) -> u64 {
        self.identity.dev
    }
    pub fn ino(&self) -> u64 {
        self.identity.ino
    }
    pub fn len(&self) -> u64 {
        self.identity.size
    }
    pub fn nlink(&self) -> u64 {
        self.identity.nlink
    }
    pub fn blocks(&self) -> u64 {
        self.allocated_blocks
    }
    pub fn is_file(&self) -> bool {
        self.file
    }
    pub fn is_dir(&self) -> bool {
        self.directory
    }
    pub fn file_type(&self) -> &Self {
        self
    }
    pub fn is_symlink(&self) -> bool {
        self.link
    }
    fn of(m: &fs::Metadata) -> Self {
        Self {
            identity: Identity::of(m),
            allocated_blocks: m.blocks(),
            directory: m.is_dir(),
            file: m.is_file(),
            link: m.file_type().is_symlink(),
        }
    }
}
thread_local! {static CACHE:RefCell<HashMap<PathBuf,Metadata>>=RefCell::new(HashMap::new());}
pub fn is_cloud(path: &Path) -> bool {
    let text = path.to_string_lossy();
    text.contains("/Library/Mobile Documents") || text.contains("/Library/CloudStorage")
}
fn request(path: &Path, mode: &str) -> io::Result<(Vec<Value>, bool)> {
    let mut child = Command::new(std::env::current_exe()?)
        .arg("--directory-worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut input = child.stdin.take().unwrap();
    input.write_all(json!({"path":path,"mode":mode}).to_string().as_bytes())?;
    drop(input);
    let (tx, rx) = std::sync::mpsc::sync_channel(128);
    let stdout = child.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if line.len() > 1024 * 1024 {
                break;
            }
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                if tx.send(v).is_err() {
                    break;
                }
            }
        }
    });
    let mut rows = Vec::new();
    let mut timeout = false;
    loop {
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(row) => rows.push(row),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                timeout = true;
                let _ = child.kill();
                break;
            }
            Err(_) => break,
        }
    }
    let status = child.wait()?;
    if !status.success() && !timeout && rows.is_empty() {
        return Err(io::Error::other("云目录读取进程未完整结束"));
    }
    Ok((rows, timeout))
}
pub fn metadata(path: &Path) -> io::Result<Metadata> {
    if let Some(m) = CACHE.with(|cache| cache.borrow_mut().remove(path)) {
        return Ok(m);
    }
    if !is_cloud(path) || std::env::var_os("MINORMINOR_WORKER").is_none() {
        return fs::symlink_metadata(path).map(|m| Metadata::of(&m));
    }
    let (rows, timeout) = request(path, "stat")?;
    if timeout {
        return Err(io::Error::from_raw_os_error(libc::ETIMEDOUT));
    }
    for row in rows {
        if let Some(m) = row.get("metadata") {
            return serde_json::from_value(m.clone()).map_err(io::Error::other);
        }
        if let Some(code) = row["code"].as_i64() {
            return Err(io::Error::from_raw_os_error(code as i32));
        }
    }
    Err(io::Error::other("云文件元数据不可用"))
}
pub fn directory(path: &Path) -> io::Result<Vec<io::Result<Entry>>> {
    let (rows, timeout) = request(path, "list")?;
    let mut entries = Vec::new();
    for row in rows {
        if let (Some(raw), Some(m)) = (row["path"].as_str(), row.get("metadata")) {
            let p = PathBuf::from(raw);
            if p.parent() != Some(path) {
                continue;
            }
            let m: Metadata = serde_json::from_value(m.clone()).map_err(io::Error::other)?;
            CACHE.with(|cache| cache.borrow_mut().insert(p.clone(), m));
            entries.push(Ok(Entry::Cloud(p)));
        } else if let Some(code) = row["code"].as_i64() {
            entries.push(Err(io::Error::from_raw_os_error(code as i32)));
        }
    }
    if timeout {
        entries.push(Err(io::Error::from_raw_os_error(libc::ETIMEDOUT)));
    }
    Ok(entries)
}
pub enum Entry {
    Local(fs::DirEntry),
    Cloud(PathBuf),
}
impl Entry {
    pub fn path(&self) -> PathBuf {
        match self {
            Self::Local(e) => e.path(),
            Self::Cloud(p) => p.clone(),
        }
    }
    pub fn file_name(&self) -> std::ffi::OsString {
        match self {
            Self::Local(e) => e.file_name(),
            Self::Cloud(p) => p.file_name().unwrap_or_default().to_os_string(),
        }
    }
}
pub fn read_dir(path: &Path) -> io::Result<Box<dyn Iterator<Item = io::Result<Entry>>>> {
    if is_cloud(path) && std::env::var_os("MINORMINOR_WORKER").is_some() {
        Ok(Box::new(directory(path)?.into_iter()))
    } else {
        Ok(Box::new(fs::read_dir(path)?.map(|r| r.map(Entry::Local))))
    }
}
pub fn worker() -> Result<()> {
    let mut input = String::new();
    io::stdin().take(16385).read_to_string(&mut input)?;
    if input.len() > 16384 {
        return Err(fail("目录请求过长"));
    }
    let request: Value = serde_json::from_str(&input)?;
    let path = PathBuf::from(request["path"].as_str().ok_or_else(|| fail("缺少目录"))?);
    let emit = |row: Value| {
        println!("{row}");
        let _ = io::stdout().flush();
    };
    if request["mode"] == "stat" {
        match fs::symlink_metadata(&path) {
            Ok(m) => emit(json!({"path":path,"metadata":Metadata::of(&m)})),
            Err(e) => emit(json!({"code":e.raw_os_error().unwrap_or(libc::EIO)})),
        }
        return Ok(());
    }
    if fs::canonicalize(&path).ok().as_ref() != Some(&path) {
        emit(json!({"code":libc::ELOOP}));
        return Ok(());
    }
    match fs::read_dir(&path) {
        Ok(entries) => {
            for entry in entries {
                match entry {
                    Ok(entry) => match fs::symlink_metadata(entry.path()) {
                        Ok(m) => emit(json!({"path":entry.path(),"metadata":Metadata::of(&m)})),
                        Err(e) => emit(json!({"code":e.raw_os_error().unwrap_or(libc::EIO)})),
                    },
                    Err(e) => emit(json!({"code":e.raw_os_error().unwrap_or(libc::EIO)})),
                }
            }
        }
        Err(e) => emit(json!({"code":e.raw_os_error().unwrap_or(libc::EIO)})),
    }
    Ok(())
}
