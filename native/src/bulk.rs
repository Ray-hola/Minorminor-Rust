//! macOS batch metadata for scan traversal. Missing attributes always fall back
//! to lstat; unsupported or malformed batches fall back to ordinary enumeration.
use crate::cloud::{Entry, Metadata};
use std::path::Path;

pub fn directory(path: &Path, expected: &Metadata) -> Option<Vec<Entry>> {
    #[cfg(target_os = "macos")]
    {
        macos::directory(path, expected).ok()
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (path, expected);
        None
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn from_fd(fd: &std::fs::File, path: &Path) -> Option<Vec<Entry>> {
    macos::read(fd, path).ok()
}
#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use crate::index::Identity;
    use std::{
        ffi::OsString,
        fs::OpenOptions,
        io,
        os::{
            fd::AsRawFd,
            unix::{
                ffi::OsStringExt,
                fs::{MetadataExt, OpenOptionsExt},
            },
        },
    };
    const COMMON: u32 = 0xa2000c0b; // returned attrs, error, name, dev, type, mtime, ctime, fileid
    const FILE: u32 = 0x205; // linkcount, allocation (all forks), data length
    const MAX_ENTRIES: usize = 65536; // retain bounded memory, then use streaming read_dir
    fn invalid() -> io::Error {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "Invalid bulk directory metadata",
        )
    }
    struct Cursor<'a> {
        bytes: &'a [u8],
        at: usize,
    }
    impl Cursor<'_> {
        fn u32(&mut self) -> io::Result<u32> {
            let end = self.at.checked_add(4).ok_or_else(invalid)?;
            let raw = self.bytes.get(self.at..end).ok_or_else(invalid)?;
            self.at = end;
            Ok(u32::from_ne_bytes(raw.try_into().unwrap()))
        }
        fn u64(&mut self) -> io::Result<u64> {
            let end = self.at.checked_add(8).ok_or_else(invalid)?;
            let raw = self.bytes.get(self.at..end).ok_or_else(invalid)?;
            self.at = end;
            Ok(u64::from_ne_bytes(raw.try_into().unwrap()))
        }
    }
    fn parse(raw: &[u8], parent: &Path) -> io::Result<Entry> {
        let mut c = Cursor { bytes: raw, at: 4 };
        let common = c.u32()?;
        let _vol = c.u32()?;
        let _dir = c.u32()?;
        let file = c.u32()?;
        let _fork = c.u32()?;
        // FSOPT_PACK_INVAL_ATTRS preserves requested common slots. File slots
        // are present only for file/symlink records; they are never assumed for dirs.
        if common & 0x20000000 != 0 && c.u32()? != 0 {
            return Err(invalid());
        }
        let base = c.at;
        let offset = c.u32()? as i32 as i64;
        let length = c.u32()? as usize;
        let start = (base as i64)
            .checked_add(offset)
            .filter(|n| *n >= 0)
            .ok_or_else(invalid)? as usize;
        let end = start.checked_add(length).ok_or_else(invalid)?;
        let name = raw
            .get(start..end)
            .filter(|s| s.last() == Some(&0))
            .ok_or_else(invalid)?;
        let name = &name[..name.len() - 1];
        if name.is_empty()
            || name == b"."
            || name == b".."
            || name.contains(&b'/')
            || name.contains(&0)
        {
            return Err(invalid());
        }
        let path = parent.join(OsString::from_vec(name.to_vec()));
        if common & COMMON != COMMON {
            return Ok(Entry::Bulk(path, None));
        }
        let dev = c.u32()? as u64;
        let kind = c.u32()?;
        let mtime = c.u64()? as i64;
        let mtime_ns = c.u64()? as i64;
        let ctime = c.u64()? as i64;
        let ctime_ns = c.u64()? as i64;
        let ino = c.u64()?;
        if kind != 1 && kind != 5 || file & FILE != FILE {
            return Ok(Entry::Bulk(path, None));
        }
        let nlink = c.u32()? as u64;
        let allocated = c.u64()?;
        let size = c.u64()?;
        if allocated > i64::MAX as u64
            || size > i64::MAX as u64
            || allocated % 512 != 0
            || !(0..1_000_000_000).contains(&mtime_ns)
            || !(0..1_000_000_000).contains(&ctime_ns)
        {
            return Ok(Entry::Bulk(path, None));
        }
        Ok(Entry::Bulk(
            path,
            Some(Metadata {
                identity: Identity {
                    dev,
                    ino,
                    size,
                    mtime,
                    mtime_ns,
                    ctime,
                    ctime_ns,
                    nlink,
                },
                allocated_blocks: allocated / 512,
                directory: false,
                file: kind == 1,
                link: kind == 5,
            }),
        ))
    }
    pub(super) fn directory(path: &Path, expected: &Metadata) -> io::Result<Vec<Entry>> {
        let fd = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(path)?;
        let m = fd.metadata()?;
        if m.dev() != expected.dev() || m.ino() != expected.ino() {
            return Err(invalid());
        }
        let entries = read(&fd, path)?;
        // Opening a descriptor must not accidentally redirect a changed path.
        let current = std::fs::symlink_metadata(path)?;
        if current.file_type().is_symlink()
            || current.dev() != expected.dev()
            || current.ino() != expected.ino()
        {
            return Err(invalid());
        }
        Ok(entries)
    }
    thread_local! {static BUFFER:std::cell::RefCell<Vec<u8>>=std::cell::RefCell::new(vec![0u8;128*1024]);}
    pub(super) fn read(fd: &std::fs::File, path: &Path) -> io::Result<Vec<Entry>> {
        BUFFER.with(|buffer| {
            let mut buffer = buffer.borrow_mut();
            let mut attrs = libc::attrlist {
                bitmapcount: 5,
                reserved: 0,
                commonattr: COMMON,
                volattr: 0,
                dirattr: 0,
                fileattr: FILE,
                forkattr: 0,
            };
            let mut entries = Vec::new();
            loop {
                let count = unsafe {
                    libc::getattrlistbulk(
                        fd.as_raw_fd(),
                        (&mut attrs as *mut libc::attrlist).cast(),
                        buffer.as_mut_ptr().cast(),
                        buffer.len(),
                        libc::FSOPT_PACK_INVAL_ATTRS as u64,
                    )
                };
                if count < 0 {
                    return Err(io::Error::last_os_error());
                }
                if count == 0 {
                    break;
                }
                let mut at = 0usize;
                for _ in 0..count {
                    let length = u32::from_ne_bytes(
                        buffer
                            .get(at..at + 4)
                            .ok_or_else(invalid)?
                            .try_into()
                            .unwrap(),
                    ) as usize;
                    if length < 24 || length % 4 != 0 {
                        return Err(invalid());
                    }
                    let end = at.checked_add(length).ok_or_else(invalid)?;
                    entries.push(parse(buffer.get(at..end).ok_or_else(invalid)?, path)?);
                    at = end;
                    if entries.len() > MAX_ENTRIES {
                        return Err(invalid());
                    }
                }
            }

            Ok(entries)
        })
    }
}
