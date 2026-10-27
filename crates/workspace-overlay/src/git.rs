//! Git invocations bound to a checkout's own metadata, with native byte paths.
use crate::{Cancellation, Error, Result, subprocess};
use path_clean::PathClean;
use std::{
    collections::HashSet,
    ffi::OsString,
    fs, io,
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

/// Bind Git to `target/.git`; a missing or damaged file cannot select an ancestor.
pub fn command(target: &Path) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(target);
    let mut directory = OsString::from("--git-dir=");
    directory.push(target.join(".git"));
    command.arg(directory);
    command
}

pub(crate) fn has_metadata(target: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(target.join(".git")) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Parse Git's NUL-delimited porcelain output, retaining spaces and newlines.
fn parse_worktrees(target: &Path, data: &[u8]) -> Vec<PathBuf> {
    let mut targets = vec![target.to_owned()];
    let mut seen = HashSet::from([target.clean()]);
    let mut name = None;
    let mut unavailable = false;
    let finish = |name: &mut Option<PathBuf>,
                  unavailable: &mut bool,
                  targets: &mut Vec<PathBuf>,
                  seen: &mut HashSet<PathBuf>| {
        if let Some(path) = name.take()
            && !*unavailable
            && seen.insert(path.clean())
        {
            targets.push(path);
        }
        *unavailable = false;
    };
    for field in data.split(|byte| *byte == 0) {
        if field.is_empty() {
            finish(&mut name, &mut unavailable, &mut targets, &mut seen);
        } else if let Some(path) = field.strip_prefix(b"worktree ") {
            name = Some(OsString::from_vec(path.to_vec()).into());
        } else if field == b"bare"
            || field == b"prunable"
            || field.starts_with(b"prunable ")
            || field == b"locked initializing"
        {
            unavailable = true;
        }
    }
    finish(&mut name, &mut unavailable, &mut targets, &mut seen);
    targets
}

/// Discover worktrees, retrying Git's transient incomplete-registration errors.
pub fn worktrees(target: &Path, include: bool, cancel: &Cancellation) -> Result<Vec<PathBuf>> {
    cancel.check()?;
    if !include || !has_metadata(target)? {
        return Ok(vec![target.to_owned()]);
    }
    for attempt in 0..4 {
        match list_worktrees(target, cancel) {
            Ok(paths) => return Ok(paths),
            Err(error @ Error::Command { .. }) if attempt < 3 => {
                log::warn!("discover worktrees: {error}; listing again");
                cancel.wait(Duration::from_millis(100 << attempt));
                cancel.check()?;
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("fourth attempt returns its result")
}

pub(crate) fn list_worktrees(target: &Path, cancel: &Cancellation) -> Result<Vec<PathBuf>> {
    let mut command = command(target);
    command.args(["worktree", "list", "--porcelain", "-z"]);
    Ok(parse_worktrees(
        target,
        &subprocess::output(&mut command, cancel)?,
    ))
}

/// The child accesses its parent's retained backing descriptor, never a served
/// pathname in the parent. `/proc/self/fd` would refer to the child's closed fd.
pub(crate) fn backing_worktrees(
    target: &Path,
    backing: &cap_std::fs::Dir,
    cancel: &Cancellation,
) -> Result<Vec<PathBuf>> {
    use std::os::fd::AsRawFd;
    let native = PathBuf::from(format!(
        "/proc/{}/fd/{}",
        std::process::id(),
        backing.as_raw_fd()
    ));
    let mut command = command(&native);
    command.args(["worktree", "list", "--porcelain", "-z"]);
    Ok(parse_worktrees(
        target,
        &subprocess::output(&mut command, cancel)?,
    ))
}

pub(crate) fn metadata_path(
    target: &Path,
    args: &[&str],
    cancel: &Cancellation,
) -> Result<PathBuf> {
    let mut command = command(target);
    command.arg("rev-parse").args(args);
    let mut data = subprocess::output(&mut command, cancel)?;
    // Git adds one record separator. Other whitespace belongs to the path.
    if data.last() == Some(&b'\n') {
        data.pop();
    }
    let path = PathBuf::from(OsString::from_vec(data));
    if !path.is_absolute() {
        return Err(Error::Invalid(
            "Git returned a non-absolute metadata path".into(),
        ));
    }
    Ok(path)
}

/// Read a linked worktree's registration name, when its metadata identifies one.
pub fn registration(target: &Path, cancel: &Cancellation) -> Result<Option<OsString>> {
    let path = metadata_path(target, &["--absolute-git-dir"], cancel)?;
    Ok(path
        .parent()
        .filter(|parent| parent.file_name() == Some(std::ffi::OsStr::new("worktrees")))
        .and_then(|_| path.file_name())
        .map(OsStrExt::as_bytes)
        .map(|bytes| OsString::from_vec(bytes.into())))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn porcelain_ignores_unavailable_records_without_trimming_paths() {
        let paths=parse_worktrees(Path::new("/primary"),b"worktree /primary\0\0worktree /space \n\0HEAD aa\0\0worktree /skip\0locked initializing\0\0worktree /bare\0bare\0\0worktree /gone\0prunable missing\0\0worktree /space \n\0\0");
        assert_eq!(
            paths,
            vec![PathBuf::from("/primary"), PathBuf::from("/space \n")]
        );
    }

    #[test]
    fn retained_backing_lists_own_project_without_visiting_the_live_mount() {
        use std::sync::Arc;
        fs::create_dir_all(".tmp").unwrap();
        let temp = tempfile::tempdir_in(".tmp").unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let project = root.join("project");
        fs::create_dir(&project).unwrap();
        let output = Command::new("git")
            .args(["init", "--initial-branch=main"])
            .arg(&project)
            .output()
            .unwrap();
        assert!(output.status.success());
        let view = Arc::new(
            crate::view::View::open(&crate::config::Selection {
                root,
                project: "project".into(),
                target: project.clone(),
                markers: vec![],
                sources: vec![],
            })
            .unwrap(),
        );
        let mut mounted = view.mount().unwrap();
        let targets =
            backing_worktrees(&project, &view.layers[0].root, &Cancellation::default()).unwrap();
        assert_eq!(targets, vec![project]);
        mounted.unmount().unwrap();
    }
}
