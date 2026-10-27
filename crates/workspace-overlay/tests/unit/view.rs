use crate::{
    config::{Selection, Source},
    view::View,
};
use std::{
    fs,
    os::unix::{ffi::OsStrExt, fs::symlink},
    path::Path,
};

fn setup() -> (tempfile::TempDir, View) {
    fs::create_dir_all(".tmp").unwrap();
    let temp = tempfile::tempdir_in(".tmp").unwrap();
    for dir in ["project", "first", "last"] {
        fs::create_dir(temp.path().join(dir)).unwrap();
    }
    let selection = Selection {
        root: temp.path().to_path_buf(),
        project: "project".into(),
        target: temp.path().join("project"),
        markers: vec![],
        sources: vec![
            Source {
                name: "first".into(),
                path: temp.path().join("first"),
                glob: "**/*".into(),
            },
            Source {
                name: "last".into(),
                path: temp.path().join("last"),
                glob: "nested/**/*.md".into(),
            },
        ],
    };
    (temp, View::open(&selection).unwrap())
}

#[test]
fn rooted_visibility_order_types_and_destinations() {
    let (temp, view) = setup();
    fs::write(temp.path().join("project/AGENTS.md"), b"base").unwrap();
    fs::write(temp.path().join("first/AGENTS.md"), b"overlay").unwrap();
    fs::create_dir_all(temp.path().join("last/nested/deep")).unwrap();
    fs::write(temp.path().join("last/nested/deep/file.md"), b"last").unwrap();
    fs::write(temp.path().join("last/hidden.md"), b"hidden").unwrap();
    fs::create_dir_all(temp.path().join("first/.git")).unwrap();
    fs::write(temp.path().join("first/.git/config"), b"hidden").unwrap();
    view.refresh_paths().unwrap();
    assert_eq!(
        view.resolve(Path::new("AGENTS.md"))
            .unwrap()
            .iter()
            .map(|p| p.layer)
            .collect::<Vec<_>>(),
        vec![0, 1]
    );
    assert_eq!(view.resolve(Path::new("nested")).unwrap()[0].layer, 2);
    assert!(view.resolve(Path::new("hidden.md")).is_err());
    assert!(view.resolve(Path::new(".git")).is_err());
    assert_eq!(view.destination(Path::new("new.txt")).unwrap(), 0);
    assert_eq!(
        view.destination(Path::new("nested/deep/new.md")).unwrap(),
        2
    );
    assert_eq!(
        view.destination(Path::new("nested/deep/new.txt"))
            .unwrap_err()
            .raw_os_error(),
        Some(libc::EPERM)
    );
    let names = view
        .entries(Path::new("."))
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect::<Vec<_>>();
    assert_eq!(names, vec!["AGENTS.md", "nested"]);
    assert_eq!(
        view.entries(Path::new("AGENTS.md"))
            .unwrap_err()
            .raw_os_error(),
        Some(libc::ENOTDIR)
    );
    fs::create_dir(temp.path().join("first/mismatch")).unwrap();
    fs::write(temp.path().join("project/mismatch"), b"x").unwrap();
    view.refresh_paths().unwrap();
    assert_eq!(
        view.resolve(Path::new("mismatch"))
            .unwrap_err()
            .raw_os_error(),
        Some(libc::EIO)
    );
    symlink("../outside", temp.path().join("first/link")).unwrap();
    view.refresh_paths().unwrap();
    assert_eq!(
        view.read_link(Path::new("link"))
            .unwrap()
            .as_os_str()
            .as_bytes(),
        b"../outside"
    );
    symlink("../other", temp.path().join("project/link")).unwrap();
    assert_eq!(
        view.resolve(Path::new("link")).unwrap_err().raw_os_error(),
        Some(libc::EIO)
    );
    assert!(view.resolve(Path::new("../project")).is_err());
}

#[test]
fn stable_identity_preopened_roots_and_unfollowed_filter_walk() {
    let (temp, view) = setup();
    fs::write(temp.path().join("project/one"), b"x").unwrap();
    fs::hard_link(
        temp.path().join("project/one"),
        temp.path().join("project/two"),
    )
    .unwrap();
    let one = view.resolve(Path::new("one")).unwrap();
    let two = view.resolve(Path::new("two")).unwrap();
    assert_eq!(view.stable(&one[0]), view.stable(&two[0]));
    fs::write(temp.path().join("replacement"), b"y").unwrap();
    fs::rename(
        temp.path().join("replacement"),
        temp.path().join("project/one"),
    )
    .unwrap();
    assert_ne!(
        view.stable(&one[0]),
        view.stable(&view.resolve(Path::new("one")).unwrap()[0])
    );
    fs::rename(
        temp.path().join("project"),
        temp.path().join("hidden-project"),
    )
    .unwrap();
    fs::create_dir(temp.path().join("project")).unwrap();
    assert_eq!(view.layers[0].root.read("one").unwrap(), b"y");
    fs::create_dir(temp.path().join("outside")).unwrap();
    fs::write(temp.path().join("outside/secret.md"), b"secret").unwrap();
    symlink("../outside", temp.path().join("first/escape")).unwrap();
    view.refresh_paths().unwrap();
    assert!(view.resolve(Path::new("escape/secret.md")).is_err());
    assert!(view.resolve(Path::new("escape")).is_ok());
}
