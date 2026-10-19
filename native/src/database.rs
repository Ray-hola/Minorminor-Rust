use crate::{
    fail,
    index::{self, Identity},
    Result,
};
use rusqlite::Connection;
use std::{
    fs,
    io::{self, Read},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};
/// A filename suffix is only a candidate; never open lock or pointer metadata as SQL.
pub fn format(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(b"SQLite format 3\0") {
        "sqlite"
    } else if bytes.is_empty() {
        "empty"
    } else if bytes.len() < 4096
        && std::str::from_utf8(bytes).is_ok_and(|s| {
            let s = s.trim();
            s.starts_with('/') && !s.contains(['\n', '\r', '\0'])
        })
    {
        "path-reference"
    } else {
        "unknown"
    }
}
pub struct Snapshot {
    pub path: PathBuf,
    pub immutable: bool,
    directory: Option<PathBuf>,
}
impl Drop for Snapshot {
    fn drop(&mut self) {
        if let Some(path) = &self.directory {
            let _ = fs::remove_dir_all(path);
        }
    }
}
impl Snapshot {
    pub fn open(source: &Path, index_db: &Connection) -> Result<Self> {
        let mut last = None;
        for attempt in 0..3 {
            match Self::copy_pair(source, index_db) {
                Ok(snapshot) => return Ok(snapshot),
                Err(e) => {
                    let retry = e.to_string().contains("变化");
                    last = Some(e);
                    if !retry {
                        break;
                    }
                    if attempt < 2 {
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                }
            }
        }
        Err(last.unwrap_or_else(|| fail("无法读取数据库快照")))
    }
    fn copy_pair(source: &Path, index_db: &Connection) -> Result<Self> {
        let wal = PathBuf::from(format!("{}-wal", source.display()));
        if fs::symlink_metadata(&wal)
            .map(|m| m.len() == 0)
            .unwrap_or(true)
        {
            return Ok(Self {
                path: source.into(),
                immutable: true,
                directory: None,
            });
        }
        let data = Path::new(index_db.path().ok_or_else(|| fail("索引没有文件路径"))?)
            .parent()
            .ok_or_else(|| fail("缺少应用目录"))?;
        let directory = data.join(format!(".database-snapshot-{}", crate::id()));
        index::secure_directory(&directory)?;
        let target = directory.join("state.sqlite3");
        let snapshot = Self {
            path: target.clone(),
            immutable: false,
            directory: Some(directory),
        };
        let mut identities = Vec::new();
        for (input, output) in [
            (source.to_path_buf(), target.clone()),
            (
                wal.clone(),
                PathBuf::from(format!("{}-wal", target.display())),
            ),
        ] {
            let before = fs::symlink_metadata(&input)?;
            if !before.is_file()
                || before.file_type().is_symlink()
                || fs::canonicalize(&input)? != input
            {
                return Err(fail("数据库或WAL是链接或特殊文件"));
            }
            let file = fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(&input)?;
            let identity = Identity::of(&before);
            identities.push((input.clone(), identity.clone()));
            if Identity::of(&file.metadata()?) != identity {
                return Err(fail("数据库正在变化，请稍后重新扫描"));
            }
            let mut out = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(output)?;
            let copied = io::copy(&mut (&file).take(before.len()), &mut out)?;
            if copied != before.len()
                || Identity::of(&file.metadata()?) != identity
                || Identity::of(&fs::symlink_metadata(&input)?) != identity
            {
                return Err(fail("数据库正在变化，未采用不完整的快照"));
            }
        }
        // Validate the pair again: main DB checkpointing during the WAL copy is not
        // a consistent snapshot and must never produce misleading associations.
        for (path, identity) in identities {
            if Identity::of(&fs::symlink_metadata(path)?) != identity {
                return Err(fail("数据库或WAL在复制期间变化，请重新扫描"));
            }
        }
        let copied = fs::metadata(&snapshot.path)?;
        let current = fs::metadata(source)?;
        if copied.len() != current.len() {
            return Err(fail("数据库检查点正在变化，请重试"));
        }
        Ok(snapshot)
    }
}
