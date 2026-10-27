use std::{
    fs,
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::PermissionsExt,
    },
    path::Path,
};
use workspace_overlay::registry::{MountRecord, Store};

#[test]
fn ownership_recovery_atomic_records_and_byte_paths() {
    fs::create_dir_all(".tmp").unwrap();
    let temp = tempfile::tempdir_in(".tmp").unwrap();
    let store = Store::new(temp.path().join("state")).unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let mut owner = store.open(&root, "project").unwrap();
    assert!(store.acquire(&root, "project").is_err());
    let target = root.join(std::ffi::OsString::from_vec(b"target-\xff".to_vec()));
    owner
        .record(MountRecord {
            target: target.clone(),
            exclude: None,
            block: vec![],
        })
        .unwrap();
    owner
        .record(MountRecord {
            target: root.join("alpha"),
            exclude: Some(root.join("exclude")),
            block: b"marker-\xff".to_vec(),
        })
        .unwrap();
    let state = store.read(&root, "project").unwrap().unwrap();
    assert_eq!(state.mounts.len(), 2);
    assert_eq!(
        state.mounts[1].target.as_os_str().as_bytes(),
        target.as_os_str().as_bytes()
    );
    assert_eq!(
        fs::metadata(owner.file()).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(store.directory())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert!(owner.close().is_err());
    assert!(store.open(&root, "project").is_err());
    let mut recovery = store.acquire(&root, "project").unwrap();
    recovery.adopt(state).unwrap();
    recovery.forget(&target).unwrap();
    assert_eq!(
        store.read(&root, "project").unwrap().unwrap().mounts.len(),
        1
    );
    recovery.forget(&root.join("alpha")).unwrap();
    recovery.close().unwrap();
    assert!(store.read(&root, "project").unwrap().is_none());
    assert_eq!(fs::read_dir(store.directory()).unwrap().count(), 0);
    let mut again = store.open(&root, "project").unwrap();
    again.close().unwrap();
    assert!(store.open(Path::new("relative"), "project").is_err());
}

#[test]
fn malformed_and_mismatched_records_are_rejected_without_overwrite() {
    fs::create_dir_all(".tmp").unwrap();
    let temp = tempfile::tempdir_in(".tmp").unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let store = Store::new(root.join("state")).unwrap();
    let mut owner = store.open(&root, "project").unwrap();
    let file = owner.file().to_owned();
    owner
        .record(MountRecord {
            target: root.join("target"),
            exclude: None,
            block: vec![],
        })
        .unwrap();
    owner.unlock().unwrap();
    let valid = fs::read(&file).unwrap();
    for bad in [
        b"garbage".as_slice(),
        br#"{"version":2,"root":"/","project":"project","mounts":[]}"#,
    ] {
        fs::write(&file, bad).unwrap();
        assert!(store.read(&root, "project").is_err());
        assert!(store.open(&root, "project").is_err());
        assert_eq!(fs::read(&file).unwrap(), bad);
    }
    fs::write(&file, valid).unwrap();
    let mut recovery = store.acquire(&root, "project").unwrap();
    recovery.discard().unwrap();
    recovery.unlock().unwrap();
}

#[test]
fn cleanup_attempts_independent_paths_and_retains_every_failure() {
    fs::create_dir_all(".tmp").unwrap();
    let temp = tempfile::tempdir_in(".tmp").unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let store = Store::new(root.join("state")).unwrap();
    let mut owner = store.open(&root, "project").unwrap();
    // Native unlink rejects a directory. Later independent paths still need
    // cleanup, and releasing the ownership lock must happen even on failure.
    fs::create_dir(owner.file()).unwrap();
    fs::write(owner.stop_file(), b"stop").unwrap();
    fs::create_dir(owner.file().with_extension("json.next")).unwrap();
    let error = owner.close().unwrap_err();
    assert!(!owner.stop_file().exists());
    let workspace_overlay::Error::Failures(errors) = error else {
        panic!("both native unlink failures must survive: {error}");
    };
    assert_eq!(errors.0.len(), 2);
    assert!(errors.0.iter().all(|error| matches!(error, workspace_overlay::Error::Io(io) if io.raw_os_error() == Some(libc::EISDIR))));
    // Consumers can report all cleanup failures and inspect the primary
    // cause through the standard error chain without parsing its message.
    let diagnostic = errors.to_string();
    assert_eq!(diagnostic.matches(&errors.0[0].to_string()).count(), 2);
    let source = std::error::Error::source(&errors)
        .unwrap()
        .downcast_ref::<workspace_overlay::Error>()
        .unwrap();
    assert!(
        matches!(source, workspace_overlay::Error::Io(io) if io.raw_os_error() == Some(libc::EISDIR))
    );
    let mut next = store.acquire(&root, "project").unwrap();
    next.unlock().unwrap();
}
