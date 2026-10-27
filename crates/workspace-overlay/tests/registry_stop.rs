use notify::Watcher;
use std::{fs, sync::mpsc, time::Duration};
use workspace_overlay::{
    Cancellation, Error,
    registry::{MountRecord, Store},
};

#[test]
fn stop_requests_distinguish_missing_abandoned_and_live_owners() {
    fs::create_dir_all(".tmp").unwrap();
    let temporary = tempfile::tempdir_in(".tmp").unwrap();
    let root = fs::canonicalize(temporary.path()).unwrap();
    let store = Store::new(root.join("state")).unwrap();
    let token = Cancellation::default();
    assert!(store.request_stop(&root, "project", &token).unwrap());
    let mut owner = store.open(&root, "project").unwrap();
    let target = root.join("project");
    owner
        .record(MountRecord {
            target: target.clone(),
            exclude: None,
            block: vec![],
        })
        .unwrap();
    owner.unlock().unwrap();
    assert!(!store.request_stop(&root, "project", &token).unwrap());
    assert!(!owner.stop_file().exists());
    let mut owner = store.acquire(&root, "project").unwrap();
    owner
        .adopt(store.read(&root, "project").unwrap().unwrap())
        .unwrap();
    let stop = owner.stop_file().to_owned();
    let (send, receive) = mpsc::channel();
    let mut watch = notify::recommended_watcher(move |event| {
        let _ = send.send(event);
    })
    .unwrap();
    watch
        .watch(store.directory(), notify::RecursiveMode::NonRecursive)
        .unwrap();
    let worker = std::thread::spawn(move || {
        while !stop.exists() {
            receive
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap();
        }
        owner.forget(&target).unwrap();
        owner.close().unwrap();
    });
    assert!(store.request_stop(&root, "project", &token).unwrap());
    worker.join().unwrap();
    assert!(store.read(&root, "project").unwrap().is_none());
}

#[test]
fn stop_request_cancellation_and_invalid_destination_preserve_state() {
    fs::create_dir_all(".tmp").unwrap();
    let temporary = tempfile::tempdir_in(".tmp").unwrap();
    let root = fs::canonicalize(temporary.path()).unwrap();
    let store = Store::new(root.join("state")).unwrap();
    let mut owner = store.open(&root, "project").unwrap();
    let target = root.join("project");
    owner
        .record(MountRecord {
            target: target.clone(),
            exclude: None,
            block: vec![],
        })
        .unwrap();
    let cancelled = Cancellation::default();
    cancelled.cancel();
    assert!(matches!(
        store.request_stop(&root, "project", &cancelled),
        Err(Error::Cancelled)
    ));
    assert!(!owner.stop_file().exists());
    fs::create_dir(owner.stop_file()).unwrap();
    assert!(
        store
            .request_stop(&root, "project", &Cancellation::default())
            .is_err()
    );
    fs::remove_dir(owner.stop_file()).unwrap();
    let (send, receive) = mpsc::channel();
    let mut watch = notify::recommended_watcher(move |event| {
        let _ = send.send(event);
    })
    .unwrap();
    watch
        .watch(store.directory(), notify::RecursiveMode::NonRecursive)
        .unwrap();
    let token = Cancellation::default();
    let active = token.clone();
    let store2 = store.clone();
    let root2 = root.clone();
    let worker = std::thread::spawn(move || store2.request_stop(&root2, "project", &active));
    while !owner.stop_file().exists() {
        receive
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
    }
    token.cancel();
    assert!(matches!(worker.join().unwrap(), Err(Error::Cancelled)));
    assert!(store.read(&root, "project").unwrap().is_some());
    owner.forget(&target).unwrap();
    owner.close().unwrap();
}
