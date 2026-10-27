use crate::{
    config::{Selection, Source},
    view::View,
};
use std::{
    fs,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

struct Mount {
    session: Option<fuser::BackgroundSession>,
}
impl Drop for Mount {
    fn drop(&mut self) {
        if let Some(mut session) = self.session.take() {
            session.unmount().unwrap();
            session.join().unwrap();
        }
    }
}
fn mounted(
    glob: &str,
) -> (
    tempfile::TempDir,
    Arc<View>,
    Arc<crate::filesystem::Counts>,
    Mount,
) {
    fs::create_dir_all(".tmp").unwrap();
    let temp = tempfile::tempdir_in(".tmp").unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let project = root.join("project");
    let source = root.join("source");
    fs::create_dir(&project).unwrap();
    fs::create_dir(&source).unwrap();
    fs::write(project.join("plain"), b"base").unwrap();
    let view = Arc::new(
        View::open(&Selection {
            root,
            project: "project".into(),
            target: project,
            markers: vec![],
            sources: vec![Source {
                name: "source".into(),
                path: source,
                glob: glob.into(),
            }],
        })
        .unwrap(),
    );
    let filesystem = crate::filesystem::Filesystem::new(view.clone()).unwrap();
    let counts = filesystem.counts.clone();
    let cache = filesystem.cache.clone();
    let session =
        fuser::Session::new(filesystem, &view.layers[0].path, &fuser::Config::default()).unwrap();
    cache.bind(session.notifier());
    (
        temp,
        view,
        counts,
        Mount {
            session: Some(session.spawn().unwrap()),
        },
    )
}
fn before_timeout(mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_millis(500);
    while !check() {
        assert!(
            Instant::now() < deadline,
            "metadata was not invalidated before its one-second timeout"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn native_kernel_caches_project_metadata_and_negative_entries() {
    use std::sync::atomic::Ordering;
    let (_temp, view, counts, _mounted) = mounted("docs/**");
    let target = &view.layers[0].path;
    fs::metadata(target.join("plain")).unwrap();
    let before = (
        counts.lookups.load(Ordering::Relaxed),
        counts.attrs.load(Ordering::Relaxed),
    );
    for _ in 0..100 {
        assert_eq!(fs::metadata(target.join("plain")).unwrap().len(), 4);
    }
    assert_eq!(
        (
            counts.lookups.load(Ordering::Relaxed),
            counts.attrs.load(Ordering::Relaxed)
        ),
        before
    );
    let before = counts.lookups.load(Ordering::Relaxed);
    for _ in 0..100 {
        assert_eq!(
            fs::metadata(target.join("missing")).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
    }
    assert_eq!(counts.lookups.load(Ordering::Relaxed) - before, 1);
    // Future overlay paths never acquire negative/attribute timeouts.
    let before = counts.lookups.load(Ordering::Relaxed);
    for _ in 0..10 {
        assert!(fs::metadata(target.join("docs")).is_err());
    }
    assert_eq!(counts.lookups.load(Ordering::Relaxed) - before, 10);
}

#[test]
fn native_backing_events_invalidate_missing_attributes_and_names() {
    use std::os::unix::fs::PermissionsExt;
    let (_temp, view, _counts, _mounted) = mounted("docs/**");
    let target = &view.layers[0].path;
    let backing = &view.layers[0].root;
    assert_eq!(
        fs::metadata(target.join("missing")).unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );
    backing.write("missing", b"one").unwrap();
    before_timeout(|| fs::metadata(target.join("missing")).is_ok_and(|info| info.len() == 3));
    backing.write("missing", b"longer").unwrap();
    before_timeout(|| fs::metadata(target.join("missing")).is_ok_and(|info| info.len() == 6));
    backing
        .set_permissions(
            "missing",
            cap_std::fs::Permissions::from_std(fs::Permissions::from_mode(0o600)),
        )
        .unwrap();
    before_timeout(|| {
        fs::metadata(target.join("missing"))
            .is_ok_and(|info| info.permissions().mode() & 0o777 == 0o600)
    });
    backing.rename("missing", backing, "renamed").unwrap();
    before_timeout(|| fs::metadata(target.join("missing")).is_err());
    assert_eq!(fs::metadata(target.join("renamed")).unwrap().len(), 6);
    backing.remove_file("renamed").unwrap();
    before_timeout(|| fs::metadata(target.join("renamed")).is_err());
}

#[test]
fn metadata_watch_limits_aliases_expiration_and_future_overlay_namespaces() {
    fs::create_dir_all(".tmp").unwrap();
    let temp = tempfile::tempdir_in(".tmp").unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let project = root.join("project");
    let overlay = root.join("overlay");
    fs::create_dir(&project).unwrap();
    fs::create_dir(&overlay).unwrap();
    let view = Arc::new(
        View::open(&Selection {
            root: root.clone(),
            project: "project".into(),
            target: project.clone(),
            markers: vec![],
            sources: vec![Source {
                name: "overlay".into(),
                path: overlay.clone(),
                glob: "docs/**/*.md".into(),
            }],
        })
        .unwrap(),
    );
    let cache = crate::metadata::Cache::new(Arc::downgrade(&view)).unwrap();
    assert_eq!(
        cache.remember(
            &view,
            1,
            Path::new("."),
            std::ffi::OsStr::new("native"),
            None
        ),
        Duration::from_secs(1)
    );
    assert!(!view.project_namespace(Path::new("docs")));
    for n in 0..1023 {
        assert!(
            !cache
                .remember(
                    &view,
                    1,
                    Path::new("."),
                    std::ffi::OsStr::new(&format!("missing-{n}")),
                    None
                )
                .is_zero()
        );
    }
    assert_eq!(
        cache.remember(
            &view,
            1,
            Path::new("."),
            std::ffi::OsStr::new("over-budget"),
            None
        ),
        Duration::ZERO
    );
    {
        let mut state = cache.shared.state.lock();
        for directory in state.directories.values_mut() {
            for value in directory.entries.values_mut() {
                value.expires = Instant::now() - Duration::from_secs(1);
            }
        }
    }
    assert!(
        !cache
            .remember(
                &view,
                1,
                Path::new("."),
                std::ffi::OsStr::new("over-budget"),
                None
            )
            .is_zero()
    );
    assert!(
        !cache
            .remember(
                &view,
                2,
                Path::new("."),
                std::ffi::OsStr::new("alias"),
                None
            )
            .is_zero()
    );
    assert_eq!(cache.shared.state.lock().directories.len(), 1);
    for n in 0..254 {
        let path = format!("dir-{n}");
        fs::create_dir(project.join(&path)).unwrap();
        assert!(
            !cache
                .remember(
                    &view,
                    n + 3,
                    Path::new(&path),
                    std::ffi::OsStr::new("missing"),
                    None
                )
                .is_zero()
        );
    }
    fs::create_dir(project.join("overflow")).unwrap();
    assert!(
        cache
            .remember(
                &view,
                300,
                Path::new("overflow"),
                std::ffi::OsStr::new("missing"),
                None
            )
            .is_zero()
    );
    cache
        .shared
        .disable(crate::Error::Invalid("controlled watcher failure".into()));
    let warning = cache.warning().unwrap();
    assert!(warning.to_string().starts_with("disable metadata caching:"));
    assert!(
        matches!(warning, crate::Error::Context { source, .. } if matches!(*source, crate::Error::Invalid(_)))
    );
    assert!(
        cache
            .remember(
                &view,
                1,
                Path::new("."),
                std::ffi::OsStr::new("native"),
                None
            )
            .is_zero()
    );
}
