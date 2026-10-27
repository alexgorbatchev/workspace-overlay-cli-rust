use crate::{
    config::{Selection, Source},
    file::Lock,
    view::View,
};
use std::{fs, path::Path, sync::Arc};

fn acquire_after_last_close(file: &crate::file::OpenFile, lock: Lock) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    loop {
        match file.set_lock(lock, false) {
            Ok(()) => return,
            Err(error)
                if error.raw_os_error() == Some(libc::EAGAIN)
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::yield_now()
            }
            Err(error) => panic!("native lock remained held after descriptor close: {error}"),
        }
    }
}

fn setup() -> (tempfile::TempDir, Arc<View>) {
    fs::create_dir_all(".tmp").unwrap();
    let temp = tempfile::tempdir_in(".tmp").unwrap();
    for directory in ["project", "overlay"] {
        fs::create_dir(temp.path().join(directory)).unwrap();
    }
    fs::write(temp.path().join("project/native"), b"native bytes").unwrap();
    fs::write(temp.path().join("project/merged"), b"base").unwrap();
    fs::write(temp.path().join("overlay/merged"), b"overlay").unwrap();
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
fn descriptor_operations_retain_native_identity_and_reject_staged_files() {
    let (temp, view) = setup();
    let file = view
        .open_file(Path::new("native"), libc::O_RDWR, 0)
        .unwrap();
    file.allocate(0, 8192, 0).unwrap();
    assert_eq!(file.metadata().unwrap().size, 8192);
    assert!((12..=8192).contains(&file.seek(0, libc::SEEK_HOLE).unwrap()));
    assert_eq!(file.seek(3, libc::SEEK_SET).unwrap(), 3);
    assert_eq!(file.seek(2, libc::SEEK_CUR).unwrap(), 5);
    assert_eq!(file.seek(-1, libc::SEEK_END).unwrap(), 8191);
    assert!(file.seek(-1, libc::SEEK_SET).is_err());
    assert!(file.seek(0, 12345).is_err());
    assert!(file.allocate(u64::MAX, 1, 0).is_err());
    fs::remove_file(temp.path().join("project/native")).unwrap();
    file.allocate(0, 16384, 0).unwrap();
    assert_eq!(file.metadata().unwrap().size, 16384);
    let staged = view
        .open_file(Path::new("merged"), libc::O_RDWR, 0)
        .unwrap();
    assert_eq!(
        staged.allocate(0, 4096, 0).unwrap_err().raw_os_error(),
        Some(libc::ENOTSUP)
    );
    assert_eq!(
        staged.seek(0, libc::SEEK_DATA).unwrap_err().raw_os_error(),
        Some(libc::ENOTSUP)
    );
    assert!(file.ioctl(0x1234, 0, &[], 0).is_err());
    let (_, flags) = file
        .ioctl(
            libc::FS_IOC_GETFLAGS as u32,
            0,
            &[],
            std::mem::size_of::<libc::c_long>() as u32,
        )
        .unwrap();
    assert_eq!(flags.len(), std::mem::size_of::<libc::c_long>());
    assert!(file.ioctl(libc::FS_IOC_SETFLAGS as u32, 0, &[], 0).is_err());
    assert!(file.ioctl(libc::FS_IOC_GETFLAGS as u32, 0, &[], 1).is_err());
    file.release().unwrap();
    assert_eq!(
        file.seek(0, libc::SEEK_SET).unwrap_err().raw_os_error(),
        Some(libc::EBADF)
    );
}

#[test]
fn open_description_locks_conflict_without_cross_handle_release() {
    let (_temp, view) = setup();
    let first = view
        .open_file(Path::new("native"), libc::O_RDWR, 0)
        .unwrap();
    let second = view
        .open_file(Path::new("native"), libc::O_RDWR, 0)
        .unwrap();
    let lock = Lock {
        start: 2,
        end: 5,
        typ: libc::F_WRLCK,
        pid: 0,
    };
    first.set_lock(lock, false).unwrap();
    let conflict = second.get_lock(lock).unwrap();
    assert_eq!(
        (conflict.start, conflict.end, conflict.typ),
        (2, 5, libc::F_WRLCK)
    );
    assert!(second.set_lock(lock, false).is_err());
    let third = view
        .open_file(Path::new("native"), libc::O_RDWR, 0)
        .unwrap();
    third.release().unwrap();
    assert!(second.set_lock(lock, false).is_err());
    first
        .set_lock(
            Lock {
                typ: libc::F_UNLCK,
                ..lock
            },
            false,
        )
        .unwrap();
    assert_eq!(second.get_lock(lock).unwrap().typ, libc::F_UNLCK);
    second.set_lock(lock, false).unwrap();
    second.release().unwrap();
    // Other tests can fork between open and close. CLOEXEC releases their
    // inherited references at exec, rather than at this parent's close.
    acquire_after_last_close(&first, lock);
    first
        .set_lock(
            Lock {
                typ: libc::F_UNLCK,
                ..lock
            },
            false,
        )
        .unwrap();
    first.set_lock(lock, true).unwrap();
    assert!(
        first
            .get_lock(Lock {
                start: 9,
                end: 2,
                ..lock
            })
            .is_err()
    );
    assert!(first.set_lock(Lock { typ: 12345, ..lock }, true).is_err());
    first
        .set_lock(
            Lock {
                typ: libc::F_UNLCK,
                ..lock
            },
            true,
        )
        .unwrap();
    let whole = Lock {
        start: 0,
        end: i64::MAX as u64,
        ..lock
    };
    first.set_lock(whole, false).unwrap();
    let conflict = second.get_lock(whole).unwrap_err();
    assert_eq!(conflict.raw_os_error(), Some(libc::EBADF));
    let fourth = view
        .open_file(Path::new("native"), libc::O_RDWR, 0)
        .unwrap();
    let conflict = fourth.get_lock(whole).unwrap();
    assert_eq!(
        (conflict.start, conflict.end, conflict.typ),
        (0, i64::MAX as u64, libc::F_WRLCK)
    );
    first
        .set_lock(
            Lock {
                typ: libc::F_UNLCK,
                ..whole
            },
            false,
        )
        .unwrap();
}

#[test]
fn forked_descriptor_keeps_native_ofd_lock_until_exec() {
    use std::{
        io::{Read, Write},
        os::{
            fd::AsRawFd,
            unix::{net::UnixStream, process::CommandExt},
        },
        process::Command,
    };
    let (_temp, view) = setup();
    let first = view
        .open_file(Path::new("native"), libc::O_RDWR, 0)
        .unwrap();
    let second = view
        .open_file(Path::new("native"), libc::O_RDWR, 0)
        .unwrap();
    let lock = Lock {
        start: 0,
        end: i64::MAX as u64,
        typ: libc::F_WRLCK,
        pid: 0,
    };
    second.set_lock(lock, false).unwrap();
    let (mut ready, child_ready) = UnixStream::pair().unwrap();
    let (mut release, child_release) = UnixStream::pair().unwrap();
    ready
        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .unwrap();
    child_release
        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .unwrap();
    let mut command = Command::new("true");
    // SAFETY: after fork the closure uses only async-signal-safe read/write,
    // borrowing descriptors retained by the closure until exec completes.
    unsafe {
        command.pre_exec(move || {
            let mut byte = 0u8;
            if libc::write(child_ready.as_raw_fd(), (&byte as *const u8).cast(), 1) != 1 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::read(child_release.as_raw_fd(), (&mut byte as *mut u8).cast(), 1) != 1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let worker = std::thread::spawn(move || command.spawn().unwrap().wait().unwrap());
    ready.read_exact(&mut [0]).unwrap();
    second.release().unwrap();
    let conflict = first.set_lock(lock, false);
    release.write_all(b"x").unwrap();
    assert!(worker.join().unwrap().success());
    assert_eq!(conflict.unwrap_err().raw_os_error(), Some(libc::EAGAIN));
    acquire_after_last_close(&first, lock);
}
