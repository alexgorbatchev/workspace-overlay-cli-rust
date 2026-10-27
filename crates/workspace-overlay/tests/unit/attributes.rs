use crate::{
    attributes::Change,
    config::{Selection, Source},
    view::View,
};
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::Path,
    sync::Arc,
    time::{Duration, UNIX_EPOCH},
};

#[test]
fn native_metadata_changes_need_no_read_access_and_preserve_omitted_times() {
    fs::create_dir_all(".tmp").unwrap();
    let temp = tempfile::tempdir_in(".tmp").unwrap();
    fs::write(temp.path().join("private"), b"secret").unwrap();
    let view = Arc::new(
        View::open(&Selection {
            root: temp.path().into(),
            project: "project".into(),
            target: temp.path().into(),
            markers: vec![],
            sources: vec![],
        })
        .unwrap(),
    );
    fs::set_permissions(temp.path().join("private"), fs::Permissions::from_mode(0o0)).unwrap();
    let time = UNIX_EPOCH + Duration::from_secs(1_000_000);
    view.set_attributes(
        Path::new("private"),
        &Change {
            mtime: Some(fuser::TimeOrNow::SpecificTime(time)),
            uid: Some(unsafe { libc::getuid() }),
            gid: Some(unsafe { libc::getgid() }),
            ..Change::default()
        },
        0,
    )
    .unwrap();
    let initial = fs::metadata(temp.path().join("private")).unwrap();
    view.set_attributes(
        Path::new("private"),
        &Change {
            mode: Some(0o640),
            atime: Some(fuser::TimeOrNow::SpecificTime(
                time + Duration::from_secs(12),
            )),
            ..Change::default()
        },
        0,
    )
    .unwrap();
    let changed = fs::metadata(temp.path().join("private")).unwrap();
    assert_eq!(changed.permissions().mode() & 0o777, 0o640);
    assert_eq!(changed.mtime(), initial.mtime());
    assert_eq!(changed.atime(), 1_000_012);
    let file = view
        .open_file(Path::new("private"), libc::O_RDWR, 0)
        .unwrap();
    fs::remove_file(temp.path().join("private")).unwrap();
    file.set_attributes(&Change {
        mode: Some(0o600),
        size: Some(3),
        ..Change::default()
    })
    .unwrap();
    assert_eq!(file.metadata().unwrap().size, 3);
    assert_eq!(file.metadata().unwrap().perm, 0o600);
    file.release().unwrap();
}

#[test]
fn path_truncation_reaches_every_pending_merged_writer() {
    fs::create_dir_all(".tmp").unwrap();
    let temp = tempfile::tempdir_in(".tmp").unwrap();
    for name in ["project", "overlay"] {
        fs::create_dir(temp.path().join(name)).unwrap();
        fs::write(
            temp.path().join(name).join("plain.txt"),
            if name == "project" {
                b"base-"
            } else {
                b"extra"
            },
        )
        .unwrap();
    }
    let view = Arc::new(
        View::open(&Selection {
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
        .unwrap(),
    );
    let first = view
        .open_file(Path::new("plain.txt"), libc::O_RDWR, 0)
        .unwrap();
    let second = view
        .open_file(Path::new("plain.txt"), libc::O_RDWR, 0)
        .unwrap();
    view.set_attributes(
        Path::new("plain.txt"),
        &Change {
            size: Some(0),
            ..Change::default()
        },
        0,
    )
    .unwrap();
    assert!(first.read(0, 100).unwrap().is_empty());
    assert!(second.read(0, 100).unwrap().is_empty());
    first.write(0, b"base-new").unwrap();
    first.flush().unwrap();
    assert_eq!(
        fs::read(temp.path().join("overlay/plain.txt")).unwrap(),
        b"new"
    );
    // A final path truncate without any writer must commit through the same
    // merged validation, rather than truncating the final raw layer to 6.
    drop(first);
    drop(second);
    view.set_attributes(
        Path::new("plain.txt"),
        &Change {
            size: Some(6),
            ..Change::default()
        },
        0,
    )
    .unwrap();
    assert_eq!(
        fs::read(temp.path().join("overlay/plain.txt")).unwrap(),
        b"n"
    );
}

#[test]
fn descriptor_times_ownership_and_path_directories_use_native_metadata() {
    fs::create_dir_all(".tmp").unwrap();
    let temp = tempfile::tempdir_in(".tmp").unwrap();
    fs::write(temp.path().join("file"), b"data").unwrap();
    fs::create_dir(temp.path().join("directory")).unwrap();
    let view = Arc::new(
        View::open(&Selection {
            root: temp.path().into(),
            project: "project".into(),
            target: temp.path().into(),
            markers: vec![],
            sources: vec![],
        })
        .unwrap(),
    );
    let file = view.open_file(Path::new("file"), libc::O_RDWR, 0).unwrap();
    // SAFETY: these read the calling process's real IDs and have no preconditions.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let before_epoch = UNIX_EPOCH - Duration::from_millis(1250);
    file.set_attributes(&Change {
        uid: Some(uid),
        gid: Some(gid),
        atime: Some(fuser::TimeOrNow::SpecificTime(before_epoch)),
        mtime: Some(fuser::TimeOrNow::SpecificTime(
            UNIX_EPOCH - Duration::from_secs(2),
        )),
        ..Change::default()
    })
    .unwrap();
    let metadata = fs::metadata(temp.path().join("file")).unwrap();
    assert_eq!((metadata.atime(), metadata.atime_nsec()), (-2, 750_000_000));
    assert_eq!((metadata.mtime(), metadata.mtime_nsec()), (-2, 0));
    assert_eq!((metadata.uid(), metadata.gid()), (uid, gid));
    file.set_attributes(&Change {
        atime: Some(fuser::TimeOrNow::Now),
        ..Change::default()
    })
    .unwrap();
    assert_eq!(fs::metadata(temp.path().join("file")).unwrap().mtime(), -2);
    file.set_attributes(&Change {
        mtime: Some(fuser::TimeOrNow::Now),
        ..Change::default()
    })
    .unwrap();
    assert!(fs::metadata(temp.path().join("file")).unwrap().mtime() > 1_000_000);
    let attr = view
        .set_attributes(
            Path::new("directory"),
            &Change {
                mode: Some(0o710),
                uid: Some(uid),
                gid: Some(gid),
                atime: Some(fuser::TimeOrNow::Now),
                mtime: Some(fuser::TimeOrNow::SpecificTime(
                    UNIX_EPOCH + Duration::from_secs(1234),
                )),
                ..Change::default()
            },
            0,
        )
        .unwrap();
    assert_eq!(attr.perm, 0o710);
    assert_eq!(
        fs::metadata(temp.path().join("directory")).unwrap().mtime(),
        1234
    );
    for change in [
        Change {
            uid: Some(u32::MAX),
            ..Change::default()
        },
        Change {
            gid: Some(u32::MAX),
            ..Change::default()
        },
    ] {
        assert_eq!(
            view.set_attributes(Path::new("file"), &change, 0)
                .unwrap_err()
                .raw_os_error(),
            Some(libc::EINVAL)
        );
        assert_eq!(
            file.set_attributes(&change).unwrap_err().raw_os_error(),
            Some(libc::EINVAL)
        );
    }
    file.release().unwrap();
}
