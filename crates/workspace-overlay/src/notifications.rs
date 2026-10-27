//! Directory notifications through capabilities retained before any mount.
use crate::{Error, Result, view::View};
use cap_std::fs::{Dir, MetadataExt};
use notify::{
    Event, EventKind, RecursiveMode, Watcher,
    event::{MetadataKind, ModifyKind, RenameMode},
};
use std::{
    collections::HashSet,
    ffi::OsString,
    io,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, TrySendError},
    },
    time::Duration,
};

struct Root {
    directory: Dir,
    git: bool,
}
impl Root {
    fn path(&self) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}", self.directory.as_raw_fd()))
    }
}
pub(crate) struct Notifications {
    watcher: Option<notify::RecommendedWatcher>,
    events: Receiver<notify::Result<Event>>,
    overflow: Arc<AtomicBool>,
    roots: Vec<Root>,
    identities: HashSet<(u64, u64, bool)>,
    watched: HashSet<PathBuf>,
    control: Option<PathBuf>,
    stop: Arc<parking_lot::RwLock<Option<(PathBuf, crate::Cancellation)>>>,
}
impl Notifications {
    pub(crate) fn new() -> Result<Self> {
        let (send, events) = mpsc::sync_channel(1024);
        let overflow = Arc::new(AtomicBool::new(false));
        let dropped = overflow.clone();
        let stop = Arc::new(parking_lot::RwLock::new(
            None::<(PathBuf, crate::Cancellation)>,
        ));
        let requested = stop.clone();
        let watcher = notify::recommended_watcher(move |event: notify::Result<Event>| {
            if let Ok(event) = &event
                && matches!(event.kind, EventKind::Create(_) | EventKind::Modify(_))
                && let Some((path, cancel)) = &*requested.read()
                && event.paths.contains(path)
            {
                cancel.cancel();
            }
            match send.try_send(event) {
                Ok(()) | Err(TrySendError::Disconnected(_)) => {}
                Err(TrySendError::Full(_)) => {
                    dropped.store(true, Ordering::Release);
                }
            }
        })?;
        Ok(Self {
            watcher: Some(watcher),
            events,
            overflow,
            roots: vec![],
            identities: HashSet::new(),
            watched: HashSet::new(),
            control: None,
            stop,
        })
    }
    fn add(&mut self, directory: Dir, git: bool) -> io::Result<()> {
        let metadata = directory.dir_metadata()?;
        if self
            .identities
            .insert((metadata.dev(), metadata.ino(), git))
        {
            self.roots.push(Root { directory, git });
        }
        Ok(())
    }
    pub(crate) fn add_sources(&mut self, view: &View) -> io::Result<()> {
        for layer in &view.layers[1..] {
            self.add(layer.root.try_clone()?, false)?;
        }
        Ok(())
    }
    pub(crate) fn add_git(&mut self, path: &Path) -> io::Result<()> {
        self.add(
            Dir::open_ambient_dir(path, cap_std::ambient_authority())?,
            true,
        )
    }
    pub(crate) fn set_control(&mut self, path: &Path, stop: &Path, cancel: crate::Cancellation) {
        self.control = Some(path.into());
        *self.stop.write() = Some((stop.into(), cancel));
    }
    pub(crate) fn git_roots(&self) -> impl Iterator<Item = PathBuf> + '_ {
        self.roots.iter().filter(|root| root.git).map(Root::path)
    }
    pub(crate) fn sync(&mut self) -> Result<()> {
        let mut desired = HashSet::new();
        if let Some(control) = &self.control {
            desired.insert(control.clone());
        }
        for root in &self.roots {
            let base = root.path();
            desired.insert(base.clone());
            if root.git {
                match root.directory.read_dir("worktrees") {
                    Ok(entries) => {
                        desired.insert(base.join("worktrees"));
                        for entry in entries {
                            let entry = entry?;
                            if entry.file_type()?.is_dir() {
                                desired.insert(base.join("worktrees").join(entry.file_name()));
                            }
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
                continue;
            }
            let mut pending = vec![PathBuf::from(".")];
            while let Some(relative) = pending.pop() {
                desired.insert(if relative == Path::new(".") {
                    base.clone()
                } else {
                    base.join(&relative)
                });
                for entry in root.directory.read_dir(&relative)? {
                    let entry = entry?;
                    if entry.file_name() != std::ffi::OsStr::new(".git")
                        && entry.file_type()?.is_dir()
                    {
                        pending.push(relative.join(entry.file_name()));
                    }
                }
            }
        }
        let watcher = self
            .watcher
            .as_mut()
            .ok_or_else(|| Error::Invalid("filesystem watcher is closed".into()))?;
        for removed in self.watched.difference(&desired) {
            match watcher.unwatch(removed) {
                Ok(()) => {}
                Err(error)
                    if matches!(
                        error.kind,
                        notify::ErrorKind::WatchNotFound | notify::ErrorKind::PathNotFound
                    ) => {}
                Err(error) if matches!(&error.kind,notify::ErrorKind::Io(error) if error.raw_os_error()==Some(libc::EINVAL)) =>
                    {}
                Err(error) => return Err(error.into()),
            }
        }
        for added in desired.difference(&self.watched) {
            match watcher.watch(added, RecursiveMode::NonRecursive) {
                Ok(()) => {}
                Err(error) if matches!(error.kind, notify::ErrorKind::PathNotFound) => {}
                Err(error) => return Err(error.into()),
            }
        }
        self.watched = desired;
        Ok(())
    }
    pub(crate) fn relevant(&self, event: &Event) -> bool {
        if matches!(
            event.kind,
            EventKind::Access(_)
                | EventKind::Modify(ModifyKind::Metadata(MetadataKind::Permissions))
        ) {
            return false;
        }
        event.paths.iter().any(|path| {
            self.roots.iter().any(|root| {
                let base = root.path();
                let Ok(relative) = path.strip_prefix(&base) else {
                    return false;
                };
                !root.git
                    || relative == Path::new("worktrees")
                    || relative.parent() == Some(Path::new("worktrees"))
                    || matches!(
                        relative.file_name().and_then(|name| name.to_str()),
                        Some("gitdir" | "locked")
                    )
            })
        })
    }
    pub(crate) fn unregistered(&self, event: &Event) -> Vec<OsString> {
        if !matches!(
            event.kind,
            EventKind::Remove(_)
                | EventKind::Modify(ModifyKind::Name(
                    RenameMode::From | RenameMode::Both | RenameMode::Any
                ))
        ) {
            return vec![];
        }
        let mut names = Vec::new();
        for path in &event.paths {
            for base in self.git_roots() {
                if let Ok(relative) = path.strip_prefix(&base) {
                    if relative == Path::new("worktrees") {
                        names.push(OsString::new());
                    } else if relative.parent() == Some(Path::new("worktrees"))
                        && let Some(name) = relative.file_name()
                    {
                        names.push(name.to_owned());
                    }
                }
            }
        }
        names
    }
    pub(crate) fn receive(&self, timeout: Duration) -> Result<Option<notify::Result<Event>>> {
        if self.overflow.swap(false, Ordering::AcqRel) {
            return Ok(Some(Err(notify::Error::generic(
                "notification queue overflow; reconcile backing state",
            ))));
        }
        match self.events.recv_timeout(timeout) {
            Ok(event) => Ok(Some(event)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(Error::Invalid(
                "filesystem watcher closed unexpectedly".into(),
            )),
        }
    }
    fn shutdown(&mut self) {
        // notify's native Drop schedules shutdown. The sole sender lives in its
        // event handler: channel disconnect proves that worker released it.
        if self.watcher.take().is_some() {
            while self.events.recv().is_ok() {}
        }
    }
}
impl Drop for Notifications {
    fn drop(&mut self) {
        self.shutdown();
    }
}
