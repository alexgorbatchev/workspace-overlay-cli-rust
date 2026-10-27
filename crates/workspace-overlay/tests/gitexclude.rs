use std::{ffi::OsStr, fs, os::unix::ffi::OsStrExt};
use workspace_overlay::gitexclude::{Exclude, pattern};
use workspace_overlay::{
    Cancellation,
    config::{Selection, Source},
    view::View,
};

#[test]
fn concurrent_blocks_preserve_user_data_and_restore_exactly() {
    fs::create_dir_all(".tmp").unwrap();
    let temp = tempfile::tempdir_in(".tmp").unwrap();
    let target = temp.path().join("exclude");
    fs::write(&target, b"user pattern\n").unwrap();
    let mut first = Exclude::at(&target).unwrap();
    let mut second = first.fork().unwrap();
    let raw = pattern(OsStr::from_bytes(b"only/a [b]*?#!\\\xff")).unwrap();
    first.update(&[raw]).unwrap();
    second.update(&[b"/other".to_vec()]).unwrap();
    let block = first.block().to_owned();
    let contents = fs::read(&target).unwrap();
    assert!(contents.starts_with(b"user pattern\n"));
    assert!(contents.windows(block.len()).any(|window| window == block));
    first.close().unwrap();
    assert!(
        fs::read(&target)
            .unwrap()
            .windows(6)
            .any(|window| window == b"/other")
    );
    second.close().unwrap();
    assert_eq!(fs::read(&target).unwrap(), b"user pattern\n");
    assert!(pattern(OsStr::from_bytes(b"line\nname")).is_err());
    assert!(pattern(OsStr::from_bytes(b"line\rname")).is_err());
    let mut owner = Exclude::at(&target).unwrap();
    owner.update(&[b"/keep".to_vec()]).unwrap();
    let block = owner.block().to_owned();
    let mut duplicated = fs::read(&target).unwrap();
    duplicated.extend(&block);
    fs::write(&target, &duplicated).unwrap();
    assert!(owner.close().is_err());
    assert_eq!(fs::read(&target).unwrap(), duplicated);
    fs::write(&target, b"user pattern\n").unwrap();
    let mut saved = b"user pattern\n".to_vec();
    saved.extend(&block);
    fs::write(&target, saved).unwrap();
    Exclude::restore(&target, &block).unwrap();
    assert_eq!(fs::read(&target).unwrap(), b"user pattern\n");
}

#[test]
fn view_refresh_excludes_only_visible_overlay_paths_missing_from_project() {
    fs::create_dir_all(".tmp").unwrap();
    let temp = tempfile::tempdir_in(".tmp").unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let base = root.join("base");
    let overlay = root.join("overlay");
    fs::create_dir(&base).unwrap();
    fs::create_dir(&overlay).unwrap();
    assert!(
        std::process::Command::new("git")
            .arg("init")
            .arg("-q")
            .arg(&base)
            .status()
            .unwrap()
            .success()
    );
    fs::write(base.join("shared"), b"base").unwrap();
    fs::write(overlay.join("shared"), b"overlay").unwrap();
    fs::create_dir(overlay.join("only")).unwrap();
    fs::write(overlay.join("only/file"), b"only").unwrap();
    fs::write(overlay.join("space *"), b"special").unwrap();
    fs::create_dir(overlay.join(".git")).unwrap();
    fs::write(overlay.join(".git/secret"), b"hidden").unwrap();
    let view = View::open(&Selection {
        root: root.clone(),
        project: "base".into(),
        target: base.clone(),
        markers: vec![],
        sources: vec![Source {
            name: "overlay".into(),
            path: overlay.clone(),
            glob: "**/*".into(),
        }],
    })
    .unwrap();
    let exclude = Exclude::open(&base, &Cancellation::default())
        .unwrap()
        .unwrap();
    let path = exclude.path().to_owned();
    let original = fs::read(&path).unwrap();
    let updates = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let capture = updates.clone();
    view.attach_exclude(exclude, move |block| {
        capture.lock().unwrap().push(block.to_owned());
        Ok(())
    })
    .unwrap();
    let data = fs::read(&path).unwrap();
    assert!(data.windows(6).any(|bytes| bytes == b"/only\n"));
    assert!(data.windows(11).any(|bytes| bytes == b"/space\\ \\*\n"));
    assert!(!data.windows(7).any(|bytes| bytes == b"/shared"));
    assert!(!data.windows(5).any(|bytes| bytes == b"/.git"));
    assert!(!data.windows(10).any(|bytes| bytes == b"/only/file"));
    view.refresh_exclude().unwrap();
    assert_eq!(updates.lock().unwrap().len(), 1);
    fs::write(overlay.join("added"), b"new").unwrap();
    view.refresh_paths().unwrap();
    assert!(
        fs::read(&path)
            .unwrap()
            .windows(7)
            .any(|bytes| bytes == b"/added\n")
    );
    fs::write(base.join("added"), b"project now owns it").unwrap();
    view.refresh_paths().unwrap();
    assert!(
        !fs::read(&path)
            .unwrap()
            .windows(7)
            .any(|bytes| bytes == b"/added\n")
    );
    view.close_exclude().unwrap();
    assert_eq!(fs::read(&path).unwrap(), original);
    assert!(updates.lock().unwrap().last().unwrap().is_empty());
}
