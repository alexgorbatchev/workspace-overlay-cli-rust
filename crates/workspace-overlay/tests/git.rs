use std::{ffi::OsString, fs, os::unix::ffi::OsStringExt, path::Path, process::Command};
use workspace_overlay::{Cancellation, git, gitexclude::Exclude, subprocess};

fn run(root: &Path, args: &[&str]) {
    let mut command = Command::new("git");
    command.arg("-C").arg(root).args(args);
    subprocess::output(&mut command, &Cancellation::default()).unwrap();
}

#[test]
fn native_worktrees_and_exclude_preserve_byte_paths_and_repository_identity() {
    fs::create_dir_all(".tmp").unwrap();
    let temporary = tempfile::tempdir_in(".tmp").unwrap();
    let root = fs::canonicalize(temporary.path()).unwrap();
    let main = root.join(OsString::from_vec(b"main-\xff \n".to_vec()));
    fs::create_dir(&main).unwrap();
    run(&main, &["init", "-b", "main"]);
    run(
        &main,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.com",
            "commit",
            "--allow-empty",
            "-m",
            "fixture",
        ],
    );
    let worktree = root.join(OsString::from_vec(b"work-\xfe\n".to_vec()));
    let mut command = git::command(&main);
    command.args(["worktree", "add", "--detach"]).arg(&worktree);
    subprocess::output(&mut command, &Cancellation::default()).unwrap();
    let token = Cancellation::default();
    assert_eq!(
        git::worktrees(&main, true, &token).unwrap(),
        vec![main.clone(), worktree.clone()]
    );
    assert_eq!(
        git::worktrees(&worktree, true, &token).unwrap(),
        vec![worktree.clone(), main.clone()]
    );
    assert_eq!(
        git::worktrees(&main, false, &token).unwrap(),
        vec![main.clone()]
    );
    let exclude = Exclude::open(&main, &token).unwrap().unwrap();
    assert_eq!(exclude.path(), main.join(".git/info/exclude"));
    let linked = Exclude::open(&worktree, &token).unwrap().unwrap();
    assert_eq!(linked.path(), exclude.path());
    let nested = main.join("nested");
    fs::create_dir(&nested).unwrap();
    assert!(Exclude::open(&nested, &token).unwrap().is_none());
    assert_eq!(
        git::worktrees(&nested, true, &token).unwrap(),
        vec![nested.clone()]
    );
    fs::write(nested.join(".git"), b"gitdir: nonexistent\n").unwrap();
    assert!(git::worktrees(&nested, true, &token).is_err());
    assert!(Exclude::open(&nested, &token).is_err());
}
