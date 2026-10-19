//! Descriptor-relative traversal. Ancestors are never re-resolved between reads.
use crate::cloud::{Entry, Metadata};
use std::{
    ffi::{CString, OsStr},
    fs::{File, OpenOptions},
    io,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt},
        },
    },
    path::Path,
    sync::Arc,
};
#[derive(Clone)]
pub struct Handle(Arc<File>);
impl Handle {
    pub fn root(path: &Path, expected: &Metadata) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(path)?;
        Self::checked(file, expected)
    }
    pub fn child(&self, name: &OsStr, expected: &Metadata) -> io::Result<Self> {
        let bytes = name.as_bytes();
        if bytes.is_empty() || bytes == b"." || bytes == b".." || bytes.contains(&b'/') {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        let name = CString::new(bytes)?;
        let fd = unsafe {
            libc::openat(
                self.0.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Self::checked(unsafe { File::from_raw_fd(fd) }, expected)
    }
    fn checked(file: File, expected: &Metadata) -> io::Result<Self> {
        let m = file.metadata()?;
        if !m.is_dir() || m.dev() != expected.dev() || m.ino() != expected.ino() {
            return Err(io::Error::from_raw_os_error(libc::ESTALE));
        }
        Ok(Self(Arc::new(file)))
    }
    pub fn entries(&self, path: &Path) -> io::Result<Box<dyn Iterator<Item = io::Result<Entry>>>> {
        #[cfg(target_os = "macos")]
        {
            if let Some(entries) = crate::bulk::from_fd(&self.0, path) {
                return Ok(Box::new(
                    entries
                        .into_iter()
                        .map(|e| {
                            Ok(match e {
                                Entry::Bulk(p, None) => Entry::At(p, self.clone()),
                                other => other,
                            })
                        })
                        .collect::<Vec<_>>()
                        .into_iter(),
                ));
            }
            let duplicate = unsafe { libc::fcntl(self.0.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
            if duplicate < 0 {
                return Err(io::Error::last_os_error());
            }
            // dup shares the directory offset with a failed/oversized bulk attempt.
            if unsafe { libc::lseek(duplicate, 0, libc::SEEK_SET) } < 0 {
                let e = io::Error::last_os_error();
                unsafe { libc::close(duplicate) };
                return Err(e);
            }
            let dir = unsafe { libc::fdopendir(duplicate) };
            if dir.is_null() {
                let e = io::Error::last_os_error();
                unsafe { libc::close(duplicate) };
                return Err(e);
            }
            Ok(Box::new(Records {
                dir,
                parent: self.clone(),
                path: path.into(),
                done: false,
            }))
        }
        #[cfg(not(target_os = "macos"))]
        {
            crate::cloud::read_dir(path)
        }
    }
    #[cfg(target_os = "macos")]
    pub fn metadata(&self, path: &Path) -> io::Result<Metadata> {
        let name = CString::new(
            path.file_name()
                .ok_or_else(|| io::Error::from_raw_os_error(libc::EINVAL))?
                .as_bytes(),
        )?;
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe {
            libc::fstatat(
                self.0.as_raw_fd(),
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        let m = unsafe { stat.assume_init() };
        if m.st_size < 0 || m.st_blocks < 0 {
            return Err(io::Error::from_raw_os_error(libc::EIO));
        }
        let kind = m.st_mode & libc::S_IFMT;
        Ok(Metadata {
            identity: crate::index::Identity {
                dev: m.st_dev as u32 as u64,
                ino: m.st_ino,
                size: m.st_size as u64,
                mtime: m.st_mtime,
                mtime_ns: m.st_mtime_nsec,
                ctime: m.st_ctime,
                ctime_ns: m.st_ctime_nsec,
                nlink: m.st_nlink as u64,
            },
            allocated_blocks: m.st_blocks as u64,
            directory: kind == libc::S_IFDIR,
            file: kind == libc::S_IFREG,
            link: kind == libc::S_IFLNK,
        })
    }
}
#[cfg(target_os = "macos")]
struct Records {
    dir: *mut libc::DIR,
    parent: Handle,
    path: std::path::PathBuf,
    done: bool,
}
#[cfg(target_os = "macos")]
impl Drop for Records {
    fn drop(&mut self) {
        unsafe { libc::closedir(self.dir) };
    }
}
#[cfg(target_os = "macos")]
impl Iterator for Records {
    type Item = io::Result<Entry>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        loop {
            unsafe { *libc::__error() = 0 };
            let entry = unsafe { libc::readdir(self.dir) };
            if entry.is_null() {
                self.done = true;
                let e = io::Error::last_os_error();
                return if e.raw_os_error() == Some(0) {
                    None
                } else {
                    Some(Err(e))
                };
            }
            let entry = unsafe { &*entry };
            let n = entry.d_namlen as usize;
            if n == 0 || n >= entry.d_name.len() {
                self.done = true;
                return Some(Err(io::Error::from_raw_os_error(libc::EIO)));
            }
            let raw = unsafe { std::slice::from_raw_parts(entry.d_name.as_ptr().cast::<u8>(), n) };
            if raw == b"." || raw == b".." {
                continue;
            }
            if raw.contains(&0) || raw.contains(&b'/') {
                self.done = true;
                return Some(Err(io::Error::from_raw_os_error(libc::EIO)));
            }
            return Some(Ok(Entry::At(
                self.path.join(OsStr::from_bytes(raw)),
                self.parent.clone(),
            )));
        }
    }
}

impl AsRawFd for Handle {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.0.as_raw_fd()
    }
}
