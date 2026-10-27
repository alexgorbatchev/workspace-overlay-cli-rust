use std::{
    ffi::OsString,
    fs,
    os::unix::ffi::OsStringExt,
    path::{Path, PathBuf},
    process::Command,
};
use workspace_overlay::{Cancellation, Error, config::Configuration, fixture, git, subprocess};

fn temporary() -> tempfile::TempDir {
    fs::create_dir_all(".tmp").unwrap();
    tempfile::tempdir_in(".tmp").unwrap()
}

fn git_output(target: &Path, args: &[&str]) -> Vec<u8> {
    subprocess::output(git::command(target).args(args), &Cancellation::default()).unwrap()
}

fn root_error(mut error: &Error) -> &Error {
    while let Error::Context { source, .. } = error {
        error = source;
    }
    error
}

#[test]
fn creates_native_workspace_and_linked_repositories_then_reuses_without_writes() {
    let temp = temporary();
    let parent = fs::canonicalize(temp.path()).unwrap();
    let root = parent.join(OsString::from_vec(b"workspace space\nbyte-\xff".to_vec()));
    let config_file = fixture::create(&root, &Cancellation::default()).unwrap();
    assert_eq!(config_file, root.join("workspace-overlay.toml"));
    let config = Configuration::load(&config_file).unwrap();
    let selections = config.selections(None).unwrap();
    assert_eq!(selections.len(), 2);
    for selection in selections {
        assert_eq!(selection.sources.len(), 2);
        assert_eq!(selection.sources[0].name, "workspace");
        assert_eq!(selection.sources[1].name, selection.project);
        let worktree = root.join(".workspaces/one").join(&selection.project);
        assert_eq!(
            git::worktrees(&selection.target, true, &Cancellation::default()).unwrap(),
            [selection.target.clone(), worktree.clone()]
        );
        assert_eq!(
            git_output(&selection.target, &["branch", "--show-current"]),
            b"main\n"
        );
        assert_eq!(
            git_output(&worktree, &["branch", "--show-current"]),
            b"verification\n"
        );
        assert_eq!(
            git_output(&selection.target, &["rev-parse", "HEAD"]),
            git_output(&worktree, &["rev-parse", "HEAD"])
        );
        assert!(
            git_output(
                &selection.target,
                &["status", "--porcelain", "--untracked-files=all"]
            )
            .is_empty()
        );
        assert_eq!(
            fs::read(worktree.join("AGENTS.md")).unwrap(),
            fs::read(selection.target.join("AGENTS.md")).unwrap()
        );
        for source in &selection.sources {
            assert!(source.path.join("AGENTS.md").is_file());
            assert!(source.path.join("docs/nested/notes.md").is_file());
            let skill = if source.name == "workspace" {
                "workspace-check".to_owned()
            } else {
                format!("{}-check", source.name)
            };
            assert!(
                source
                    .path
                    .join(".agents/skills")
                    .join(&skill)
                    .join("SKILL.md")
                    .is_file()
            );
            assert!(
                source
                    .path
                    .join(".agents/skills")
                    .join(skill)
                    .join("reference.md")
                    .is_file()
            );
        }
    }
    let changed = root.join("alpha/AGENTS.md");
    fs::write(&changed, b"user's changes\n").unwrap();
    let before = fs::metadata(&changed).unwrap().modified().unwrap();
    assert_eq!(
        fixture::create(&root, &Cancellation::default()).unwrap(),
        config_file
    );
    assert_eq!(fs::read(&changed).unwrap(), b"user's changes\n");
    assert_eq!(fs::metadata(changed).unwrap().modified().unwrap(), before);
}

#[test]
fn refuses_unowned_incomplete_or_damaged_destinations_and_retains_cancelled_data() {
    let temp = temporary();
    for marker in [None, Some(b"wrong marker\n".as_slice())] {
        let root = temp
            .path()
            .join(if marker.is_some() { "wrong" } else { "unowned" });
        fs::create_dir(&root).unwrap();
        fs::write(root.join("keep"), b"existing").unwrap();
        if let Some(marker) = marker {
            fs::write(root.join(".workspace-overlay-fixture"), marker).unwrap();
        }
        assert!(fixture::create(&root, &Cancellation::default()).is_err());
        assert_eq!(fs::read(root.join("keep")).unwrap(), b"existing");
        assert!(!root.join("alpha").exists());
    }
    let file = temp.path().join("file");
    fs::write(&file, b"keep").unwrap();
    assert!(fixture::create(&file, &Cancellation::default()).is_err());
    assert!(fixture::create(&file.join("child"), &Cancellation::default()).is_err());
    assert_eq!(fs::read(file).unwrap(), b"keep");

    let cancelled = temp.path().join("cancelled");
    let cancel = Cancellation::default();
    cancel.cancel();
    let error = fixture::create(&cancelled, &cancel).unwrap_err();
    assert!(matches!(root_error(&error), Error::Cancelled));
    assert!(cancelled.join("alpha/AGENTS.md").is_file());
    assert!(!cancelled.join(".workspace-overlay-fixture").exists());
    assert!(fixture::create(&cancelled, &Cancellation::default()).is_err());

    let damaged = temp.path().join("damaged");
    let config = fixture::create(&damaged, &Cancellation::default()).unwrap();
    fs::write(config, b"invalid TOML [").unwrap();
    assert!(fixture::create(&damaged, &Cancellation::default()).is_err());
}

// A separate harness process gives the fixture real inherited Git overrides
// without mutating this parallel test process's environment.
#[test]
fn fixture_child() {
    let Some(root) = std::env::var_os("WORKSPACE_FIXTURE_TEST_ROOT") else {
        return;
    };
    let result = fixture::create(Path::new(&root), &Cancellation::default());
    if std::env::var_os("WORKSPACE_FIXTURE_EXPECT_FAILURE").is_some() {
        let error = result.unwrap_err();
        assert!(
            matches!(root_error(&error), Error::Io(error) if error.kind() == std::io::ErrorKind::NotFound)
        );
    } else {
        result.unwrap();
    }
}

fn child(root: &Path) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--exact", "fixture_child", "--nocapture"]);
    command.env("WORKSPACE_FIXTURE_TEST_ROOT", root);
    command
}

#[test]
fn fixture_git_ignores_inherited_repository_and_configuration_overrides() {
    let temp = temporary();
    let foreign = temp.path().join("foreign");
    fs::create_dir(&foreign).unwrap();
    fs::write(foreign.join("keep"), b"preserved").unwrap();
    let root = temp.path().join("workspace");
    let output = child(&root)
        .env("GIT_DIR", foreign.join(".git"))
        .env("GIT_WORK_TREE", &foreign)
        .env("GIT_INDEX_FILE", foreign.join("index"))
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "core.hooksPath")
        .env("GIT_CONFIG_VALUE_0", foreign.join("hooks"))
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(fs::read(foreign.join("keep")).unwrap(), b"preserved");
    assert_eq!(fs::read_dir(foreign).unwrap().count(), 1);
    assert!(root.join("alpha/.git").is_dir());
    assert!(root.join(".workspaces/one/beta/.git").is_file());
}

#[test]
fn failed_git_initialization_keeps_incomplete_workspace_and_never_marks_it_complete() {
    let temp = temporary();
    let root = temp.path().join("workspace");
    let output = child(&root)
        .env("PATH", PathBuf::from("/nonexistent-workspace-fixture-bin"))
        .env("WORKSPACE_FIXTURE_EXPECT_FAILURE", "1")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(root.join("beta/src/message.txt").is_file());
    assert!(!root.join(".workspace-overlay-fixture").exists());
    assert!(fixture::create(&root, &Cancellation::default()).is_err());
}
