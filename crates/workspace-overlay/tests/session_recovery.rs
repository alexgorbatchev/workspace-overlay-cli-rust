use std::{
    fs::{self, File},
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    os::{fd::OwnedFd, unix::net::UnixStream},
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread::JoinHandle,
    time::Duration,
};
use workspace_overlay::{
    Cancellation, Error,
    config::{Selection, Source},
    management, mounts,
    registry::Store,
    session::{self, Event, Options},
};

const WAIT: Duration = Duration::from_secs(5);

struct SessionGuard {
    cancel: Cancellation,
    child: Option<Child>,
    worker: Option<JoinHandle<()>>,
    finished: mpsc::Receiver<workspace_overlay::Result<()>>,
    store: Store,
    selection: Selection,
}
impl SessionGuard {
    fn release_child(&mut self) {
        if let Some(mut child) = self.child.take() {
            child.kill().unwrap();
            child.wait().unwrap();
        }
    }
}
impl Drop for SessionGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.cancel.cancel();
        if self.worker.is_some() && self.finished.recv_timeout(WAIT).is_ok() {
            let _ = self.worker.take().unwrap().join();
        }
        let _ = management::unmount(
            &self.store,
            &self.selection,
            false,
            None,
            &Cancellation::default(),
        );
    }
}

fn contents(file: &mut File) -> Vec<u8> {
    file.seek(SeekFrom::Start(0)).unwrap();
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).unwrap();
    bytes
}

fn busy(error: &Error) -> Option<&mounts::BusyError> {
    match error {
        Error::Busy(error) => Some(error),
        Error::Context { source, .. } => busy(source),
        Error::Failures(errors) => errors.0.iter().find_map(busy),
        _ => None,
    }
}

#[test]
fn busy_shutdown_preserves_recovery_records_and_owned_exclusions_until_released() {
    fs::create_dir_all(".tmp").unwrap();
    let temp = tempfile::tempdir_in(".tmp").unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let target = root.join("project");
    let overlay = root.join("overlay");
    fs::create_dir(&target).unwrap();
    fs::create_dir(&overlay).unwrap();
    let git = Command::new("git")
        .args(["init", "--initial-branch=main"])
        .arg(&target)
        .output()
        .unwrap();
    assert!(git.status.success(), "{git:?}");
    fs::write(overlay.join("only.txt"), b"overlay\n").unwrap();
    let exclude_path = target.join(".git/info/exclude");
    let original = b"# user's rules\n*.local\n";
    fs::write(&exclude_path, original).unwrap();
    // Retain the underlying inode before the target becomes a FUSE mount.
    let mut exclude = File::open(&exclude_path).unwrap();
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
    let cancel = Cancellation::default();
    let records = store.clone();
    let stopping = cancel.clone();
    let selected = selection.clone();
    let (send, events) = mpsc::channel();
    let (done, finished) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let result = session::serve(
            vec![selected],
            Options {
                worktrees: false,
                ..Options::default()
            },
            records,
            &stopping,
            None,
            &mut |event| {
                send.send(event).unwrap();
                Ok(())
            },
        );
        let _ = done.send(result);
    });
    let mut guard = SessionGuard {
        cancel,
        child: None,
        worker: Some(worker),
        finished,
        store,
        selection,
    };
    assert!(
        matches!(events.recv_timeout(WAIT).unwrap(), Event::Mounted { target: mounted, .. } if mounted == target)
    );
    let (reader, writer) = UnixStream::pair().unwrap();
    reader.set_read_timeout(Some(WAIT)).unwrap();
    guard.child = Some(
        Command::new("sh")
            .args(["-c", "printf 'ready\\n'; read value"])
            .current_dir(&target)
            .stdin(Stdio::piped())
            .stdout(Stdio::from(OwnedFd::from(writer)))
            .spawn()
            .unwrap(),
    );
    let mut ready = String::new();
    BufReader::new(reader).read_line(&mut ready).unwrap();
    assert_eq!(ready, "ready\n");
    let pid = guard.child.as_ref().unwrap().id();
    let state = guard.store.read(&root, "project").unwrap().unwrap();
    assert_eq!(state.mounts.len(), 1);
    let record = &state.mounts[0];
    assert_eq!(record.target, target);
    assert_eq!(record.exclude.as_deref(), Some(exclude_path.as_path()));
    assert!(!record.block.is_empty());
    let owned = contents(&mut exclude);
    assert!(owned.starts_with(original));
    assert!(
        owned
            .windows(record.block.len())
            .any(|bytes| bytes == record.block)
    );

    guard.cancel.cancel();
    let error = guard.finished.recv_timeout(WAIT).unwrap().unwrap_err();
    guard.worker.take().unwrap().join().unwrap();
    let blocked = busy(&error).unwrap_or_else(|| panic!("missing typed busy failure: {error}"));
    assert_eq!(blocked.target, target);
    assert!(blocked.holders.iter().any(|holder| holder.pid == pid));
    assert!(guard.child.as_mut().unwrap().try_wait().unwrap().is_none());
    assert_eq!(
        mounts::mounted_type(&target, &Cancellation::default())
            .unwrap()
            .as_deref(),
        Some(std::ffi::OsStr::new(mounts::FILESYSTEM_TYPE))
    );
    let retained = guard.store.read(&root, "project").unwrap().unwrap();
    assert_eq!(retained.mounts.len(), 1);
    assert_eq!(retained.mounts[0].target, record.target);
    assert_eq!(retained.mounts[0].exclude, record.exclude);
    assert_eq!(retained.mounts[0].block, record.block);
    assert_eq!(contents(&mut exclude), owned);

    guard.release_child();
    management::unmount(
        &guard.store,
        &guard.selection,
        false,
        None,
        &Cancellation::default(),
    )
    .unwrap();
    assert!(
        mounts::mounted_type(&target, &Cancellation::default())
            .unwrap()
            .is_none()
    );
    assert!(guard.store.read(&root, "project").unwrap().is_none());
    assert_eq!(contents(&mut exclude), original);
}
