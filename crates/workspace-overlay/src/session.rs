//! Owned FUSE sessions, worktree reconciliation, and durable cleanup.
use crate::{
    Cancellation, Error, Mounted, Result,
    config::Selection,
    git,
    gitexclude::Exclude,
    management,
    mounts::{self, Confirm},
    notifications::Notifications,
    registry::{MountRecord, Owner, Store},
    view::View,
};
use parking_lot::Mutex;
use std::{
    collections::{BTreeMap, HashSet},
    ffi::{OsStr, OsString},
    fs, io,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

/// Invocation policy. Worktrees are included by default.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub replace: bool,
    pub worktrees: bool,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            replace: false,
            worktrees: true,
        }
    }
}

/// Diagnostics emitted by the owning controller. Callback failures stop the session.
#[derive(Debug)]
pub enum Event {
    Mounted {
        target: PathBuf,
        sources: Vec<PathBuf>,
    },
    Unmounted {
        target: PathBuf,
    },
    Skipped {
        target: PathBuf,
        reason: String,
    },
    Warning(Error),
}
type Emit<'a> = dyn FnMut(Event) -> Result<()> + 'a;
struct Running {
    view: Arc<View>,
    mounted: Option<Mounted>,
    registration: Option<OsString>,
}
struct Project {
    selection: Selection,
    owner: Arc<Mutex<Owner>>,
    active: BTreeMap<PathBuf, Running>,
    suppressed: BTreeMap<PathBuf, Option<OsString>>,
    template: Option<Exclude>,
    primary: Option<Arc<View>>,
    notify: Notifications,
    cancel: Cancellation,
    worktrees: bool,
    changed: Option<Instant>,
    fallback: Instant,
    failures: u32,
}

fn overlap(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}
fn unsupported(target: &Path, accepted: &[PathBuf], sources: &[PathBuf]) -> Option<String> {
    if let Some(existing) = accepted.iter().find(|existing| overlap(existing, target)) {
        return Some(format!("overlaps mount target {}", existing.display()));
    }
    sources
        .iter()
        .find(|source| source.starts_with(target))
        .map(|source| format!("contains overlay source {}", source.display()))
}

fn preflight(
    selections: &[Selection],
    options: Options,
    store: &Store,
    cancel: &Cancellation,
    confirm: Option<&Confirm<'_>>,
    emit: &mut Emit<'_>,
) -> Result<Vec<(Vec<PathBuf>, Vec<PathBuf>)>> {
    if options.replace {
        for selection in selections {
            management::stop_registered(
                store,
                &selection.root,
                &selection.project,
                confirm,
                cancel,
            )?;
            mounts::unmount(&selection.target, confirm, cancel)?;
        }
    }
    let mut sources = Vec::new();
    for selection in selections {
        for source in &selection.sources {
            sources.push(fs::canonicalize(&source.path)?);
        }
    }
    let mut accepted = Vec::new();
    let mut lists = Vec::new();
    for selection in selections {
        if let Some(kind) = mounts::mounted_type(&selection.target, cancel)? {
            return Err(Error::Invalid(format!(
                "{} is already mounted as {}; use --replace for an overlay",
                selection.target.display(),
                kind.to_string_lossy()
            )));
        }
        let target = fs::canonicalize(&selection.target)?;
        if let Some(reason) = unsupported(&target, &accepted, &sources) {
            return Err(Error::Invalid(format!(
                "mount target {} {reason}",
                target.display()
            )));
        }
        accepted.push(target);
        lists.push(git::worktrees(
            &selection.target,
            options.worktrees,
            cancel,
        )?);
    }
    let mut result = Vec::new();
    for targets in lists {
        let mut selected = vec![targets[0].clone()];
        let mut suppressed = Vec::new();
        for target in &targets[1..] {
            if options.replace {
                mounts::unmount(target, confirm, cancel)?;
            }
            let reason = if mounts::holding_type(target, cancel)?.as_deref()
                == Some(OsStr::new(mounts::FILESYSTEM_TYPE))
            {
                Some("overlaps an active overlay mount".into())
            } else if let Some(kind) = mounts::mounted_type(target, cancel)? {
                Some(format!("already mounted as {}", kind.to_string_lossy()))
            } else {
                let resolved = fs::canonicalize(target)?;
                let reason = unsupported(&resolved, &accepted, &sources);
                if reason.is_none() {
                    accepted.push(resolved);
                }
                reason
            };
            if let Some(reason) = reason {
                emit(Event::Skipped {
                    target: target.clone(),
                    reason,
                })?;
                suppressed.push(target.clone());
            } else {
                selected.push(target.clone());
            }
        }
        result.push((selected, suppressed));
    }
    Ok(result)
}

impl Project {
    fn prepare(
        selection: Selection,
        targets: Vec<PathBuf>,
        suppressed: Vec<PathBuf>,
        options: Options,
        store: &Store,
        cancel: &Cancellation,
    ) -> Result<Self> {
        let owner = Arc::new(Mutex::new(store.open(&selection.root, &selection.project)?));
        let notify = match Notifications::new() {
            Ok(notify) => notify,
            Err(error) => {
                let _ = owner.lock().close();
                return Err(error);
            }
        };
        let mut project = Self {
            selection,
            owner,
            active: BTreeMap::new(),
            suppressed: BTreeMap::new(),
            template: None,
            primary: None,
            notify,
            cancel: cancel.child(),
            worktrees: options.worktrees,
            changed: None,
            fallback: Instant::now() + Duration::from_secs(30),
            failures: 0,
        };
        let prepared = (|| {
            project.template = Exclude::open(&project.selection.target, &project.cancel)?;
            for target in suppressed {
                project.suppressed.insert(
                    target.clone(),
                    git::registration(&target, &project.cancel).ok().flatten(),
                );
            }
            for target in targets {
                let running = project.prepare_target(&target)?;
                if target == project.selection.target {
                    project.primary = Some(running.view.clone());
                }
                project.active.insert(target, running);
            }
            let primary = project
                .primary
                .as_ref()
                .ok_or_else(|| Error::Invalid("project has no primary mount".into()))?;
            project.notify.add_sources(primary)?;
            if project.worktrees && git::has_metadata(&project.selection.target)? {
                let common = git::metadata_path(
                    &project.selection.target,
                    &["--path-format=absolute", "--git-common-dir"],
                    &project.cancel,
                )?;
                project.notify.add_git(&common)?;
            }
            project.notify.set_control(
                store.directory(),
                project.owner.lock().stop_file(),
                project.cancel.clone(),
            );
            project.notify.sync()
        })();
        if let Err(error) = prepared {
            let cleanup = project.cleanup(None, &mut |_| Ok(()));
            return crate::failures(std::iter::once(error).chain(cleanup.err()).collect())
                .map(|_| project);
        }
        Ok(project)
    }
    fn prepare_target(&self, target: &Path) -> Result<Running> {
        let mut selection = self.selection.clone();
        selection.target = target.into();
        let view = Arc::new(View::open(&selection)?);
        let exclude = self.template.as_ref().map(Exclude::fork).transpose()?;
        let path = exclude.as_ref().map(|exclude| exclude.path().to_owned());
        let target_owned = target.to_owned();
        let owner = self.owner.clone();
        let prepared = (|| {
            owner.lock().record(MountRecord {
                target: target_owned.clone(),
                exclude: path.clone(),
                block: vec![],
            })?;
            if let Some(exclude) = exclude {
                let target = target_owned.clone();
                view.attach_exclude(exclude, move |block| {
                    owner.lock().record(MountRecord {
                        target: target.clone(),
                        exclude: path.clone(),
                        block: block.into(),
                    })
                })?;
            }
            Ok(())
        })();
        if let Err(error) = prepared {
            let mut errors = vec![error];
            match view.close_exclude() {
                Ok(()) => {
                    if let Err(error) = self.owner.lock().forget(target) {
                        errors.push(error);
                    }
                }
                Err(error) => errors.push(error),
            }
            return crate::failures(errors).map(|_| unreachable!());
        }
        let registration = if target == self.selection.target {
            None
        } else {
            git::registration(target, &self.cancel).ok().flatten()
        };
        Ok(Running {
            view,
            mounted: None,
            registration,
        })
    }
    fn start(&mut self, target: &Path, emit: &mut Emit<'_>) -> Result<()> {
        let running = self
            .active
            .get_mut(target)
            .ok_or_else(|| Error::Invalid("prepared mount missing".into()))?;
        running.mounted = Some(running.view.mount()?);
        emit(Event::Mounted {
            target: target.into(),
            sources: self
                .selection
                .sources
                .iter()
                .map(|source| source.path.clone())
                .collect(),
        })
    }
    fn finish(
        &mut self,
        target: &Path,
        confirm: Option<&Confirm<'_>>,
        emit: &mut Emit<'_>,
    ) -> Result<()> {
        let Some(mut running) = self.active.remove(target) else {
            return Ok(());
        };
        let mounted = running.mounted.is_some();
        if let Some(server) = running.mounted.as_mut() {
            if server.is_finished() {
                server.wait()?;
            } else {
                mounts::release(target, confirm, &Cancellation::default(), || {
                    Ok(server.unmount()?)
                })?;
            }
        }
        // A failed unmount returns above: recovery state and Git blocks survive.
        running.view.close_exclude()?;
        self.owner.lock().forget(target)?;
        if mounted {
            emit(Event::Unmounted {
                target: target.into(),
            })?;
        }
        Ok(())
    }
    fn cleanup(&mut self, confirm: Option<&Confirm<'_>>, emit: &mut Emit<'_>) -> Result<()> {
        self.cancel.cancel();
        let targets: Vec<_> = self.active.keys().cloned().collect();
        let mut errors = Vec::new();
        for target in targets {
            if let Err(error) = self.finish(&target, confirm, emit) {
                errors.push(error);
            }
        }
        if let Err(error) = self.owner.lock().close() {
            errors.push(error);
        }
        crate::failures(errors)
    }
    fn targets(&self) -> Result<Vec<PathBuf>> {
        let primary = self
            .primary
            .as_ref()
            .ok_or_else(|| Error::Invalid("primary backing is unavailable".into()))?;
        if !self.worktrees {
            return Ok(vec![self.selection.target.clone()]);
        }
        match primary.layers[0].root.symlink_metadata(".git") {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(vec![self.selection.target.clone()]);
            }
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }
        git::backing_worktrees(
            &self.selection.target,
            &primary.layers[0].root,
            &self.cancel,
        )
    }
    fn reconcile(
        &mut self,
        cancel: &Cancellation,
        confirm: Option<&Confirm<'_>>,
        emit: &mut Emit<'_>,
    ) -> Result<()> {
        let targets = self.targets()?;
        let desired: HashSet<_> = targets.iter().cloned().collect();
        self.suppressed.retain(|target, _| desired.contains(target));
        let removed: Vec<_> = self
            .active
            .keys()
            .filter(|target| !desired.contains(*target))
            .cloned()
            .collect();
        for target in removed {
            self.finish(&target, confirm, emit)?;
        }
        for running in self.active.values() {
            running.view.refresh_paths()?;
        }
        let mut accepted = mounts::overlay_targets(cancel)?;
        let sources: Vec<_> = self
            .selection
            .sources
            .iter()
            .map(|source| source.path.clone())
            .collect();
        for target in targets {
            if self.active.contains_key(&target) || self.suppressed.contains_key(&target) {
                continue;
            }
            let Some(kind) = mounts::holding_type(&target, &self.cancel)? else {
                continue;
            };
            let reason = if kind == OsStr::new(mounts::FILESYSTEM_TYPE) {
                Some("overlaps an active overlay mount".into())
            } else {
                let resolved = match fs::canonicalize(&target) {
                    Ok(path) => path,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error.into()),
                };
                if let Some(reason) = unsupported(&resolved, &accepted, &sources) {
                    Some(reason)
                } else {
                    mounts::mounted_type(&target, &self.cancel)?
                        .map(|kind| format!("already mounted as {}", kind.to_string_lossy()))
                }
            };
            if let Some(reason) = reason {
                self.suppressed.insert(
                    target.clone(),
                    git::registration(&target, &self.cancel).ok().flatten(),
                );
                emit(Event::Skipped { target, reason })?;
                continue;
            }
            let running = match self.prepare_target(&target) {
                Ok(running) => running,
                Err(Error::Io(error)) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            self.active.insert(target.clone(), running);
            self.start(&target, emit)?;
            accepted.push(target);
        }
        self.notify.sync()
    }
    fn tick(
        &mut self,
        cancel: &Cancellation,
        confirm: Option<&Confirm<'_>>,
        emit: &mut Emit<'_>,
    ) -> Result<bool> {
        if self.cancel.is_cancelled() {
            self.cleanup(confirm, emit)?;
            return Ok(false);
        }
        for running in self.active.values() {
            if let Some(warning) = running.view.take_cache_warning() {
                emit(Event::Warning(warning))?;
            }
        }
        let finished: Vec<_> = self
            .active
            .iter()
            .filter(|(_, running)| running.mounted.as_ref().is_some_and(Mounted::is_finished))
            .map(|(target, running)| (target.clone(), running.registration.clone()))
            .collect();
        for (target, registration) in finished {
            self.finish(&target, confirm, emit)?;
            if target == self.selection.target {
                self.cleanup(confirm, emit)?;
                return Ok(false);
            }
            self.suppressed.insert(target, registration);
        }
        while let Some(event) = self.notify.receive(Duration::ZERO)? {
            match event {
                Ok(event) => {
                    for name in self.notify.unregistered(&event) {
                        self.suppressed.retain(|_, registration| {
                            registration
                                .as_ref()
                                .is_none_or(|registered| !name.is_empty() && registered != &name)
                        });
                    }
                    if self.notify.relevant(&event) {
                        self.notify.sync()?;
                        self.changed = Some(Instant::now() + Duration::from_millis(200));
                    }
                }
                Err(error) => {
                    emit(Event::Warning(error.into()))?;
                    self.changed = Some(Instant::now());
                }
            }
        }
        let now = Instant::now();
        if now >= self.fallback || self.changed.is_some_and(|deadline| now >= deadline) {
            self.changed = None;
            self.fallback = now + Duration::from_secs(30);
            match self.reconcile(cancel, confirm, emit) {
                Err(error @ Error::Command { .. }) if self.failures < 3 => {
                    self.changed = Some(now + Duration::from_millis(100 << self.failures));
                    self.failures += 1;
                    emit(Event::Warning(error))?;
                }
                result => {
                    self.failures = 0;
                    result?;
                }
            }
        }
        Ok(true)
    }
}

/// Serve selected projects until cancelled, externally unmounted, or failed.
/// Backing directories, exclusion files, and Git metadata are opened before
/// the first mount. Cleanup uses uncancelled operations and retains recovery
/// records whenever unmount or exclusion restoration fails.
pub fn serve(
    selections: Vec<Selection>,
    options: Options,
    store: Store,
    cancel: &Cancellation,
    confirm: Option<&Confirm<'_>>,
    emit: &mut Emit<'_>,
) -> Result<()> {
    cancel.check()?;
    let lists = preflight(&selections, options, &store, cancel, confirm, emit)?;
    let mut projects = Vec::new();
    let result = (|| {
        for (selection, (targets, suppressed)) in selections.into_iter().zip(lists) {
            projects.push(Project::prepare(
                selection, targets, suppressed, options, &store, cancel,
            )?);
        }
        for project in &mut projects {
            let targets: Vec<_> = project.active.keys().cloned().collect();
            for target in targets {
                project.start(&target, emit)?;
            }
        }
        loop {
            if cancel.is_cancelled() {
                return Ok(());
            }
            let mut live = false;
            for project in &mut projects {
                if !project.active.is_empty() {
                    live |= project.tick(cancel, confirm, emit)?;
                }
            }
            if !live {
                return Ok(());
            }
            cancel.wait(Duration::from_millis(10));
        }
    })();
    let mut errors = Vec::new();
    if let Err(error) = result
        && !matches!(error, Error::Cancelled)
    {
        errors.push(error);
    }
    for project in &mut projects {
        if let Err(error) = project.cleanup(confirm, &mut |_| Ok(())) {
            errors.push(error);
        }
    }
    crate::failures(errors)
}
