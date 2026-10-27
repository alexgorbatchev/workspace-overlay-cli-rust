use crate::{
    config::{Selection, Source},
    notifications::Notifications,
    view::View,
};
use std::{fs, path::PathBuf, sync::Arc, time::Duration};

#[test]
fn source_events_use_retained_backing_handles_and_git_registrations_are_classified() {
    fs::create_dir_all(".tmp").unwrap();
    let temp = tempfile::tempdir_in(".tmp").unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    for path in ["project", "overlay", "metadata/worktrees/one"] {
        fs::create_dir_all(root.join(path)).unwrap();
    }
    let view = Arc::new(
        View::open(&Selection {
            root: root.clone(),
            project: "project".into(),
            target: root.join("project"),
            markers: vec![],
            sources: vec![Source {
                name: "overlay".into(),
                path: root.join("overlay"),
                glob: "**/*".into(),
            }],
        })
        .unwrap(),
    );
    let mut notifications = Notifications::new().unwrap();
    notifications.add_sources(&view).unwrap();
    notifications.add_git(&root.join("metadata")).unwrap();
    notifications.sync().unwrap();
    let stop = root.join("stop");
    let cancelled = crate::Cancellation::default();
    notifications.set_control(&root, &stop, cancelled.clone());
    notifications.sync().unwrap();
    fs::rename(root.join("overlay"), root.join("renamed")).unwrap();
    fs::write(root.join("renamed/changed"), b"live").unwrap();
    let event = loop {
        let event = notifications
            .receive(Duration::from_secs(2))
            .unwrap()
            .unwrap()
            .unwrap();
        if event
            .paths
            .iter()
            .any(|path| path.file_name() == Some(std::ffi::OsStr::new("changed")))
        {
            break event;
        }
    };
    assert!(notifications.relevant(&event));
    let git = notifications.git_roots().next().unwrap();
    for (tail, expected) in [
        ("refs/heads/main", false),
        ("worktrees", true),
        ("worktrees/one/gitdir", true),
        ("worktrees/one/locked", true),
    ] {
        let event = notify::Event::new(notify::EventKind::Create(notify::event::CreateKind::File))
            .add_path(git.join(tail));
        assert_eq!(notifications.relevant(&event), expected);
    }
    let event = notify::Event::new(notify::EventKind::Remove(notify::event::RemoveKind::Folder))
        .add_path(git.join("worktrees/one"));
    assert_eq!(
        notifications.unregistered(&event),
        vec![std::ffi::OsString::from("one")]
    );
    let event = notify::Event::new(notify::EventKind::Remove(notify::event::RemoveKind::Folder))
        .add_path(git.join("worktrees"));
    assert_eq!(
        notifications.unregistered(&event),
        vec![std::ffi::OsString::new()]
    );
    let event = notify::Event::new(notify::EventKind::Modify(
        notify::event::ModifyKind::Metadata(notify::event::MetadataKind::Permissions),
    ))
    .add_path(PathBuf::from("/other"));
    assert!(!notifications.relevant(&event));
    fs::remove_dir(root.join("metadata/worktrees/one")).unwrap();
    notifications.sync().unwrap();
    fs::write(stop, b"").unwrap();
    assert!(cancelled.wait(Duration::from_secs(2)));
    drop(notifications);
}
