use std::{fs, sync::Arc};
use workspace_overlay::{
    Cancellation,
    config::{Selection, Source},
    gitexclude::Exclude,
    management, mounts,
    registry::{MountRecord, Store},
    view::View,
};

#[test]
fn abandoned_registry_recovers_mounts_before_owned_exclusions_and_retains_user_bytes() {
    fs::create_dir_all(".tmp").unwrap();
    let temp = tempfile::tempdir_in(".tmp").unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let target = root.join("project");
    let overlay = root.join("overlay");
    fs::create_dir(&target).unwrap();
    fs::create_dir(&overlay).unwrap();
    fs::write(overlay.join("only"), b"overlay").unwrap();
    let exclude_path = root.join("exclude");
    fs::write(&exclude_path, b"user pattern\n").unwrap();
    let selection = Selection {
        root: root.clone(),
        project: "project".into(),
        target: target.clone(),
        markers: vec![],
        sources: vec![Source {
            name: "overlay".into(),
            path: overlay,
            glob: "**/*".into(),
        }],
    };
    let store = Store::new(root.join("state")).unwrap();
    let mut owner = store.open(&root, "project").unwrap();
    let mut exclude = Exclude::at(&exclude_path).unwrap();
    exclude.update(&[b"/only".to_vec()]).unwrap();
    owner
        .record(MountRecord {
            target: target.clone(),
            exclude: Some(exclude_path.clone()),
            block: exclude.block().to_vec(),
        })
        .unwrap();
    owner.unlock().unwrap();
    let view = Arc::new(View::open(&selection).unwrap());
    let mut mounted = view.mount().unwrap();
    let status = management::status(&store, &selection, false, &Cancellation::default()).unwrap();
    assert_eq!(status.len(), 1);
    assert_eq!(status[0].target, target);
    assert_eq!(
        status[0].filesystem.as_deref(),
        Some(std::ffi::OsStr::new(mounts::FILESYSTEM_TYPE))
    );
    let cancelled = Cancellation::default();
    cancelled.cancel();
    assert!(matches!(
        management::status(&store, &selection, false, &cancelled),
        Err(workspace_overlay::Error::Cancelled)
    ));
    management::unmount(&store, &selection, false, None, &Cancellation::default()).unwrap();
    mounted.wait().unwrap();
    assert_eq!(fs::read(&exclude_path).unwrap(), b"user pattern\n");
    assert!(store.read(&root, "project").unwrap().is_none());
    let status = management::status(&store, &selection, false, &Cancellation::default()).unwrap();
    assert_eq!(status.len(), 1);
    assert!(status[0].filesystem.is_none());
    management::unmount(&store, &selection, false, None, &Cancellation::default()).unwrap();
}

#[test]
fn recovery_retains_record_and_exclusion_on_foreign_mount_or_changed_block() {
    fs::create_dir_all(".tmp").unwrap();
    let temp = tempfile::tempdir_in(".tmp").unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let store = Store::new(root.join("state")).unwrap();
    let exclude = root.join("exclude");
    fs::write(&exclude, b"user\nmanaged\n").unwrap();
    let mut owner = store.open(&root, "project").unwrap();
    owner
        .record(MountRecord {
            target: "/".into(),
            exclude: Some(exclude.clone()),
            block: b"managed\n".to_vec(),
        })
        .unwrap();
    owner.unlock().unwrap();
    assert!(management::recover(&store, &root, "project", None, &Cancellation::default()).is_err());
    assert_eq!(fs::read(&exclude).unwrap(), b"user\nmanaged\n");
    assert!(store.read(&root, "project").unwrap().is_some());
    let target = root.join("unmounted");
    fs::create_dir(&target).unwrap();
    let mut owner = store.acquire(&root, "project").unwrap();
    owner
        .record(MountRecord {
            target: target.clone(),
            exclude: Some(exclude.clone()),
            block: b"missing managed block\n".to_vec(),
        })
        .unwrap();
    owner.forget(std::path::Path::new("/")).unwrap();
    owner.unlock().unwrap();
    assert!(management::recover(&store, &root, "project", None, &Cancellation::default()).is_err());
    assert!(store.read(&root, "project").unwrap().is_some());
    assert!(
        management::status(
            &store,
            &Selection {
                root: root.clone(),
                project: "project".into(),
                target,
                markers: vec![],
                sources: vec![]
            },
            false,
            &Cancellation::default()
        )
        .is_ok()
    );
}
