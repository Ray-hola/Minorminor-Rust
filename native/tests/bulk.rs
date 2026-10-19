#![cfg(target_os = "macos")]
use minorminor_core::{bulk, cloud, fs_compression, index};
use std::{
    ffi::OsString,
    fs,
    os::unix::{
        ffi::OsStringExt,
        fs::{symlink, MetadataExt},
    },
};
#[test]
fn batch_metadata_matches_lstat_exactly_for_forks_links_sparse_and_unusual_names() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path();
    fs::write(root.join("normal"), b"example").unwrap();
    fs::hard_link(root.join("normal"), root.join("hard")).unwrap();
    fs::create_dir(root.join("folder")).unwrap();
    symlink(root.join("normal"), root.join("link")).unwrap();
    fs::write(root.join("中文\n换行"), b"utf8").unwrap();
    fs::write(
        root.join(OsString::from_vec(vec![255, 254])),
        b"invalid utf8",
    )
    .unwrap_or_else(|e| assert_eq!(e.raw_os_error(), Some(libc::EILSEQ)));
    let sparse = fs::File::create(root.join("sparse")).unwrap();
    sparse.set_len(1 << 30).unwrap();
    drop(sparse);
    let file = root.join("forked");
    fs::write(&file, b"data").unwrap();
    fs::write(file.join("..namedfork/rsrc"), vec![42; 32768]).unwrap();
    let source = root.join("source");
    fs::write(&source, vec![0; 100000]).unwrap();
    fs_compression::copy(&source, &root.join("compressed"), |_, _, _| Ok(()))
        .unwrap()
        .unwrap();
    let expected = cloud::metadata(root).unwrap();
    let handle = minorminor_core::directory::Handle::root(root, &expected).unwrap();
    let entries = bulk::directory(root, &expected).expect("APFS supports bulk metadata");
    assert_eq!(entries.len(), fs::read_dir(root).unwrap().count());
    for entry in entries {
        let actual = entry.metadata().unwrap();
        assert_eq!(
            serde_json::to_value(handle.metadata(&entry.path()).unwrap().identity).unwrap(),
            serde_json::to_value(&actual.identity).unwrap()
        );
        let m = fs::symlink_metadata(entry.path()).unwrap();
        assert_eq!(
            serde_json::to_value(&actual.identity).unwrap(),
            serde_json::to_value(index::Identity::of(&m)).unwrap(),
            "{}",
            entry.path().display()
        );
        assert_eq!(actual.blocks(), m.blocks(), "{}", entry.path().display());
        assert_eq!(actual.is_dir(), m.is_dir());
        assert_eq!(actual.is_file(), m.is_file());
        assert_eq!(actual.is_symlink(), m.file_type().is_symlink());
    }
    let wrong = cloud::metadata(&root.join("folder")).unwrap();
    assert!(bulk::directory(root, &wrong).is_none());
    assert!(bulk::directory(&root.join("link"), &expected).is_none());
}

#[test]
fn directory_handles_reject_replaced_links_and_never_resolve_redirected_ancestors() {
    use minorminor_core::directory::Handle;
    let t = tempfile::tempdir().unwrap();
    let root = t.path();
    fs::create_dir(root.join("child")).unwrap();
    fs::write(root.join("child/keep"), b"keep").unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("never-follow"), b"outside").unwrap();
    let parent = Handle::root(root, &cloud::metadata(root).unwrap()).unwrap();
    let expected = cloud::metadata(&root.join("child")).unwrap();
    let child = parent
        .child(std::ffi::OsStr::new("child"), &expected)
        .unwrap();
    fs::rename(root.join("child"), root.join("original")).unwrap();
    symlink(outside.path(), root.join("child")).unwrap();
    assert!(parent
        .child(std::ffi::OsStr::new("child"), &expected)
        .is_err());
    let entries = child
        .entries(&root.join("child"))
        .unwrap()
        .map(|r| r.unwrap().file_name())
        .collect::<Vec<_>>();
    assert_eq!(entries, vec![std::ffi::OsString::from("keep")]);
    assert!(parent
        .child(std::ffi::OsStr::new("../"), &expected)
        .is_err());
}
