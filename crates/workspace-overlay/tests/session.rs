use std::{fs, path::Path, process::Command, sync::mpsc, time::Duration};
use workspace_overlay::{
    Cancellation,
    config::{Selection, Source},
    registry::Store,
    session::{self, Event, Options},
};

fn read(path: &Path) -> Vec<u8> {
    let output = Command::new("cat").arg(path).output().unwrap();
    assert!(output.status.success(), "{:?}", output);
    output.stdout
}
fn git(project: &Path, args: &[&std::ffi::OsStr]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(project)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output);
}
fn args(values: &[&'static str]) -> Vec<&'static std::ffi::OsStr> {
    values
        .iter()
        .map(|value| std::ffi::OsStr::new(*value))
        .collect()
}
fn await_target(events: &mpsc::Receiver<Event>, target: &Path, mounted: bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let event = events
            .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
            .unwrap();
        match event {
            Event::Mounted { target: actual, .. } if mounted && actual == target => return,
            Event::Unmounted { target: actual } if !mounted && actual == target => return,
            Event::Warning(error) => panic!("unexpected warning: {error}"),
            _ => {}
        }
    }
}

#[test]
fn serves_live_sources_and_cleans_owned_registry_and_exclusions() {
    fs::create_dir_all(".tmp").unwrap();
    let temp = tempfile::tempdir_in(".tmp").unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let project = root.join("project");
    let overlay = root.join("overlay");
    fs::create_dir(&project).unwrap();
    fs::create_dir(&overlay).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "--initial-branch=main"])
            .arg(&project)
            .output()
            .unwrap()
            .status
            .success()
    );
    fs::write(project.join("shared.txt"), b"base\n").unwrap();
    fs::write(overlay.join("shared.txt"), b"overlay\n").unwrap();
    let selection = Selection {
        root: root.clone(),
        project: "project".into(),
        target: project.clone(),
        markers: vec![],
        sources: vec![Source {
            name: "overlay".into(),
            path: overlay.clone(),
            glob: "**/*".into(),
        }],
    };
    let store = Store::new(root.join("state")).unwrap();
    let cancel = Cancellation::default();
    let stopping = cancel.clone();
    let records = store.clone();
    let (send, events) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        session::serve(
            vec![selection],
            Options::default(),
            records,
            &stopping,
            None,
            &mut |event| {
                send.send(event).unwrap();
                Ok(())
            },
        )
    });
    assert!(matches!(
        events.recv_timeout(Duration::from_secs(5)).unwrap(),
        Event::Mounted { .. }
    ));
    assert_eq!(read(&project.join("shared.txt")), b"base\noverlay\n");
    fs::write(overlay.join("new.txt"), b"new\n").unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let output = Command::new("cat")
            .arg(project.join("new.txt"))
            .output()
            .unwrap();
        if output.status.success() {
            assert_eq!(output.stdout, b"new\n");
            break;
        }
        assert!(std::time::Instant::now() < deadline);
        std::thread::yield_now();
    }
    assert!(store.read(&root, "project").unwrap().unwrap().mounts.len() == 1);
    cancel.cancel();
    worker.join().unwrap().unwrap();
    assert!(store.read(&root, "project").unwrap().is_none());
    assert_eq!(fs::read(project.join("shared.txt")).unwrap(), b"base\n");
    assert_eq!(
        fs::read(project.join(".git/info/exclude"))
            .unwrap()
            .windows(b"# begin workspace-overlay".len())
            .filter(|bytes| *bytes == b"# begin workspace-overlay")
            .count(),
        0
    );
}

#[test]
fn rejects_overlapping_primaries_before_mount_and_callback_failure_cleans_up() {
    fs::create_dir_all(".tmp").unwrap();
    let temp = tempfile::tempdir_in(".tmp").unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let project = root.join("project");
    fs::create_dir_all(project.join("child")).unwrap();
    let selection = Selection {
        root: root.clone(),
        project: "project".into(),
        target: project.clone(),
        markers: vec![],
        sources: vec![],
    };
    let mut nested = selection.clone();
    nested.project = "nested".into();
    nested.target = project.join("child");
    let store = Store::new(root.join("state")).unwrap();
    assert!(
        session::serve(
            vec![selection.clone(), nested],
            Options::default(),
            store.clone(),
            &Cancellation::default(),
            None,
            &mut |_| Ok(())
        )
        .unwrap_err()
        .to_string()
        .contains("overlap")
    );
    assert!(store.read(&root, "project").unwrap().is_none());
    let error = session::serve(
        vec![selection],
        Options::default(),
        store.clone(),
        &Cancellation::default(),
        None,
        &mut |_| Err(workspace_overlay::Error::Invalid("consumer failed".into())),
    )
    .unwrap_err();
    assert!(error.to_string().contains("consumer failed"));
    assert!(store.read(&root, "project").unwrap().is_none());
}

#[test]
fn worktree_registration_is_live_and_hand_unmounted_registration_stays_suppressed() {
    fs::create_dir_all(".tmp").unwrap();
    let temp = tempfile::tempdir_in(".tmp").unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let project = root.join("project");
    let overlay = root.join("overlay");
    fs::create_dir(&project).unwrap();
    fs::create_dir(&overlay).unwrap();
    git(&project, &args(&["init", "--initial-branch=main"]));
    fs::write(project.join("tracked"), b"base").unwrap();
    git(&project, &args(&["add", "tracked"]));
    git(&project, &args(&["commit", "-m", "fixture"]));
    fs::write(overlay.join("only.txt"), b"overlay").unwrap();
    let selection = Selection {
        root: root.clone(),
        project: "project".into(),
        target: project.clone(),
        markers: vec![],
        sources: vec![Source {
            name: "overlay".into(),
            path: overlay.clone(),
            glob: "**/*".into(),
        }],
    };
    let store = Store::new(root.join("state")).unwrap();
    let cancel = Cancellation::default();
    let stopping = cancel.clone();
    let records = store.clone();
    let (send, events) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        session::serve(
            vec![selection],
            Options::default(),
            records,
            &stopping,
            None,
            &mut |event| {
                send.send(event).unwrap();
                Ok(())
            },
        )
    });
    await_target(&events, &project, true);
    let worktree = root.join("linked");
    let mut add = args(&["worktree", "add", "-b", "linked"]);
    add.push(worktree.as_os_str());
    add.push(std::ffi::OsStr::new("main"));
    git(&project, &add);
    await_target(&events, &worktree, true);
    assert_eq!(read(&worktree.join("only.txt")), b"overlay");
    assert!(
        Command::new("fusermount3")
            .args(["-u", "--"])
            .arg(&worktree)
            .output()
            .unwrap()
            .status
            .success()
    );
    await_target(&events, &worktree, false);
    fs::write(overlay.join("later.txt"), b"later").unwrap();
    let barrier = root.join("barrier");
    let mut add = args(&["worktree", "add", "-b", "barrier"]);
    add.push(barrier.as_os_str());
    add.push(std::ffi::OsStr::new("main"));
    git(&project, &add);
    await_target(&events, &barrier, true);
    assert!(
        workspace_overlay::mounts::mounted_type(&worktree, &Cancellation::default())
            .unwrap()
            .is_none()
    );
    let mut remove = args(&["worktree", "remove", "--force"]);
    remove.push(worktree.as_os_str());
    git(&project, &remove);
    let mut add = args(&["worktree", "add", "-b", "replacement"]);
    add.push(worktree.as_os_str());
    add.push(std::ffi::OsStr::new("main"));
    git(&project, &add);
    await_target(&events, &worktree, true);
    assert_eq!(read(&worktree.join("later.txt")), b"later");
    cancel.cancel();
    worker.join().unwrap().unwrap();
    assert!(store.read(&root, "project").unwrap().is_none());
}

#[test]
fn live_owner_stop_request_cancels_its_project_and_restores_exclusions() {
    fs::create_dir_all(".tmp").unwrap();
    let temp = tempfile::tempdir_in(".tmp").unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let project = root.join("project");
    fs::create_dir(&project).unwrap();
    let selection = Selection {
        root: root.clone(),
        project: "project".into(),
        target: project.clone(),
        markers: vec![],
        sources: vec![],
    };
    let store = Store::new(root.join("state")).unwrap();
    let records = store.clone();
    let (send, events) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        session::serve(
            vec![selection],
            Options::default(),
            records,
            &Cancellation::default(),
            None,
            &mut |event| {
                send.send(event).unwrap();
                Ok(())
            },
        )
    });
    await_target(&events, &project, true);
    assert!(
        store
            .request_stop(&root, "project", &Cancellation::default())
            .unwrap()
    );
    worker.join().unwrap().unwrap();
    assert!(store.read(&root, "project").unwrap().is_none());
}

#[test]
fn preparation_failures_and_existing_mounts_leave_no_new_mounts_or_locks() {
    fs::create_dir_all(".tmp").unwrap();
    let temp = tempfile::tempdir_in(".tmp").unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let target = root.join("project");
    fs::create_dir(&target).unwrap();
    let selection = Selection {
        root: root.clone(),
        project: "project".into(),
        target: target.clone(),
        markers: vec![],
        sources: vec![],
    };
    let store = Store::new(root.join("state")).unwrap();
    let cancel = Cancellation::default();
    session::serve(
        vec![],
        Options::default(),
        store.clone(),
        &cancel,
        None,
        &mut |_| Ok(()),
    )
    .unwrap();
    let mut contains_source = selection.clone();
    contains_source.sources.push(Source {
        name: "nested".into(),
        path: target.clone(),
        glob: "**/*".into(),
    });
    assert!(
        session::serve(
            vec![contains_source],
            Options::default(),
            store.clone(),
            &cancel,
            None,
            &mut |_| Ok(())
        )
        .unwrap_err()
        .to_string()
        .contains("contains overlay source")
    );
    let mut invalid = selection.clone();
    invalid.markers.push(workspace_overlay::config::MarkerRule {
        glob: "[".into(),
        start: "begin".into(),
        end: "end".into(),
    });
    assert!(
        session::serve(
            vec![invalid],
            Options::default(),
            store.clone(),
            &cancel,
            None,
            &mut |_| Ok(())
        )
        .is_err()
    );
    assert!(store.read(&root, "project").unwrap().is_none());
    store.open(&root, "project").unwrap().close().unwrap();
    let view = std::sync::Arc::new(workspace_overlay::view::View::open(&selection).unwrap());
    let mut mounted = view.mount().unwrap();
    assert!(
        session::serve(
            vec![selection.clone()],
            Options::default(),
            store.clone(),
            &cancel,
            None,
            &mut |_| Ok(())
        )
        .unwrap_err()
        .to_string()
        .contains("already mounted")
    );
    let result = session::serve(
        vec![selection],
        Options {
            replace: true,
            worktrees: false,
        },
        store.clone(),
        &cancel,
        None,
        &mut |event| {
            if matches!(event, Event::Mounted { .. }) {
                cancel.cancel();
            }
            Ok(())
        },
    );
    result.unwrap();
    mounted.wait().unwrap();
    assert!(store.read(&root, "project").unwrap().is_none());
    assert!(
        workspace_overlay::mounts::mounted_type(&target, &Cancellation::default())
            .unwrap()
            .is_none()
    );
}

#[test]
fn external_primary_unmount_finishes_owner_without_global_cancellation() {
    fs::create_dir_all(".tmp").unwrap();
    let temp = tempfile::tempdir_in(".tmp").unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let target = root.join("project");
    fs::create_dir(&target).unwrap();
    let selection = Selection {
        root: root.clone(),
        project: "project".into(),
        target: target.clone(),
        markers: vec![],
        sources: vec![],
    };
    let store = Store::new(root.join("state")).unwrap();
    let records = store.clone();
    let cancel = Cancellation::default();
    let stopping = cancel.clone();
    let (send, events) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        session::serve(
            vec![selection],
            Options::default(),
            records,
            &stopping,
            None,
            &mut |event| {
                send.send(event).unwrap();
                Ok(())
            },
        )
    });
    await_target(&events, &target, true);
    workspace_overlay::mounts::unmount(&target, None, &Cancellation::default()).unwrap();
    await_target(&events, &target, false);
    worker.join().unwrap().unwrap();
    assert!(!cancel.is_cancelled());
    assert!(store.read(&root, "project").unwrap().is_none());
}
