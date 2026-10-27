//! Bounded native backing watches for caller-invariant kernel metadata.
use crate::{Error, Result, view::View};
use cap_std::fs::{Dir, MetadataExt};
use fuser::{INodeNo, Notifier};
use notify::{Event, EventKind, RecursiveMode, Watcher, event::ModifyKind};
use parking_lot::{Mutex, RwLock};
use std::{
    collections::{HashMap, HashSet},
    ffi::{OsStr, OsString},
    io,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const TTL: Duration = Duration::from_secs(1);
const WATCH_LIMIT: usize = 256;
const ENTRY_LIMIT: usize = 1024;
const QUEUE_LIMIT: usize = 64;
pub(crate) struct Entry {
    pub(crate) expires: Instant,
    child: Option<u64>,
}
pub(crate) struct Directory {
    _backing: Dir,
    parents: HashSet<u64>,
    dead: bool,
    pub(crate) entries: HashMap<OsString, Entry>,
}
#[derive(Default)]
pub(crate) struct State {
    pub(crate) directories: HashMap<(u64, u64), Directory>,
    parents: HashMap<u64, (u64, u64)>,
    paths: HashMap<PathBuf, (u64, u64)>,
}
pub(crate) struct Shared {
    pub(crate) state: Mutex<State>,
    notifier: RwLock<Option<Notifier>>,
    view: Weak<View>,
    failed: AtomicBool,
    flush: AtomicBool,
    warning: Mutex<Option<Error>>,
}
enum Message {
    Event(notify::Result<Event>),
    Attribute(u64),
    Wake,
    Closed,
}
struct Handler {
    send: SyncSender<Message>,
    shared: Arc<Shared>,
}
impl Handler {
    fn event(&self, event: notify::Result<Event>) {
        if let Err(mpsc::TrySendError::Full(_)) = self.send.try_send(Message::Event(event)) {
            self.shared.disable(Error::Invalid(
                "metadata notification queue overflow".into(),
            ));
        }
    }
}
impl Drop for Handler {
    fn drop(&mut self) {
        let _ = self.send.send(Message::Closed);
    }
}
pub(crate) struct Cache {
    pub(crate) shared: Arc<Shared>,
    watcher: Mutex<Option<notify::RecommendedWatcher>>,
    send: SyncSender<Message>,
    worker: Mutex<Option<JoinHandle<()>>>,
}
impl Cache {
    pub(crate) fn new(view: Weak<View>) -> Result<Self> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            notifier: RwLock::new(None),
            view,
            failed: AtomicBool::new(false),
            flush: AtomicBool::new(false),
            warning: Mutex::new(None),
        });
        let (send, receive) = mpsc::sync_channel(QUEUE_LIMIT);
        let handler = Handler {
            send: send.clone(),
            shared: shared.clone(),
        };
        let watcher = notify::recommended_watcher(move |event| handler.event(event))?;
        let work = shared.clone();
        let worker = thread::Builder::new()
            .name("overlay-metadata".into())
            .spawn(move || work.run(receive))?;
        Ok(Self {
            shared,
            watcher: Mutex::new(Some(watcher)),
            send,
            worker: Mutex::new(Some(worker)),
        })
    }
    pub(crate) fn bind(&self, notifier: Notifier) {
        *self.shared.notifier.write() = Some(notifier);
    }
    pub(crate) fn remember(
        &self,
        view: &View,
        parent: u64,
        path: &Path,
        name: &OsStr,
        child: Option<u64>,
    ) -> Duration {
        if self.shared.failed.load(Ordering::Acquire) {
            return Duration::ZERO;
        }
        let mut state = self.shared.state.lock();
        let identity = if let Some(identity) = state.parents.get(&parent) {
            *identity
        } else {
            if state.parents.len() >= WATCH_LIMIT {
                return Duration::ZERO;
            }
            let backing = match view.layers[0].root.open_dir(path) {
                Ok(backing) => backing,
                Err(_) => return Duration::ZERO,
            };
            let info = match backing.dir_metadata() {
                Ok(info) => info,
                Err(_) => return Duration::ZERO,
            };
            let identity = (info.dev(), info.ino());
            if !state.directories.contains_key(&identity) {
                let native = PathBuf::from(format!("/proc/self/fd/{}", backing.as_raw_fd()));
                let mut watcher = self.watcher.lock();
                let Some(watcher) = watcher.as_mut() else {
                    return Duration::ZERO;
                };
                if watcher.watch(&native, RecursiveMode::NonRecursive).is_err() {
                    return Duration::ZERO;
                }
                state.paths.insert(native, identity);
                state.directories.insert(
                    identity,
                    Directory {
                        _backing: backing,
                        parents: HashSet::new(),
                        dead: false,
                        entries: HashMap::new(),
                    },
                );
            }
            state.parents.insert(parent, identity);
            state
                .directories
                .get_mut(&identity)
                .expect("registered native directory")
                .parents
                .insert(parent);
            identity
        };
        let directory = state
            .directories
            .get_mut(&identity)
            .expect("registered parent");
        if directory.dead {
            return Duration::ZERO;
        }
        if !directory.entries.contains_key(name) && directory.entries.len() >= ENTRY_LIMIT {
            let now = Instant::now();
            directory.entries.retain(|_, entry| entry.expires > now);
            if directory.entries.len() >= ENTRY_LIMIT {
                return Duration::ZERO;
            }
        }
        directory.entries.insert(
            name.into(),
            Entry {
                expires: Instant::now() + TTL,
                child,
            },
        );
        TTL
    }
    pub(crate) fn flush(&self) {
        self.shared.flush.store(true, Ordering::Release);
        let _ = self.send.try_send(Message::Wake);
    }
    pub(crate) fn attribute(&self, ino: u64) {
        if self.send.try_send(Message::Attribute(ino)).is_err() {
            self.flush();
        }
    }
    pub(crate) fn warning(&self) -> Option<Error> {
        self.shared.warning.lock().take()
    }
}
impl Drop for Cache {
    fn drop(&mut self) {
        // The native event-handler's Drop sends Closed only after notify has
        // released its worker. Join our invalidation worker before capabilities.
        self.watcher.get_mut().take();
        if let Some(worker) = self.worker.get_mut().take() {
            let _ = worker.join();
        }
    }
}
impl Shared {
    fn disable(&self, error: Error) {
        if !self.failed.swap(true, Ordering::AcqRel) {
            *self.warning.lock() = Some(error.context("disable metadata caching"));
        }
        self.flush.store(true, Ordering::Release);
    }
    fn notification(&self, result: io::Result<()>) {
        if let Err(error) = result
            && !matches!(error.raw_os_error(), Some(libc::ENOENT | libc::ENODEV))
        {
            self.disable(error.into());
        }
    }
    fn attribute(&self, ino: u64) {
        if let Some(notifier) = self.notifier.read().as_ref() {
            self.notification(notifier.inval_inode(INodeNo(ino), -1, 0));
        }
    }
    fn entry(&self, parent: u64, name: &OsStr, child: Option<u64>) {
        if let Some(child) = child {
            self.attribute(child);
        }
        if let Some(notifier) = self.notifier.read().as_ref() {
            self.notification(notifier.inval_entry(INodeNo(parent), name));
        }
    }
    fn invalidate_all(&self) {
        let entries: Vec<_> = self
            .state
            .lock()
            .directories
            .values()
            .flat_map(|directory| {
                directory.parents.iter().flat_map(|parent| {
                    directory
                        .entries
                        .iter()
                        .map(move |(name, entry)| (*parent, name.clone(), entry.child))
                })
            })
            .collect();
        for (parent, name, child) in entries {
            self.entry(parent, &name, child);
        }
    }
    fn event(&self, event: Event) {
        if matches!(event.kind, EventKind::Access(_)) {
            return;
        }
        let structural = matches!(
            event.kind,
            EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_))
        );
        let removed = matches!(
            event.kind,
            EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_))
        );
        for path in event.paths {
            let entries = {
                let mut state = self.state.lock();
                let own = state.paths.get(&path).copied();
                let identity = own.or_else(|| {
                    path.parent()
                        .and_then(|parent| state.paths.get(parent))
                        .copied()
                });
                let Some(directory) =
                    identity.and_then(|identity| state.directories.get_mut(&identity))
                else {
                    continue;
                };
                if own.is_some() && removed {
                    directory.dead = true;
                }
                let name = path.file_name().unwrap_or_else(|| OsStr::new("."));
                let child = directory.entries.get(name).and_then(|entry| entry.child);
                directory
                    .parents
                    .iter()
                    .map(|parent| (*parent, name.to_owned(), child))
                    .collect::<Vec<_>>()
            };
            for (parent, name, child) in entries {
                if structural {
                    self.attribute(parent);
                    self.entry(parent, &name, child);
                } else if let Some(child) = child {
                    self.attribute(child);
                }
            }
        }
        if removed {
            if let Some(view) = self.view.upgrade() {
                view.sniffs.clear();
            }
            self.invalidate_all();
        }
    }
    fn run(&self, receive: Receiver<Message>) {
        loop {
            if self.flush.swap(false, Ordering::AcqRel) {
                self.invalidate_all();
            }
            match receive.recv_timeout(Duration::from_millis(50)) {
                Ok(Message::Event(Ok(event))) => self.event(event),
                Ok(Message::Event(Err(error))) => self.disable(error.into()),
                Ok(Message::Attribute(ino)) => self.attribute(ino),
                Ok(Message::Wake) => {}
                Ok(Message::Closed) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
    }
}
#[cfg(test)]
#[path = "../tests/unit/metadata.rs"]
mod tests;
