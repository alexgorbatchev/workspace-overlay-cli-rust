use std::{ffi::OsString, fs, os::unix::ffi::OsStringExt, sync::Arc};
use workspace_overlay::{
    Cancellation,
    config::{Selection, Source},
    mounts,
    view::View,
};

#[test]
fn native_mount_inspection_preserves_raw_paths_and_never_unmounts_foreign_filesystems() {
    fs::create_dir_all(".tmp").unwrap();
    let temp = tempfile::tempdir_in(".tmp").unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let target = root.join(OsString::from_vec(b"project space\nbyte-\xff".to_vec()));
    let overlay = root.join("overlay");
    fs::create_dir(&target).unwrap();
    fs::create_dir(&overlay).unwrap();
    fs::write(target.join("file"), b"base").unwrap();
    let cancel = Cancellation::default();
    assert_eq!(mounts::mounted_type(&target, &cancel).unwrap(), None);
    assert!(mounts::holding_type(&target, &cancel).unwrap().is_some());
    assert!(mounts::unmount(std::path::Path::new("/"), None, &cancel).is_err());
    let view = Arc::new(
        View::open(&Selection {
            root,
            project: "project".into(),
            target: target.clone(),
            markers: vec![],
            sources: vec![Source {
                name: "overlay".into(),
                path: overlay,
                glob: "**/*".into(),
            }],
        })
        .unwrap(),
    );
    let mut mounted = view.mount().unwrap();
    assert_eq!(
        mounts::mounted_type(&target, &cancel).unwrap().as_deref(),
        Some(std::ffi::OsStr::new(mounts::FILESYSTEM_TYPE))
    );
    assert_eq!(
        mounts::holding_type(&target.join("file"), &cancel)
            .unwrap()
            .as_deref(),
        Some(std::ffi::OsStr::new(mounts::FILESYSTEM_TYPE))
    );
    assert!(mounts::overlay_targets(&cancel).unwrap().contains(&target));
    mounts::unmount(&target, None, &cancel).unwrap();
    mounted.wait().unwrap();
    mounts::unmount(&target, None, &cancel).unwrap();
    assert_eq!(mounts::mounted_type(&target, &cancel).unwrap(), None);
}

#[test]
fn busy_mount_retains_typed_holders_until_explicit_confirmation() {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    fs::create_dir_all(".tmp").unwrap();
    let temp = tempfile::tempdir_in(".tmp").unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let target = root.join("project");
    let overlay = root.join("overlay");
    fs::create_dir(&target).unwrap();
    fs::create_dir(&overlay).unwrap();
    let view = Arc::new(
        View::open(&Selection {
            root,
            project: "project".into(),
            target: target.clone(),
            markers: vec![],
            sources: vec![Source {
                name: "overlay".into(),
                path: overlay,
                glob: "**/*".into(),
            }],
        })
        .unwrap(),
    );
    let mut mounted = view.mount().unwrap();
    struct ChildGuard(std::process::Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut child = ChildGuard(
        Command::new("sh")
            .args(["-c", "printf 'ready\\n'; read value"])
            .current_dir(&target)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut ready = String::new();
    BufReader::new(child.0.stdout.take().unwrap())
        .read_line(&mut ready)
        .unwrap();
    assert_eq!(ready, "ready\n");
    let cancel = Cancellation::default();
    let busy = match mounts::unmount(&target, None, &cancel).unwrap_err() {
        workspace_overlay::Error::Busy(busy) => busy,
        error => panic!("{error}"),
    };
    assert_eq!(busy.target, target);
    assert!(
        busy.holders
            .iter()
            .any(|process| process.pid == child.0.id()
                && process.directory.as_deref() == Some(target.as_path()))
    );
    assert!(busy.to_string().contains(&format!("pid {}", child.0.id())));
    assert_eq!(
        mounts::mounted_type(&target, &cancel).unwrap().as_deref(),
        Some(std::ffi::OsStr::new(mounts::FILESYSTEM_TYPE))
    );
    let confirm = |path: &std::path::Path, holders: &[workspace_overlay::holders::Process]| {
        assert_eq!(path, target);
        assert!(holders.iter().any(|process| process.pid == child.0.id()));
        true
    };
    mounts::unmount(&target, Some(&confirm), &cancel).unwrap();
    child.0.wait().unwrap();
    mounted.wait().unwrap();
}
