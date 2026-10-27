use std::{
    fs,
    sync::{Arc, Mutex},
};
use workspace_overlay::{
    config::{Selection, Source},
    view::View,
};

struct Capture(Mutex<Vec<String>>);
static LOG: Capture = Capture(Mutex::new(Vec::new()));
impl log::Log for Capture {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Warn
            && matches!(
                metadata.target(),
                "workspace_overlay::file" | "workspace_overlay::git"
            )
    }
    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            self.0.lock().unwrap().push(record.args().to_string());
        }
    }
    fn flush(&self) {}
}

#[test]
fn native_binary_collision_logs_each_open_but_not_metadata_or_text_reads() {
    log::set_logger(&LOG).unwrap();
    log::set_max_level(log::LevelFilter::Warn);
    fs::create_dir_all(".tmp").unwrap();
    let temp = tempfile::tempdir_in(".tmp").unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    for name in ["project", "overlay"] {
        fs::create_dir(root.join(name)).unwrap();
        fs::write(root.join(name).join("text.txt"), name).unwrap();
    }
    fs::write(root.join("project/logo.png"), b"project").unwrap();
    fs::write(root.join("overlay/logo.png"), b"\0binary").unwrap();
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
    let mut mounted = view.mount().unwrap();
    assert!(fs::metadata(root.join("project/logo.png")).unwrap().len() > 0);
    assert_eq!(
        fs::read(root.join("project/text.txt")).unwrap(),
        b"projectoverlay"
    );
    assert!(LOG.0.lock().unwrap().is_empty());
    for _ in 0..2 {
        assert!(
            fs::read_to_string(root.join("project/logo.png"))
                .unwrap()
                .contains("Copies of this path:")
        );
    }
    mounted.unmount().unwrap();
    let records = LOG.0.lock().unwrap();
    assert_eq!(records.len(), 2, "{records:?}");
    assert!(
        records
            .iter()
            .all(|message| message.contains("cannot merge logo.png: binary collision"))
    );
    drop(records);
    LOG.0.lock().unwrap().clear();
    let invalid = root.join("incomplete");
    fs::create_dir(&invalid).unwrap();
    fs::write(invalid.join(".git"), b"gitdir: not-yet-created\n").unwrap();
    assert!(
        workspace_overlay::git::worktrees(
            &invalid,
            true,
            &workspace_overlay::Cancellation::default()
        )
        .is_err()
    );
    let retries = LOG.0.lock().unwrap();
    assert_eq!(retries.len(), 3, "{retries:?}");
    assert!(
        retries
            .iter()
            .all(|message| message.starts_with("discover worktrees:")
                && message.ends_with("; listing again"))
    );
}
