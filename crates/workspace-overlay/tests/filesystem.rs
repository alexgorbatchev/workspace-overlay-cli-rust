use std::{
    fs,
    io::{Read, Seek, SeekFrom, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, PermissionsExt},
    },
    sync::Arc,
};
use workspace_overlay::{
    config::{Selection, Source},
    view::View,
};

fn setup() -> (tempfile::TempDir, Arc<View>) {
    fs::create_dir_all(".tmp").unwrap();
    let temp = tempfile::tempdir_in(".tmp").unwrap();
    for dir in ["project", "overlay"] {
        fs::create_dir(temp.path().join(dir)).unwrap();
    }
    fs::write(temp.path().join("project/plain.txt"), b"base-").unwrap();
    fs::write(temp.path().join("overlay/plain.txt"), b"overlay").unwrap();
    fs::write(temp.path().join("project/AGENTS.md"), b"base\n").unwrap();
    fs::write(temp.path().join("overlay/AGENTS.md"), b"overlay\n").unwrap();
    fs::create_dir(temp.path().join("overlay/only")).unwrap();
    fs::write(temp.path().join("overlay/only/protected"), b"protected").unwrap();
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
    (temp, view)
}

#[test]
fn native_metadata_and_directory_reads_surface_git_exclusion_refresh_failure() {
    use workspace_overlay::{Cancellation, gitexclude::Exclude};
    let (temp, view) = setup();
    let project = fs::canonicalize(temp.path().join("project")).unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(&project)
            .status()
            .unwrap()
            .success()
    );
    let exclude = Exclude::open(&project, &Cancellation::default())
        .unwrap()
        .unwrap();
    let exclude_path = exclude.path().to_owned();
    let backing = cap_std::fs::Dir::open_ambient_dir(
        exclude_path.parent().unwrap(),
        cap_std::ambient_authority(),
    )
    .unwrap();
    view.attach_exclude(exclude, |_| Ok(())).unwrap();
    let saved = fs::read(&exclude_path).unwrap();
    let mut mounted = view.mount().unwrap();
    backing.remove_file("exclude").unwrap();
    backing.create_dir("exclude").unwrap();
    fs::write(temp.path().join("overlay/new-excluded"), b"new").unwrap();
    assert!(view.refresh_paths().is_err());
    let metadata = fs::metadata(&project);
    let directory = fs::read_dir(&project).err();
    // Restore the backing file before asserting so failures cannot strand the mount.
    backing.remove_dir("exclude").unwrap();
    backing.write("exclude", saved).unwrap();
    mounted.unmount().unwrap();
    view.close_exclude().unwrap();
    assert_eq!(metadata.unwrap_err().raw_os_error(), Some(libc::EIO));
    assert_eq!(directory.unwrap().raw_os_error(), Some(libc::EIO));
}

#[test]
fn native_kernel_merge_writes_atomic_save_and_protection() {
    let (temp, view) = setup();
    let mut mounted = view.mount().unwrap();
    let root = temp.path().join("project");
    assert_eq!(fs::read(root.join("plain.txt")).unwrap(), b"base-overlay");
    fs::write(root.join("plain.txt"), b"base-new").unwrap();
    assert_eq!(
        fs::read(temp.path().join("overlay/plain.txt")).unwrap(),
        b"new"
    );
    let document = fs::read_to_string(root.join("AGENTS.md")).unwrap();
    let document = document
        .replace("base\n", "base changed\n")
        .replace("overlay\n", "overlay changed\n");
    fs::write(root.join("temporary"), document).unwrap();
    fs::rename(root.join("temporary"), root.join("AGENTS.md")).unwrap();
    assert_eq!(
        fs::read(temp.path().join("overlay/AGENTS.md")).unwrap(),
        b"overlay changed\n"
    );
    assert_eq!(
        fs::remove_file(root.join("only/protected"))
            .unwrap_err()
            .raw_os_error(),
        Some(libc::EPERM)
    );
    fs::write(root.join("only/new"), b"created").unwrap();
    fs::rename(root.join("only/new"), root.join("only/moved")).unwrap();
    fs::remove_file(root.join("only/moved")).unwrap();
    fs::create_dir(root.join("only/dir")).unwrap();
    std::os::unix::fs::symlink("../protected", root.join("only/dir/link")).unwrap();
    assert_eq!(
        fs::read_link(root.join("only/dir/link"))
            .unwrap()
            .as_os_str()
            .as_bytes(),
        b"../protected"
    );
    fs::remove_file(root.join("only/dir/link")).unwrap();
    fs::remove_dir(root.join("only/dir")).unwrap();
    mounted.unmount().unwrap();
    assert_eq!(fs::read(root.join("AGENTS.md")).unwrap(), b"base changed\n");
}

#[test]
fn native_directory_sync_and_filesystem_capacity_use_the_project_backing() {
    let (temp, view) = setup();
    let root = temp.path().join("project");
    let backing = fs::File::open(&root).unwrap();
    let expected = rustix::fs::fstatfs(&backing).unwrap();
    let mut mounted = view.mount().unwrap();
    let directory = fs::File::open(&root).unwrap();
    directory.sync_all().unwrap();
    directory.sync_data().unwrap();
    let observed = rustix::fs::fstatfs(&directory).unwrap();
    assert_eq!(observed.f_bsize, expected.f_bsize);
    assert_eq!(observed.f_blocks, expected.f_blocks);
    assert_eq!(observed.f_files, expected.f_files);
    assert_eq!(observed.f_namelen, expected.f_namelen);
    assert!(observed.f_bavail > 0);
    drop(directory);
    mounted.unmount().unwrap();
}

#[test]
fn native_kernel_descriptor_identity_directory_offsets_and_metadata() {
    let (temp, view) = setup();
    let mut mounted = view.mount().unwrap();
    let root = temp.path().join("project");
    let mut file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(root.join("native"))
        .unwrap();
    file.write_all(b"initial").unwrap();
    file.sync_all().unwrap();
    fs::hard_link(root.join("native"), root.join("alias")).unwrap();
    assert_eq!(
        fs::metadata(root.join("native")).unwrap().ino(),
        fs::metadata(root.join("alias")).unwrap().ino()
    );
    fs::remove_file(root.join("native")).unwrap();
    file.set_len(3).unwrap();
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .unwrap();
    file.seek(SeekFrom::Start(0)).unwrap();
    let mut data = Vec::new();
    file.read_to_end(&mut data).unwrap();
    assert_eq!(data, b"ini");
    assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(file.metadata().unwrap().nlink(), 1);
    drop(file);
    for n in 0..300 {
        fs::write(root.join(format!("entry-{n:03}")), b"x").unwrap();
    }
    let names = fs::read_dir(&root)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<std::collections::BTreeSet<_>>();
    assert!(names.contains(std::ffi::OsStr::new("entry-299")));
    assert_eq!(names.len(), 304);
    mounted.unmount().unwrap();
}

#[test]
fn native_kernel_allocation_holes_ioctl_and_flock_share_backing_description() {
    use std::os::fd::AsRawFd;
    let (temp, view) = setup();
    let root = temp.path().join("project");
    fs::write(root.join("native-lock"), b"native").unwrap();
    let backing = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(root.join("native-lock"))
        .unwrap();
    let mut mounted = view.mount().unwrap();
    let first = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(root.join("native-lock"))
        .unwrap();
    let second = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(root.join("native-lock"))
        .unwrap();
    assert_eq!(
        unsafe { libc::fallocate(first.as_raw_fd(), 0, 0, 16384) },
        0
    );
    assert_eq!(backing.metadata().unwrap().len(), 16384);
    assert!(unsafe { libc::lseek(first.as_raw_fd(), 0, libc::SEEK_HOLE) } > 0);
    let mut flags: libc::c_long = 0;
    assert_eq!(
        unsafe { libc::ioctl(first.as_raw_fd(), libc::FS_IOC_GETFLAGS, &mut flags) },
        0,
        "{}",
        std::io::Error::last_os_error()
    );
    assert_eq!(
        unsafe { libc::flock(first.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    assert_eq!(
        unsafe { libc::flock(backing.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        -1
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::EAGAIN)
    );
    let (finished, wait) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let result = unsafe { libc::flock(second.as_raw_fd(), libc::LOCK_EX) };
        finished.send(result).unwrap();
        assert_eq!(unsafe { libc::flock(second.as_raw_fd(), libc::LOCK_UN) }, 0);
    });
    assert!(matches!(
        wait.recv_timeout(std::time::Duration::from_millis(30)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
    ));
    assert_eq!(unsafe { libc::flock(first.as_raw_fd(), libc::LOCK_UN) }, 0);
    assert_eq!(
        wait.recv_timeout(std::time::Duration::from_secs(2))
            .unwrap(),
        0
    );
    worker.join().unwrap();
    drop(first);
    drop(backing);
    mounted.unmount().unwrap();
}
