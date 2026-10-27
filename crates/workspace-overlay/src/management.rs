//! Status, live-owner stop requests, and abandoned-session recovery.
use crate::{
    Cancellation, Result,
    config::Selection,
    git,
    gitexclude::Exclude,
    mounts::{self, Confirm},
    registry::Store,
};
use std::{
    collections::HashSet,
    ffi::OsString,
    path::{Path, PathBuf},
};

#[derive(Debug)]
pub struct Status {
    pub target: PathBuf,
    pub filesystem: Option<OsString>,
}

fn targets(
    store: &Store,
    selection: &Selection,
    worktrees: bool,
    cancel: &Cancellation,
) -> Result<Vec<PathBuf>> {
    let state = store.read(&selection.root, &selection.project)?;
    let mut targets = match git::worktrees(&selection.target, worktrees, cancel) {
        Err(crate::Error::Cancelled) => return Err(crate::Error::Cancelled),
        Ok(targets) => targets,
        Err(_) if state.as_ref().is_some_and(|state| !state.mounts.is_empty()) => vec![],
        Err(error) => return Err(error),
    };
    let mut seen: HashSet<_> = targets.iter().cloned().collect();
    if let Some(state) = state {
        for mount in state.mounts {
            if seen.insert(mount.target.clone()) {
                targets.push(mount.target);
            }
        }
    }
    Ok(targets)
}
/// Inspect registered and currently discoverable targets. No served pathname is opened here.
pub fn status(
    store: &Store,
    selection: &Selection,
    worktrees: bool,
    cancel: &Cancellation,
) -> Result<Vec<Status>> {
    targets(store, selection, worktrees, cancel)?
        .into_iter()
        .map(|target| {
            Ok(Status {
                filesystem: mounts::mounted_type(&target, cancel)?,
                target,
            })
        })
        .collect()
}
/// Recover an abandoned owner. All mounts must be released before restoring any Git blocks.
pub fn recover(
    store: &Store,
    root: &Path,
    project: &str,
    confirm: Option<&Confirm<'_>>,
    cancel: &Cancellation,
) -> Result<()> {
    let mut owner = store.acquire(root, project)?;
    let Some(state) = store.read(root, project)? else {
        owner.close()?;
        return Ok(());
    };
    owner.adopt(state.clone())?;
    let mut errors = Vec::new();
    for mount in &state.mounts {
        if let Err(error) = mounts::unmount(&mount.target, confirm, cancel) {
            errors.push(error);
        }
    }
    if errors.is_empty() {
        for mount in &state.mounts {
            let restored = if let Some(path) = &mount.exclude {
                Exclude::restore(path, &mount.block)
            } else {
                Ok(())
            };
            match restored {
                Ok(()) => {
                    if let Err(error) = owner.forget(&mount.target) {
                        errors.push(error);
                    }
                }
                Err(error) => errors.push(error.context("recover Git exclusions")),
            }
        }
    }
    if errors.is_empty() {
        if let Err(error) = owner.close() {
            errors.push(error);
        }
    } else if let Err(error) = owner.unlock() {
        errors.push(error.into());
    }
    crate::failures(errors)
}
/// Ask a live owner to stop, or recover its abandoned registry.
pub fn stop_registered(
    store: &Store,
    root: &Path,
    project: &str,
    confirm: Option<&Confirm<'_>>,
    cancel: &Cancellation,
) -> Result<()> {
    if store.request_stop(root, project, cancel)? {
        return Ok(());
    }
    recover(store, root, project, confirm, cancel)
}
/// Stop this configured project and release every known target, retaining independent failures.
pub fn unmount(
    store: &Store,
    selection: &Selection,
    worktrees: bool,
    confirm: Option<&Confirm<'_>>,
    cancel: &Cancellation,
) -> Result<()> {
    // Snapshot recovery records before the owner removes them during its clean stop.
    let targets = targets(store, selection, worktrees, cancel)?;
    stop_registered(store, &selection.root, &selection.project, confirm, cancel)?;
    let errors = targets
        .into_iter()
        .filter_map(|target| mounts::unmount(&target, confirm, cancel).err())
        .collect();
    crate::failures(errors)
}
