//! Embedded verification workspaces with real Git repositories and worktrees.
use crate::{
    Cancellation, Error, Result,
    config::{Configuration, NAME},
    file::{close_native, finish_io},
    git, subprocess,
};
use path_clean::PathClean;
use std::{
    ffi::OsString,
    fs::{self, DirBuilder, OpenOptions},
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

const MARKER: &str = ".workspace-overlay-fixture";
const COMPLETE: &[u8] = b"workspace-overlay fixture v1\n";
const FILES: &[(&str, &[u8])] = &[
    (
        "workspace-overlay.toml",
        include_bytes!("../assets/fixture/workspace-overlay.toml"),
    ),
    (
        "alpha/AGENTS.md",
        include_bytes!("../assets/fixture/alpha/AGENTS.md"),
    ),
    (
        "alpha/docs/nested/notes.md",
        include_bytes!("../assets/fixture/alpha/docs/nested/notes.md"),
    ),
    (
        "alpha/src/message.txt",
        include_bytes!("../assets/fixture/alpha/src/message.txt"),
    ),
    (
        "beta/AGENTS.md",
        include_bytes!("../assets/fixture/beta/AGENTS.md"),
    ),
    (
        "beta/docs/nested/notes.md",
        include_bytes!("../assets/fixture/beta/docs/nested/notes.md"),
    ),
    (
        "beta/src/message.txt",
        include_bytes!("../assets/fixture/beta/src/message.txt"),
    ),
    (
        ".ai/workspace/AGENTS.md",
        include_bytes!("../assets/fixture/.ai/workspace/AGENTS.md"),
    ),
    (
        ".ai/workspace/docs/nested/notes.md",
        include_bytes!("../assets/fixture/.ai/workspace/docs/nested/notes.md"),
    ),
    (
        ".ai/workspace/.agents/skills/workspace-check/SKILL.md",
        include_bytes!("../assets/fixture/.ai/workspace/.agents/skills/workspace-check/SKILL.md"),
    ),
    (
        ".ai/workspace/.agents/skills/workspace-check/reference.md",
        include_bytes!(
            "../assets/fixture/.ai/workspace/.agents/skills/workspace-check/reference.md"
        ),
    ),
    (
        ".ai/alpha/AGENTS.md",
        include_bytes!("../assets/fixture/.ai/alpha/AGENTS.md"),
    ),
    (
        ".ai/alpha/docs/nested/notes.md",
        include_bytes!("../assets/fixture/.ai/alpha/docs/nested/notes.md"),
    ),
    (
        ".ai/alpha/.agents/skills/alpha-check/SKILL.md",
        include_bytes!("../assets/fixture/.ai/alpha/.agents/skills/alpha-check/SKILL.md"),
    ),
    (
        ".ai/alpha/.agents/skills/alpha-check/reference.md",
        include_bytes!("../assets/fixture/.ai/alpha/.agents/skills/alpha-check/reference.md"),
    ),
    (
        ".ai/beta/AGENTS.md",
        include_bytes!("../assets/fixture/.ai/beta/AGENTS.md"),
    ),
    (
        ".ai/beta/docs/nested/notes.md",
        include_bytes!("../assets/fixture/.ai/beta/docs/nested/notes.md"),
    ),
    (
        ".ai/beta/.agents/skills/beta-check/SKILL.md",
        include_bytes!("../assets/fixture/.ai/beta/.agents/skills/beta-check/SKILL.md"),
    ),
    (
        ".ai/beta/.agents/skills/beta-check/reference.md",
        include_bytes!("../assets/fixture/.ai/beta/.agents/skills/beta-check/reference.md"),
    ),
];

fn directories(path: &Path) -> io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o755).create(path)
}

fn write_new(path: &Path, data: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(path)?;
    let written = file.write_all(data);
    finish_io(written, close_native(file))
}

fn initialize_git(target: &Path, args: &[OsString], cancel: &Cancellation) -> Result<()> {
    let mut command = git::command(target);
    command.args([
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "commit.gpgsign=false",
        "-c",
        "user.name=Workspace Fixture",
        "-c",
        "user.email=fixture@example.invalid",
    ]);
    command.args(args).env_clear().envs(
        std::env::vars_os().filter(|(name, _)| !name.as_encoded_bytes().starts_with(b"GIT_")),
    );
    subprocess::output(&mut command, cancel)
        .map(|_| ())
        .map_err(|error| error.context(format!("initialize fixture repository {target:?}")))
}

/// Create a verification workspace, returning its absolute configuration path.
///
/// A complete existing fixture is validated and reused without writing files.
/// Existing unowned or incomplete destinations are refused. Failed creation
/// retains its partial files without a completion marker for explicit recovery.
/// Git runs with inherited `GIT_` overrides removed and observes cancellation.
pub fn create(directory: &Path, cancel: &Cancellation) -> Result<PathBuf> {
    let root = if directory.is_absolute() {
        directory.clean()
    } else {
        std::env::current_dir()?.join(directory).clean()
    };
    let config = root.join(NAME);
    let marker = root.join(MARKER);
    if let Some(parent) = root.parent() {
        directories(parent)?;
    }
    match DirBuilder::new().mode(0o755).create(&root) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            if fs::read(&marker).as_deref().ok() != Some(COMPLETE) {
                return Err(Error::Invalid(format!(
                    "refusing to overwrite existing or incomplete fixture directory: {root:?}"
                )));
            }
            Configuration::load(&config)?;
            return Ok(config);
        }
        Err(error) => return Err(error.into()),
    }
    for (name, data) in FILES {
        let target = root.join(name);
        if let Some(parent) = target.parent() {
            directories(parent)?;
        }
        write_new(&target, data)?;
    }
    for project in ["alpha", "beta"] {
        let target = root.join(project);
        for args in [
            vec!["init".into(), "--initial-branch=main".into()],
            vec!["add".into(), ".".into()],
            vec![
                "commit".into(),
                "-m".into(),
                "Initialize verification fixture".into(),
            ],
            vec![
                "worktree".into(),
                "add".into(),
                "-b".into(),
                "verification".into(),
                root.join(".workspaces/one").join(project).into_os_string(),
                "main".into(),
            ],
        ] {
            initialize_git(&target, &args, cancel)?;
        }
    }
    write_new(&marker, COMPLETE)?;
    Ok(config)
}
