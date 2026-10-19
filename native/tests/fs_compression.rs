#![cfg(target_os = "macos")]
use minorminor_core::fs_compression;
use std::{fs, io::Write, os::macos::fs::MetadataExt};

#[test]
fn kernel_reads_every_compressed_byte_and_failed_copies_leave_the_original_intact() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let target = temp.path().join("compressed");
    let mut data = Vec::new();
    for n in 0..280000_u32 {
        data.extend_from_slice(
            format!("/fixture/project-{}/metadata-file-{}\0", n / 100, n).as_bytes(),
        );
    }
    // Exercise partial final blocks and multi-block resource-fork offsets.
    data.extend_from_slice(b"final partial block");
    fs::write(&source, &data).unwrap();
    let result = fs_compression::copy(&source, &target, |_, _, _| Ok(()))
        .unwrap()
        .unwrap();
    assert!(target.metadata().unwrap().st_flags() & libc::UF_COMPRESSED != 0);
    assert_eq!(fs::read(&target).unwrap(), data);
    assert_eq!(fs::read(&source).unwrap(), data);
    assert_eq!(result.logical_bytes, data.len() as u64);
    assert!(result.allocated_bytes < result.logical_bytes / 2);
    let failed = temp.path().join("interrupted");
    assert!(
        fs_compression::copy(&source, &failed, |phase, _, _| if phase == "verify" {
            Err(minorminor_core::fail("interrupted"))
        } else {
            Ok(())
        })
        .is_err()
    );
    assert!(!failed.exists());
    assert_eq!(fs::read(&source).unwrap(), data);
    let existing = temp.path().join("existing");
    fs::write(&existing, b"keep").unwrap();
    assert!(fs_compression::copy(&source, &existing, |_, _, _| Ok(())).is_err());
    assert_eq!(fs::read(existing).unwrap(), b"keep");
}

#[test]
fn empty_or_incompressible_input_never_expands_the_stored_snapshot() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let target = temp.path().join("target");
    fs::write(&source, b"").unwrap();
    assert!(fs_compression::copy(&source, &target, |_, _, _| Ok(()))
        .unwrap()
        .is_none());
    assert!(!target.exists());
    let mut file = fs::File::create(&source).unwrap();
    let mut seed = 1234567_u64;
    for _ in 0..33000 {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        file.write_all(&seed.to_le_bytes()).unwrap();
    }
    drop(file);
    assert!(fs_compression::copy(&source, &target, |_, _, _| Ok(()))
        .unwrap()
        .is_none());
    assert!(!target.exists());
}
