//! Lossless, streaming macOS filesystem compression of owned scan snapshots.
//!
//! The type-4 decmpfs resource-fork layout follows brkirch's GPL-3.0 afsctool:
//! https://github.com/jrk/afsctool/blob/d81014e9b6df9582da2a523e89b06523af517496/afsctool.c
//! Only a newly-created destination is touched. The original is never modified.
use crate::Result;
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Compressed {
    pub logical_bytes: u64,
    pub allocated_bytes: u64,
}

pub fn is_compressed(path: &Path) -> bool {
    #[cfg(target_os = "macos")]
    {
        use std::os::macos::fs::MetadataExt;
        std::fs::metadata(path).is_ok_and(|m| m.st_flags() & libc::UF_COMPRESSED != 0)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = path;
        false
    }
}

pub fn copy(
    source: &Path,
    target: &Path,
    progress: impl FnMut(&str, u64, u64) -> Result<()>,
) -> Result<Option<Compressed>> {
    copy_with_level(source, target, 6, progress)
}

pub fn copy_with_level(
    source: &Path,
    target: &Path,
    level: u32,
    progress: impl FnMut(&str, u64, u64) -> Result<()>,
) -> Result<Option<Compressed>> {
    if !(1..=9).contains(&level) {
        return Err(crate::fail("索引压缩级别无效"));
    }
    #[cfg(target_os = "macos")]
    {
        macos::copy(source, target, level, progress)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (source, target, level, progress);
        Ok(None)
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use crate::{fail, index::Identity};
    use flate2::{write::ZlibEncoder, Compression};
    use std::{
        fs::{self, File, OpenOptions},
        io::{Read, Seek, SeekFrom, Write},
        os::{
            fd::AsRawFd,
            unix::fs::{MetadataExt, OpenOptionsExt},
        },
        path::PathBuf,
    };

    const BLOCK: usize = 65536;
    const BLOCK_TABLE: u64 = 0x104;
    const MAGIC: u32 = 0x636d7066;

    struct Destination {
        path: PathBuf,
        keep: bool,
    }
    impl Drop for Destination {
        fn drop(&mut self) {
            if !self.keep {
                let _ = fs::remove_file(&self.path);
            }
        }
    }

    fn encode_batch(data: Vec<u8>, level: u32) -> Result<Vec<Vec<u8>>> {
        let mut blocks = Vec::with_capacity(data.len().div_ceil(BLOCK));
        for raw in data.chunks(BLOCK) {
            let mut encoder =
                ZlibEncoder::new(Vec::with_capacity(raw.len()), Compression::new(level));
            encoder.write_all(raw)?;
            let compressed = encoder.finish()?;
            blocks.push(if compressed.len() > raw.len() {
                let mut plain = Vec::with_capacity(raw.len() + 1);
                plain.push(0xff);
                plain.extend_from_slice(raw);
                plain
            } else {
                compressed
            });
        }
        Ok(blocks)
    }
    fn append_block(fork: &mut File, table: &mut Vec<u8>, encoded: &[u8]) -> Result<bool> {
        let position = fork.stream_position()?;
        if position + encoded.len() as u64 + 50 > i32::MAX as u64 {
            return Ok(false);
        }
        table.extend_from_slice(&u32::try_from(position - BLOCK_TABLE)?.to_le_bytes());
        table.extend_from_slice(&u32::try_from(encoded.len())?.to_le_bytes());
        fork.write_all(encoded)?;
        Ok(true)
    }
    fn encode_stream(
        input: &mut File,
        fork: &mut File,
        table: &mut Vec<u8>,
        size: u64,
        level: u32,
        progress: &mut impl FnMut(&str, u64, u64) -> Result<()>,
    ) -> Result<bool> {
        let workers = std::thread::available_parallelism()
            .map(|n| n.get().min(4))
            .unwrap_or(1);
        if size < 8 * 1024 * 1024 || workers == 1 {
            let mut consumed = 0;
            while consumed < size {
                let length = (size - consumed).min(BLOCK as u64) as usize;
                let mut raw = vec![0; length];
                input.read_exact(&mut raw)?;
                let blocks = encode_batch(raw, level)?;
                if !append_block(fork, table, &blocks[0])? {
                    return Ok(false);
                }
                consumed += length as u64;
                progress("compress", consumed, size)?;
            }
            return Ok(true);
        }
        // Independent 64 KiB blocks retain the same on-disk format. At most
        // four 1 MiB input batches and four encoded batches are in flight.
        std::thread::scope(|scope| -> Result<bool> {
            let mut ports = Vec::with_capacity(workers);
            for _ in 0..workers {
                let (send, receive) = std::sync::mpsc::sync_channel::<Vec<u8>>(1);
                let (reply, answers) = std::sync::mpsc::sync_channel::<Result<Vec<Vec<u8>>>>(1);
                scope.spawn(move || {
                    while let Ok(data) = receive.recv() {
                        if reply.send(encode_batch(data, level)).is_err() {
                            break;
                        }
                    }
                });
                ports.push((send, answers));
            }
            let mut submitted = 0;
            let mut consumed = 0;
            while submitted < size {
                let mut jobs = 0;
                for (send, _) in &ports {
                    if submitted == size {
                        break;
                    }
                    let length = (size - submitted).min((16 * BLOCK) as u64) as usize;
                    let mut raw = vec![0; length];
                    input.read_exact(&mut raw)?;
                    send.send(raw).map_err(|_| fail("索引压缩进程意外结束"))?;
                    submitted += length as u64;
                    jobs += 1;
                }
                for (_, answers) in ports.iter().take(jobs) {
                    let blocks = answers.recv().map_err(|_| fail("索引压缩进程意外结束"))??;
                    for block in blocks {
                        if !append_block(fork, table, &block)? {
                            return Ok(false);
                        }
                        consumed += (size - consumed).min(BLOCK as u64);
                        progress("compress", consumed, size)?;
                    }
                }
            }
            Ok(true)
        })
    }

    pub fn copy(
        source: &Path,
        target: &Path,
        level: u32,
        mut progress: impl FnMut(&str, u64, u64) -> Result<()>,
    ) -> Result<Option<Compressed>> {
        use std::{
            ffi::{CStr, CString},
            os::unix::ffi::OsStrExt,
        };
        let parent = target.parent().unwrap_or(Path::new("."));
        let native = CString::new(parent.as_os_str().as_bytes())?;
        let mut info = std::mem::MaybeUninit::<libc::statfs>::uninit();
        if unsafe { libc::statfs(native.as_ptr(), info.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let info = unsafe { info.assume_init() };
        let kind = unsafe { CStr::from_ptr(info.f_fstypename.as_ptr()) }.to_bytes();
        if kind != b"apfs" && kind != b"hfs" {
            return Ok(None);
        }
        let mut input = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(source)?;
        let metadata = input.metadata()?;
        if !metadata.is_file() {
            return Err(fail("索引不是普通文件"));
        }
        let before = Identity::of(&metadata);
        let size = metadata.len();
        if size == 0 {
            return Ok(None);
        }
        let blocks = size.div_ceil(BLOCK as u64);
        let table_end = BLOCK_TABLE
            .checked_add(4)
            .and_then(|n| n.checked_add(blocks.checked_mul(8)?))
            .ok_or_else(|| fail("索引压缩块表超出格式范围"))?;
        if blocks > u32::MAX as u64 || table_end > i32::MAX as u64 {
            return Ok(None);
        }
        let output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(target)?;
        let mut cleanup = Destination {
            path: target.into(),
            keep: false,
        };
        drop(output);
        // Resource-fork data is streamed; neither the input nor its compressed
        // form needs to fit in RAM. No full-size uncompressed copy is allocated.
        let mut fork = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(target.join("..namedfork/rsrc"))?;
        fork.set_len(table_end)?;
        fork.seek(SeekFrom::Start(table_end))?;
        let mut table = Vec::with_capacity(blocks as usize * 8);
        progress("compress", 0, size)?;
        if !encode_stream(
            &mut input,
            &mut fork,
            &mut table,
            size,
            level,
            &mut progress,
        )? {
            return Ok(None);
        }
        let end = fork.stream_position()?;
        if end + 50 >= size {
            return Ok(None);
        }
        let mut header = [0_u8; 0x104];
        header[0..4].copy_from_slice(&0x100_u32.to_be_bytes());
        header[4..8].copy_from_slice(&u32::try_from(end)?.to_be_bytes());
        header[8..12].copy_from_slice(&u32::try_from(end - 0x100)?.to_be_bytes());
        header[12..16].copy_from_slice(&50_u32.to_be_bytes());
        header[0x100..0x104].copy_from_slice(&u32::try_from(end - BLOCK_TABLE)?.to_be_bytes());
        fork.seek(SeekFrom::Start(0))?;
        fork.write_all(&header)?;
        fork.write_all(&u32::try_from(blocks)?.to_le_bytes())?;
        fork.write_all(&table)?;
        let mut tail = [0_u8; 50];
        tail[24..26].copy_from_slice(&0x1c_u16.to_be_bytes());
        tail[26..28].copy_from_slice(&0x32_u16.to_be_bytes());
        tail[30..34].copy_from_slice(&MAGIC.to_be_bytes());
        tail[34..38].copy_from_slice(&10_u32.to_be_bytes());
        tail[38..46].copy_from_slice(&0xffff0100_u64.to_le_bytes());
        fork.seek(SeekFrom::Start(end))?;
        fork.write_all(&tail)?;
        fork.sync_all()?;
        drop(fork);
        let mut attribute = Vec::with_capacity(16);
        attribute.extend_from_slice(&MAGIC.to_le_bytes());
        attribute.extend_from_slice(&4_u32.to_le_bytes());
        attribute.extend_from_slice(&size.to_le_bytes());
        let file = File::open(target)?;
        // All writable handles are closed before enabling compression. Opening
        // a compressed file for writing would immediately expand it on macOS.
        let code = unsafe {
            libc::fsetxattr(
                file.as_raw_fd(),
                c"com.apple.decmpfs".as_ptr(),
                attribute.as_ptr().cast(),
                attribute.len(),
                0,
                libc::XATTR_CREATE,
            )
        };
        if code != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if unsafe { libc::fchflags(file.as_raw_fd(), libc::UF_COMPRESSED) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        file.sync_all()?;
        drop(file);
        let mut decoded = File::open(target)?;
        if decoded.metadata()?.len() != size {
            return Err(fail("压缩索引长度核对失败，原索引已保留"));
        }
        input.seek(SeekFrom::Start(0))?;
        let mut buffer = vec![0_u8; BLOCK];
        let mut decoded_buffer = vec![0_u8; BLOCK];
        let mut compared = 0_u64;
        progress("verify", 0, size)?;
        while compared < size {
            let length = (size - compared).min(BLOCK as u64) as usize;
            input.read_exact(&mut buffer[..length])?;
            decoded.read_exact(&mut decoded_buffer[..length])?;
            if buffer[..length] != decoded_buffer[..length] {
                return Err(fail("压缩索引逐字节核对失败，原索引已保留"));
            }
            compared += length as u64;
            progress("verify", compared, size)?;
        }
        if Identity::of(&input.metadata()?) != before {
            return Err(fail("压缩期间索引发生变化，原索引已保留"));
        }
        let result = Compressed {
            logical_bytes: size,
            allocated_bytes: decoded.metadata()?.blocks() * 512,
        };
        cleanup.keep = true;
        Ok(Some(result))
    }
}
