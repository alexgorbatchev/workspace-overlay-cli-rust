use crate::{
    config::{Selection, Source},
    view::View,
};
use std::{fs, path::Path, sync::Arc};

fn setup() -> (tempfile::TempDir, Arc<View>) {
    fs::create_dir_all(".tmp").unwrap();
    let temp = tempfile::tempdir_in(".tmp").unwrap();
    for root in ["project", "overlay"] {
        fs::create_dir(temp.path().join(root)).unwrap();
    }
    fs::create_dir(temp.path().join("overlay/only")).unwrap();
    fs::write(temp.path().join("overlay/only/existing.txt"), b"protected").unwrap();
    let view = View::open(&Selection {
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
    .unwrap();
    (temp, Arc::new(view))
}
fn code<T>(result: std::io::Result<T>) -> i32 {
    result.err().unwrap().raw_os_error().unwrap()
}
fn p(value: &str) -> &Path {
    Path::new(value)
}

#[test]
fn atomic_save_retains_both_native_preparation_and_restoration_failures() {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "mutation_tests::atomic_save_write_failure_helper",
            "--ignored",
            "--nocapture",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[ignore = "isolated child changes its own file-size limit"]
fn atomic_save_write_failure_helper() {
    let (temp, view) = setup();
    fs::write(temp.path().join("project/plain.txt"), b"base-").unwrap();
    fs::write(temp.path().join("overlay/plain.txt"), b"original").unwrap();
    view.refresh_paths().unwrap();
    let staging = view
        .create_file(p("stage"), libc::O_RDWR, 0o600, 0)
        .unwrap();
    staging.write(0, b"base-updated").unwrap();
    staging.release().unwrap();
    let mut original_limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: only this isolated child changes its process-local limits and signal disposition.
    unsafe {
        assert_eq!(libc::getrlimit(libc::RLIMIT_FSIZE, &mut original_limit), 0);
        assert_ne!(libc::signal(libc::SIGXFSZ, libc::SIG_IGN), libc::SIG_ERR);
        assert_eq!(
            libc::setrlimit(
                libc::RLIMIT_FSIZE,
                &libc::rlimit {
                    rlim_cur: 2,
                    rlim_max: original_limit.rlim_max
                }
            ),
            0
        );
    }
    let result = view.rename(p("stage"), p("plain.txt"), 0);
    // Restore before test reporting and coverage output writes.
    assert_eq!(
        unsafe { libc::setrlimit(libc::RLIMIT_FSIZE, &original_limit) },
        0
    );
    let error = result.unwrap_err();
    let failures = error
        .get_ref()
        .unwrap()
        .downcast_ref::<crate::Failures>()
        .unwrap();
    assert_eq!(failures.0.len(), 2);
    assert!(failures.0.iter().all(|error| matches!(error, crate::Error::Io(error) if error.raw_os_error() == Some(libc::EFBIG))));
    assert_eq!(
        fs::read(temp.path().join("overlay/plain.txt")).unwrap(),
        b"original"
    );
    assert_eq!(fs::read(temp.path().join("project/stage")).unwrap(), b"ba");
}

#[test]
fn created_overlay_entries_move_remove_and_existing_sources_stay_protected() {
    let (temp, view) = setup();
    assert_eq!(code(view.unlink(p("only/existing.txt"))), libc::EPERM);
    assert_eq!(
        code(view.rename(p("only/existing.txt"), p("only/moved.txt"), 0)),
        libc::EPERM
    );
    let file = view
        .create_file(p("only/new.txt"), libc::O_RDWR, 0o640, 0)
        .unwrap();
    file.write(0, b"new").unwrap();
    file.release().unwrap();
    assert_eq!(
        fs::read(temp.path().join("overlay/only/new.txt")).unwrap(),
        b"new"
    );
    view.rename(p("only/new.txt"), p("only/moved.txt"), 0)
        .unwrap();
    view.unlink(p("only/moved.txt")).unwrap();
    view.mkdir(p("only/sub"), 0o750).unwrap();
    view.symlink(p("../existing.txt"), p("only/sub/link"))
        .unwrap();
    assert_eq!(
        view.read_link(p("only/sub/link")).unwrap(),
        p("../existing.txt")
    );
    assert_eq!(code(view.rmdir(p("only/sub"))), libc::ENOTEMPTY);
    view.unlink(p("only/sub/link")).unwrap();
    view.rmdir(p("only/sub")).unwrap();
    assert_eq!(code(view.rmdir(p("only"))), libc::ENOTEMPTY);
    fs::remove_file(temp.path().join("overlay/only/existing.txt")).unwrap();
    view.refresh_paths().unwrap();
    assert_eq!(code(view.rmdir(p("only"))), libc::EPERM);
    assert_eq!(
        code(view.rename(p("missing"), p("missing"), 1)),
        libc::ENOTSUP
    );
    view.rename(p("missing"), p("missing"), 0).unwrap();
}

#[test]
fn marked_atomic_save_and_removed_project_section_restore() {
    use std::os::unix::fs::PermissionsExt;
    let (temp, view) = setup();
    fs::write(temp.path().join("project/AGENTS.md"), b"base\n").unwrap();
    fs::set_permissions(
        temp.path().join("project/AGENTS.md"),
        fs::Permissions::from_mode(0o640),
    )
    .unwrap();
    fs::write(temp.path().join("overlay/AGENTS.md"), b"overlay\n").unwrap();
    view.refresh_paths().unwrap();
    let document = view.open_file(p("AGENTS.md"), libc::O_RDONLY, 0).unwrap();
    let original = String::from_utf8(document.read(0, 10000).unwrap()).unwrap();
    let edited = original
        .replace("base\n", "changed base\n")
        .replace("overlay\n", "changed overlay\n");
    let staging = view
        .create_file(p("stage"), libc::O_RDWR, 0o600, 0)
        .unwrap();
    staging.write(0, edited.as_bytes()).unwrap();
    view.rename(p("stage"), p("AGENTS.md"), 0).unwrap();
    assert_eq!(
        fs::read(temp.path().join("project/AGENTS.md")).unwrap(),
        b"changed base\n"
    );
    assert_eq!(
        fs::read(temp.path().join("overlay/AGENTS.md")).unwrap(),
        b"changed overlay\n"
    );
    assert_eq!(staging.read(0, 1000).unwrap(), b"changed overlay\n");
    staging.release().unwrap();
    view.unlink(p("AGENTS.md")).unwrap();
    assert_eq!(code(view.unlink(p("AGENTS.md"))), libc::EPERM);
    let remaining = view.open_file(p("AGENTS.md"), libc::O_RDWR, 0).unwrap();
    let mut restored = b"restored\n".to_vec();
    restored.extend(remaining.read(0, 10000).unwrap());
    remaining.truncate(restored.len() as u64).unwrap();
    remaining.write(0, &restored).unwrap();
    remaining.flush().unwrap();
    assert_eq!(
        fs::read(temp.path().join("project/AGENTS.md")).unwrap(),
        b"restored\n"
    );
    // Atomic rename replaced the overlay file but preserved the project mode.
    assert_eq!(
        fs::metadata(temp.path().join("project/AGENTS.md"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o640
    );
}

#[test]
fn atomic_save_validates_prefix_binary_markers_and_native_hard_links() {
    let (temp, view) = setup();
    for (name, base, overlay) in [
        ("plain.txt", &b"base-"[..], &b"old"[..]),
        ("AGENTS.md", &b"base\n"[..], &b"old\n"[..]),
        ("binary", &b"text"[..], &b"\0binary"[..]),
    ] {
        fs::write(temp.path().join("project").join(name), base).unwrap();
        fs::write(temp.path().join("overlay").join(name), overlay).unwrap();
    }
    view.refresh_paths().unwrap();
    let file = view
        .create_file(p("stage"), libc::O_RDWR, 0o600, 0)
        .unwrap();
    file.write(0, b"base-updated").unwrap();
    view.rename(p("stage"), p("plain.txt"), 0).unwrap();
    file.release().unwrap();
    assert_eq!(
        fs::read(temp.path().join("overlay/plain.txt")).unwrap(),
        b"updated"
    );
    for name in ["plain.txt", "AGENTS.md", "binary"] {
        let file = view
            .create_file(p("invalid"), libc::O_RDWR, 0o600, 0)
            .unwrap();
        file.write(0, b"corrupt").unwrap();
        assert_eq!(code(view.rename(p("invalid"), p(name), 0)), libc::EPERM);
        assert_eq!(file.read(0, 100).unwrap(), b"corrupt");
        file.release().unwrap();
        view.unlink(p("invalid")).unwrap();
    }
    let file = view
        .create_file(p("native"), libc::O_RDWR, 0o640, 0)
        .unwrap();
    file.write(0, b"linked").unwrap();
    file.release().unwrap();
    view.hard_link(p("native"), p("alias")).unwrap();
    assert_eq!(
        view.metadata(p("native"), 0).unwrap().ino,
        view.metadata(p("alias"), 0).unwrap().ino
    );
    assert_eq!(view.metadata(p("native"), 0).unwrap().nlink, 2);
    assert_eq!(code(view.hard_link(p("plain.txt"), p("bad"))), libc::EXDEV);
    assert_eq!(
        code(view.create_file(p("native"), libc::O_RDWR, 0o640, 0)),
        libc::EEXIST
    );
}
