use crate::{
    config::{Selection, Source},
    markers::MAX_MERGED_SIZE,
    view::View,
};
use std::{fs, io::Write, path::Path, sync::Arc};
fn setup(name: &str, base: &[u8], overlay: &[u8]) -> (tempfile::TempDir, Arc<View>) {
    fs::create_dir_all(".tmp").unwrap();
    let temp = tempfile::tempdir_in(".tmp").unwrap();
    for root in ["project", "overlay"] {
        fs::create_dir(temp.path().join(root)).unwrap();
    }
    fs::write(temp.path().join("project").join(name), base).unwrap();
    fs::write(temp.path().join("overlay").join(name), overlay).unwrap();
    let view = View::open(&Selection {
        root: temp.path().into(),
        project: "project".into(),
        target: temp.path().join("project"),
        markers: vec![],
        sources: vec![Source {
            name: "overlay".into(),
            path: temp.path().join("overlay"),
            glob: "**/*".into(),
        }],
    })
    .unwrap();
    (temp, Arc::new(view))
}
#[test]
fn unmarked_staging_prefix_append_flush_snapshot_and_limits() {
    let (temp, view) = setup("plain.txt", b"base\n", b"overlay\n");
    let path = Path::new("plain.txt");
    let old = view.open_file(path, libc::O_RDONLY, 0).unwrap();
    let writer = view
        .open_file(path, libc::O_RDWR | libc::O_APPEND, 0)
        .unwrap();
    assert!(writer.direct_io());
    assert_eq!(writer.read(0, 200).unwrap(), b"base\noverlay\n");
    writer.write(0, b"appended").unwrap();
    assert_eq!(view.metadata(path, 0).unwrap().size, 21);
    assert_eq!(
        fs::read(temp.path().join("overlay/plain.txt")).unwrap(),
        b"overlay\n"
    );
    writer.flush().unwrap();
    writer.fsync(false).unwrap();
    assert_eq!(
        fs::read(temp.path().join("overlay/plain.txt")).unwrap(),
        b"overlay\nappended"
    );
    assert_eq!(old.read(0, 200).unwrap(), b"base\noverlay\n");
    assert_eq!(writer.metadata().unwrap().nlink, 2);
    assert_eq!(writer.read(1000, 4).unwrap(), b"");
    writer.truncate(0).unwrap();
    writer.flush().unwrap();
    assert_eq!(writer.read(0, 200).unwrap(), b"base\n");
    assert_eq!(
        fs::read(temp.path().join("overlay/plain.txt")).unwrap(),
        b""
    );
    assert_eq!(
        writer
            .truncate((MAX_MERGED_SIZE + 1) as u64)
            .unwrap_err()
            .raw_os_error(),
        Some(libc::EFBIG)
    );
    let invalid = view.open_file(path, libc::O_RDWR, 0).unwrap();
    assert_eq!(
        invalid.write(0, b"WRONG").unwrap_err().raw_os_error(),
        Some(libc::EPERM)
    );
    assert!(invalid.flush().is_err());
    assert_eq!(
        old.write(0, b"x").unwrap_err().raw_os_error(),
        Some(libc::EBADF)
    );
    let write_only = view.open_file(path, libc::O_WRONLY, 0).unwrap();
    assert!(write_only.read(0, 1).is_err());
    assert!(write_only.write(MAX_MERGED_SIZE as u64, b"x").is_err());
}
#[test]
fn marked_two_way_save_and_truncate_only_final_regression() {
    let (temp, view) = setup("AGENTS.md", b"base\n", b"overlay\n");
    let path = Path::new("AGENTS.md");
    let handle = view.open_file(path, libc::O_RDWR, 0).unwrap();
    let rendered = String::from_utf8(handle.read(0, 10000).unwrap()).unwrap();
    let changed = rendered
        .replace("base\n", "changed base\n")
        .replace("overlay\n", "changed overlay\n");
    handle.truncate(changed.len() as u64).unwrap();
    handle.write(0, changed.as_bytes()).unwrap();
    handle.flush().unwrap();
    assert_eq!(
        fs::read(temp.path().join("project/AGENTS.md")).unwrap(),
        b"changed base\n"
    );
    assert_eq!(
        fs::read(temp.path().join("overlay/AGENTS.md")).unwrap(),
        b"changed overlay\n"
    );
    handle.truncate(0).unwrap();
    handle.flush().unwrap();
    assert_eq!(
        fs::read(temp.path().join("project/AGENTS.md")).unwrap(),
        b"changed base\n"
    );
    assert_eq!(
        fs::read(temp.path().join("overlay/AGENTS.md")).unwrap(),
        b""
    );
    assert!(
        handle
            .read(0, 10000)
            .unwrap()
            .starts_with(b"changed base\n<!-- BEGIN")
    );
    let corrupted = view
        .open_file(path, libc::O_WRONLY | libc::O_TRUNC, 0)
        .unwrap();
    corrupted.write(0, b"missing markers").unwrap();
    assert_eq!(
        corrupted.flush().unwrap_err().raw_os_error(),
        Some(libc::EPERM)
    );
}
#[test]
fn native_descriptors_unlink_and_binary_notice_and_sparse_limits() {
    let (temp, view) = setup("binary.bin", b"text", b"\0binary");
    let path = Path::new("binary.bin");
    let notice = view.open_file(path, libc::O_RDWR, 0).unwrap();
    assert!(notice.direct_io());
    let data = notice.read(0, 10000).unwrap();
    assert!(
        String::from_utf8(data.clone())
            .unwrap()
            .contains("Copies of this path:")
    );
    assert_eq!(notice.metadata().unwrap().size, data.len() as u64);
    assert_eq!(
        notice.write(0, b"x").unwrap_err().raw_os_error(),
        Some(libc::EPERM)
    );
    assert!(notice.truncate(0).is_err());
    let initial_notice = notice.metadata().unwrap();
    for root in ["project", "overlay"] {
        fs::remove_file(temp.path().join(root).join("binary.bin")).unwrap();
    }
    assert_eq!(notice.read(0, 10000).unwrap(), data);
    let unlinked_notice = notice.metadata().unwrap();
    assert_eq!(unlinked_notice.ino, initial_notice.ino);
    assert_eq!(unlinked_notice.size, initial_notice.size);
    fs::write(temp.path().join("project/only"), b"single").unwrap();
    let single = view.open_file(Path::new("only"), libc::O_RDWR, 0).unwrap();
    assert!(!single.direct_io());
    fs::remove_file(temp.path().join("project/only")).unwrap();
    single.write(0, b"native").unwrap();
    assert_eq!(single.read(0, 100).unwrap(), b"native");
    assert_eq!(single.metadata().unwrap().nlink, 0);
    for root in ["project", "overlay"] {
        let mut file = fs::File::create(temp.path().join(root).join("huge.txt")).unwrap();
        file.write_all(&[b'a'; 512]).unwrap();
        file.set_len((MAX_MERGED_SIZE + 1) as u64).unwrap();
    }
    view.refresh_paths().unwrap();
    assert_eq!(
        view.open_file(Path::new("huge.txt"), libc::O_RDONLY, 0)
            .err()
            .unwrap()
            .raw_os_error(),
        Some(libc::EFBIG)
    );
    assert_eq!(
        view.open_file(Path::new("."), libc::O_RDONLY, 0)
            .err()
            .unwrap()
            .raw_os_error(),
        Some(libc::EISDIR)
    );
}

#[test]
fn unreadable_contributions_keep_path_metadata_available() {
    use std::os::unix::fs::PermissionsExt;
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    for name in ["plain.txt", "AGENTS.md"] {
        let (temp, view) = setup(name, b"BASE-", b"SHARED");
        let source = temp.path().join("overlay").join(name);
        fs::set_permissions(&source, fs::Permissions::from_mode(0o0)).unwrap();
        let result = view.metadata(Path::new(name), 0);
        let opened = view.open_file(Path::new(name), libc::O_RDONLY, 0);
        fs::set_permissions(&source, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(result.unwrap().size, 11);
        assert_eq!(opened.err().unwrap().raw_os_error(), Some(libc::EIO));
    }
}

#[test]
fn released_handles_reject_operations_for_native_merged_and_notice_files() {
    let (temp, view) = setup("plain.txt", b"base", b"overlay");
    fs::write(temp.path().join("project/native"), b"native").unwrap();
    fs::write(temp.path().join("project/binary"), b"base").unwrap();
    fs::write(temp.path().join("overlay/binary"), b"\0binary").unwrap();
    view.refresh_paths().unwrap();
    for name in ["plain.txt", "native", "binary"] {
        let file = view.open_file(Path::new(name), libc::O_RDWR, 0).unwrap();
        file.flush().unwrap();
        file.release().unwrap();
        assert_eq!(
            file.read(0, 10).unwrap_err().raw_os_error(),
            Some(libc::EBADF),
            "{name}"
        );
        assert_eq!(
            file.metadata().unwrap_err().raw_os_error(),
            Some(libc::EBADF),
            "{name}"
        );
        assert_eq!(
            file.write(0, b"data").unwrap_err().raw_os_error(),
            Some(libc::EBADF),
            "{name}"
        );
        assert_eq!(
            file.truncate(0).unwrap_err().raw_os_error(),
            Some(libc::EBADF),
            "{name}"
        );
        assert_eq!(
            file.flush().unwrap_err().raw_os_error(),
            Some(libc::EBADF),
            "{name}"
        );
    }
}
#[test]
fn explicit_close_releases_descriptor_and_retains_independent_native_failures() {
    use crate::file::{close_native, finish_io};
    use std::os::fd::AsRawFd;
    let temp = tempfile::tempdir_in(".tmp").unwrap();
    let file = std::fs::File::create(temp.path().join("closed")).unwrap();
    let contender = std::fs::File::open(temp.path().join("closed")).unwrap();
    // SAFETY: both flock calls borrow live owned file descriptors.
    assert_eq!(
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    assert_eq!(
        unsafe { libc::flock(contender.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        -1
    );
    close_native(file).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        if unsafe { libc::flock(contender.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            break;
        }
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EAGAIN)
        );
        assert!(
            std::time::Instant::now() < deadline,
            "closed description retained its lock"
        );
        std::thread::yield_now();
    }
    let first = std::fs::remove_file(temp.path()).unwrap_err();
    let second = std::fs::read(temp.path().join("missing")).unwrap_err();
    let error = finish_io(Err(first), Err(second)).unwrap_err();
    let failures = error
        .get_ref()
        .unwrap()
        .downcast_ref::<crate::Failures>()
        .unwrap();
    assert_eq!(failures.0.len(), 2);
    assert!(
        matches!(&failures.0[0], crate::Error::Io(error) if error.raw_os_error() == Some(libc::EISDIR))
    );
    assert!(
        matches!(&failures.0[1], crate::Error::Io(error) if error.raw_os_error() == Some(libc::ENOENT))
    );
    assert_eq!(
        finish_io(
            Ok(()),
            Err(std::fs::read(temp.path().join("missing")).unwrap_err())
        )
        .unwrap_err()
        .raw_os_error(),
        Some(libc::ENOENT)
    );
    finish_io(Ok(()), Ok(())).unwrap();
}
