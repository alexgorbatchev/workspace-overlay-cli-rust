//! Native FUSE dispatch. Kernel references and open descriptions have separate ownership.
use crate::{
    Cancellation,
    attributes::Change,
    file::{Lock, OpenFile, attributes},
    view::{View, errno},
};
use cap_std::fs::{Dir, FileTypeExt};
use fuser::*;
use parking_lot::Mutex;
use std::{
    collections::{BTreeSet, HashMap},
    ffi::{OsStr, OsString},
    io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime},
};

struct Node {
    paths: BTreeSet<PathBuf>,
    lookups: u64,
}
struct Directory {
    entries: Vec<(OsString, FileType)>,
    backing: Dir,
}
#[cfg(test)]
#[derive(Default)]
pub(crate) struct Counts {
    pub(crate) lookups: AtomicU64,
    pub(crate) attrs: AtomicU64,
}
pub(crate) struct Filesystem {
    #[cfg(test)]
    pub(crate) counts: Arc<Counts>,
    pub(crate) cache: Arc<crate::metadata::Cache>,
    view: Arc<View>,
    nodes: Mutex<HashMap<u64, Node>>,
    files: Mutex<HashMap<u64, Arc<OpenFile>>>,
    directories: Mutex<HashMap<u64, Arc<Directory>>>,
    next: AtomicU64,
    cancelled: Cancellation,
    lock_workers: Mutex<Vec<std::thread::JoinHandle<()>>>,
}
fn kind(value: cap_std::fs::FileType) -> FileType {
    if value.is_dir() {
        FileType::Directory
    } else if value.is_symlink() {
        FileType::Symlink
    } else if value.is_fifo() {
        FileType::NamedPipe
    } else if value.is_socket() {
        FileType::Socket
    } else if value.is_char_device() {
        FileType::CharDevice
    } else if value.is_block_device() {
        FileType::BlockDevice
    } else {
        FileType::RegularFile
    }
}
fn empty(reply: ReplyEmpty, result: io::Result<()>) {
    match result {
        Ok(()) => reply.ok(),
        Err(error) => reply.error(error.into()),
    }
}
impl Filesystem {
    pub(crate) fn new(view: Arc<View>) -> io::Result<Self> {
        let cache =
            Arc::new(crate::metadata::Cache::new(Arc::downgrade(&view)).map_err(io::Error::other)?);
        *view.cache.write() = Arc::downgrade(&cache);
        Ok(Self {
            #[cfg(test)]
            counts: Arc::new(Counts::default()),
            cache,
            view,
            nodes: Mutex::new(HashMap::from([(
                1,
                Node {
                    paths: BTreeSet::from([PathBuf::from(".")]),
                    lookups: 1,
                },
            )])),
            files: Mutex::new(HashMap::new()),
            directories: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
            cancelled: Cancellation::default(),
            lock_workers: Mutex::new(Vec::new()),
        })
    }
    fn path(&self, ino: INodeNo) -> io::Result<PathBuf> {
        self.nodes
            .lock()
            .get(&ino.0)
            .and_then(|node| node.paths.first())
            .cloned()
            .ok_or_else(|| errno(libc::ENOENT))
    }
    fn child(&self, parent: INodeNo, name: &OsStr) -> io::Result<PathBuf> {
        if name == OsStr::new(".")
            || name == OsStr::new("..")
            || name.as_encoded_bytes().contains(&b'/')
        {
            return Err(errno(libc::EINVAL));
        }
        Ok(self.path(parent)?.join(name))
    }
    fn register(&self, path: PathBuf, mut attr: FileAttr) -> FileAttr {
        if path == Path::new(".") {
            attr.ino = INodeNo(1);
        }
        let mut nodes = self.nodes.lock();
        let node = nodes.entry(attr.ino.0).or_insert_with(|| Node {
            paths: BTreeSet::new(),
            lookups: 0,
        });
        node.paths.insert(path);
        node.lookups += 1;
        attr
    }
    fn entry(&self, path: PathBuf, pid: u32) -> io::Result<FileAttr> {
        let attr = self.view.metadata(&path, pid)?;
        Ok(self.register(path, attr))
    }
    fn file(&self, fh: FileHandle) -> io::Result<Arc<OpenFile>> {
        self.files
            .lock()
            .get(&fh.0)
            .cloned()
            .ok_or_else(|| errno(libc::EBADF))
    }
    fn handle(&self, file: Arc<OpenFile>) -> (FileHandle, FopenFlags) {
        let flags = if file.direct_io() {
            FopenFlags::FOPEN_DIRECT_IO
        } else {
            FopenFlags::empty()
        };
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.files.lock().insert(id, file);
        (FileHandle(id), flags)
    }
    fn removed(&self, path: &Path) {
        for node in self.nodes.lock().values_mut() {
            node.paths.remove(path);
        }
    }
    fn moved(&self, source: &Path, destination: &Path) {
        let mut nodes = self.nodes.lock();
        for node in nodes.values_mut() {
            let old: Vec<_> = node
                .paths
                .iter()
                .filter(|p| p.starts_with(source))
                .cloned()
                .collect();
            node.paths.retain(|path| !path.starts_with(destination));
            for path in old {
                node.paths.remove(&path);
                node.paths
                    .insert(destination.join(path.strip_prefix(source).expect("filtered prefix")));
            }
        }
        for file in self.files.lock().values() {
            let mut path = file.path.write();
            if let Ok(tail) = path.strip_prefix(source) {
                *path = destination.join(tail);
            }
        }
    }
    fn attr(&self, ino: INodeNo, fh: Option<FileHandle>, pid: u32) -> io::Result<FileAttr> {
        if let Ok(path) = self.path(ino) {
            let path = crate::view::relative(&path)?;
            if matches!(path.to_str(), Some("." | ".git" | ".git/info/exclude")) {
                self.view.refresh_exclude().map_err(|_| errno(libc::EIO))?;
            }
        }
        let mut attr = if let Some(fh) = fh {
            if let Some(file) = self.files.lock().get(&fh.0).cloned() {
                file.metadata()?
            } else if let Some(dir) = self.directories.lock().get(&fh.0).cloned() {
                attributes(&dir.backing.dir_metadata()?, ino.0)
            } else {
                return Err(errno(libc::EBADF));
            }
        } else {
            self.view.metadata(&self.path(ino)?, pid)?
        };
        attr.ino = ino;
        Ok(attr)
    }
    fn entry_reply(&self, reply: ReplyEntry, result: io::Result<FileAttr>) {
        match result {
            Ok(attr) => reply.entry(&Duration::ZERO, &attr, Generation(0)),
            Err(error) => reply.error(error.into()),
        }
    }
    fn attr_ttl(&self, ino: INodeNo, attr: &FileAttr) -> Duration {
        if attr.kind != FileType::RegularFile {
            return Duration::ZERO;
        }
        let Ok(path) = self.path(ino) else {
            return Duration::ZERO;
        };
        let Some(name) = path.file_name() else {
            return Duration::ZERO;
        };
        if !self.view.project_namespace(&path) {
            return Duration::ZERO;
        }
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let nodes = self.nodes.lock();
        let Some(parent_ino) = nodes
            .iter()
            .find_map(|(ino, node)| node.paths.contains(parent).then_some(*ino))
        else {
            return Duration::ZERO;
        };
        self.cache
            .remember(&self.view, parent_ino, parent, name, Some(ino.0))
    }
}
impl fuser::Filesystem for Filesystem {
    fn init(&mut self, _: &Request, config: &mut KernelConfig) -> io::Result<()> {
        let supported =
            config.capabilities() & (InitFlags::FUSE_POSIX_LOCKS | InitFlags::FUSE_FLOCK_LOCKS);
        config
            .add_capabilities(supported)
            .map_err(|flags| io::Error::other(format!("unsupported lock capabilities: {flags:?}")))
    }
    fn lookup(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        #[cfg(test)]
        self.counts.lookups.fetch_add(1, Ordering::Relaxed);
        let parent_path = self.path(parent);
        // Install the backing watch before metadata resolution. A change during
        // lookup must not fall into an unwatched interval before publication.
        let watched = parent_path.as_ref().map_or(Duration::ZERO, |path| {
            self.cache.remember(&self.view, parent.0, path, name, None)
        });
        let result = self.child(parent, name).and_then(|path| {
            if parent.0 == 1 || path.starts_with(".git") {
                self.view.refresh_exclude().map_err(io::Error::other)?;
            }
            let project = self.view.layers[0].root.symlink_metadata(&path).is_ok();
            let invariant = self.view.project_namespace(&path);
            self.entry(path, req.pid())
                .map(|attr| (attr, project, invariant))
        });
        match result {
            Ok((attr, project, invariant)) => {
                let tracked = parent_path.as_ref().map_or(Duration::ZERO, |path| {
                    self.cache
                        .remember(&self.view, parent.0, path, name, Some(attr.ino.0))
                });
                let ttl = if project { tracked } else { Duration::ZERO };
                let attr_ttl = if invariant && attr.kind == FileType::RegularFile {
                    ttl
                } else {
                    Duration::ZERO
                };
                reply.entry_with_ttls(&attr_ttl, &ttl, &attr, Generation(0));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let invariant = self
                    .child(parent, name)
                    .is_ok_and(|path| self.view.project_namespace(&path));
                if watched.is_zero() || !invariant {
                    reply.error(Errno::ENOENT);
                } else {
                    reply.negative(&watched);
                }
            }
            Err(error) => reply.error(error.into()),
        }
    }
    fn forget(&self, _: &Request, ino: INodeNo, count: u64) {
        if ino.0 == 1 {
            return;
        }
        let mut nodes = self.nodes.lock();
        if let Some(node) = nodes.get_mut(&ino.0) {
            node.lookups = node.lookups.saturating_sub(count);
            if node.lookups == 0 {
                nodes.remove(&ino.0);
            }
        }
    }
    fn getattr(&self, req: &Request, ino: INodeNo, fh: Option<FileHandle>, reply: ReplyAttr) {
        #[cfg(test)]
        self.counts.attrs.fetch_add(1, Ordering::Relaxed);
        match self.attr(ino, fh, req.pid()) {
            Ok(attr) => {
                let ttl = self.attr_ttl(ino, &attr);
                reply.attr(&ttl, &attr)
            }
            Err(error) => reply.error(error.into()),
        }
    }
    fn setattr(
        &self,
        req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _: Option<SystemTime>,
        fh: Option<FileHandle>,
        _: Option<SystemTime>,
        _: Option<SystemTime>,
        _: Option<SystemTime>,
        flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        if flags.is_some() {
            reply.error(Errno::ENOTSUP);
            return;
        }
        let change = Change {
            mode,
            uid,
            gid,
            size,
            atime,
            mtime,
        };
        let result = if let Some(fh) = fh {
            self.file(fh).and_then(|file| file.set_attributes(&change))
        } else {
            self.path(ino)
                .and_then(|path| self.view.set_attributes(&path, &change, req.pid()))
        };
        match result {
            Ok(mut attr) => {
                self.cache.attribute(ino.0);
                attr.ino = ino;
                reply.attr(&Duration::ZERO, &attr)
            }
            Err(error) => reply.error(error.into()),
        }
    }
    fn readlink(&self, _: &Request, ino: INodeNo, reply: ReplyData) {
        match self.path(ino).and_then(|path| self.view.read_link(&path)) {
            Ok(path) => reply.data(path.as_os_str().as_encoded_bytes()),
            Err(error) => reply.error(error.into()),
        }
    }
    fn mkdir(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        let result = (|| {
            let path = self.child(parent, name)?;
            self.view.mkdir(&path, mode & !umask)?;
            self.entry(path, req.pid())
        })();
        self.entry_reply(reply, result)
    }
    fn symlink(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        let result = (|| {
            let path = self.child(parent, name)?;
            self.view.symlink(target, &path)?;
            self.entry(path, req.pid())
        })();
        self.entry_reply(reply, result)
    }
    fn unlink(&self, _: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        empty(
            reply,
            (|| {
                let path = self.child(parent, name)?;
                self.view.unlink(&path)?;
                self.removed(&path);
                Ok(())
            })(),
        )
    }
    fn rmdir(&self, _: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        empty(
            reply,
            (|| {
                let path = self.child(parent, name)?;
                self.view.rmdir(&path)?;
                self.removed(&path);
                Ok(())
            })(),
        )
    }
    fn rename(
        &self,
        _: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        empty(
            reply,
            (|| {
                let source = self.child(parent, name)?;
                let destination = self.child(newparent, newname)?;
                self.view.rename(&source, &destination, flags.bits())?;
                self.moved(&source, &destination);
                Ok(())
            })(),
        )
    }
    fn link(
        &self,
        req: &Request,
        ino: INodeNo,
        newparent: INodeNo,
        newname: &OsStr,
        reply: ReplyEntry,
    ) {
        let result = (|| {
            let source = self.path(ino)?;
            let destination = self.child(newparent, newname)?;
            self.view.hard_link(&source, &destination)?;
            self.entry(destination, req.pid())
        })();
        self.entry_reply(reply, result)
    }
    fn open(&self, req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        match self
            .path(ino)
            .and_then(|path| self.view.open_file(&path, flags.0, req.pid()))
        {
            Ok(file) => {
                let (handle, flags) = self.handle(file);
                reply.opened(handle, flags)
            }
            Err(error) => reply.error(error.into()),
        }
    }
    fn create(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let result = (|| {
            let path = self.child(parent, name)?;
            let file = self
                .view
                .create_file(&path, flags, mode & !umask, req.pid())?;
            let attr = self.register(path, file.metadata()?);
            let (handle, flags) = self.handle(file);
            Ok::<_, io::Error>((attr, handle, flags))
        })();
        match result {
            Ok((attr, handle, flags)) => {
                reply.created(&Duration::ZERO, &attr, Generation(0), handle, flags)
            }
            Err(error) => reply.error(error.into()),
        }
    }
    fn read(
        &self,
        _: &Request,
        _: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _: OpenFlags,
        _: Option<LockOwner>,
        reply: ReplyData,
    ) {
        match self
            .file(fh)
            .and_then(|file| file.read(offset, size as usize))
        {
            Ok(data) => reply.data(&data),
            Err(error) => reply.error(error.into()),
        }
    }
    fn write(
        &self,
        _: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _: WriteFlags,
        _: OpenFlags,
        _: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        match self.file(fh).and_then(|file| file.write(offset, data)) {
            Ok(count) => {
                self.cache.attribute(ino.0);
                reply.written(count as u32)
            }
            Err(error) => reply.error(error.into()),
        }
    }
    fn flush(&self, _: &Request, _: INodeNo, fh: FileHandle, _: LockOwner, reply: ReplyEmpty) {
        empty(reply, self.file(fh).and_then(|file| file.flush()))
    }
    fn fsync(&self, _: &Request, _: INodeNo, fh: FileHandle, data_only: bool, reply: ReplyEmpty) {
        empty(reply, self.file(fh).and_then(|file| file.fsync(data_only)))
    }
    fn release(
        &self,
        _: &Request,
        _: INodeNo,
        fh: FileHandle,
        _: OpenFlags,
        _: Option<LockOwner>,
        _: bool,
        reply: ReplyEmpty,
    ) {
        empty(
            reply,
            self.files
                .lock()
                .remove(&fh.0)
                .ok_or_else(|| errno(libc::EBADF))
                .and_then(|file| file.release()),
        )
    }
    fn opendir(&self, _: &Request, ino: INodeNo, _: OpenFlags, reply: ReplyOpen) {
        let result = (|| {
            self.view.refresh_exclude().map_err(|_| errno(libc::EIO))?;
            let path = self.path(ino)?;
            let parts = self.view.resolve(&path)?;
            let part = parts.last().expect("nonempty resolution");
            let backing = self.view.layers[part.layer].root.open_dir(&path)?;
            let mut entries = vec![
                (OsString::from("."), FileType::Directory),
                (OsString::from(".."), FileType::Directory),
            ];
            entries.extend(
                self.view
                    .entries(&path)?
                    .into_iter()
                    .map(|entry| (entry.name, kind(entry.kind))),
            );
            let handle = self.next.fetch_add(1, Ordering::Relaxed);
            self.directories
                .lock()
                .insert(handle, Arc::new(Directory { entries, backing }));
            Ok::<_, io::Error>(FileHandle(handle))
        })();
        match result {
            Ok(handle) => reply.opened(handle, FopenFlags::empty()),
            Err(error) => reply.error(error.into()),
        }
    }
    fn readdir(
        &self,
        _: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let Some(directory) = self.directories.lock().get(&fh.0).cloned() else {
            reply.error(Errno::EBADF);
            return;
        };
        for (index, (name, kind)) in directory
            .entries
            .iter()
            .enumerate()
            .skip(usize::try_from(offset).unwrap_or(usize::MAX))
        {
            if reply.add(
                if index == 0 { ino } else { INodeNo(0) },
                index as u64 + 1,
                *kind,
                name,
            ) {
                break;
            }
        }
        reply.ok()
    }
    fn releasedir(&self, _: &Request, _: INodeNo, fh: FileHandle, _: OpenFlags, reply: ReplyEmpty) {
        empty(
            reply,
            self.directories
                .lock()
                .remove(&fh.0)
                .map(|_| ())
                .ok_or_else(|| errno(libc::EBADF)),
        )
    }
    fn fsyncdir(
        &self,
        _: &Request,
        _: INodeNo,
        fh: FileHandle,
        data_only: bool,
        reply: ReplyEmpty,
    ) {
        let directory = self.directories.lock().get(&fh.0).cloned();
        empty(
            reply,
            directory
                .ok_or_else(|| errno(libc::EBADF))
                .and_then(|directory| {
                    // A directory capability can be O_PATH, which cannot be
                    // synchronized. Open a readable description through it.
                    let file = directory.backing.open(".")?.into_std();
                    let synchronized = if data_only {
                        rustix::fs::fdatasync(&file)
                    } else {
                        rustix::fs::fsync(&file)
                    }
                    .map_err(Into::into);
                    crate::file::finish_io(synchronized, crate::file::close_native(file))
                }),
        )
    }
    fn statfs(&self, _: &Request, _: INodeNo, reply: ReplyStatfs) {
        match rustix::fs::fstatfs(&self.view.layers[0].root) {
            Ok(stat) => reply.statfs(
                stat.f_blocks,
                stat.f_bfree,
                stat.f_bavail,
                stat.f_files,
                stat.f_ffree,
                stat.f_bsize as u32,
                stat.f_namelen as u32,
                stat.f_frsize as u32,
            ),
            Err(error) => reply.error(Errno::from_i32(error.raw_os_error())),
        }
    }
    fn getlk(
        &self,
        _: &Request,
        _: INodeNo,
        fh: FileHandle,
        _: LockOwner,
        start: u64,
        end: u64,
        typ: i32,
        pid: u32,
        reply: ReplyLock,
    ) {
        match self.file(fh).and_then(|file| {
            file.get_lock(Lock {
                start,
                end,
                typ,
                pid,
            })
        }) {
            Ok(lock) => reply.locked(lock.start, lock.end, lock.typ, lock.pid),
            Err(error) => reply.error(error.into()),
        }
    }
    fn setlk(
        &self,
        _: &Request,
        _: INodeNo,
        fh: FileHandle,
        _: LockOwner,
        start: u64,
        end: u64,
        typ: i32,
        pid: u32,
        sleep: bool,
        flags: u32,
        reply: ReplyEmpty,
    ) {
        if flags & !1 != 0 {
            reply.error(Errno::EINVAL);
            return;
        }
        let file = match self.file(fh) {
            Ok(file) => file,
            Err(error) => {
                reply.error(error.into());
                return;
            }
        };
        let requested = Lock {
            start,
            end,
            typ,
            pid,
        };
        let flock = flags & 1 != 0;
        if !sleep {
            empty(reply, file.set_lock(requested, flock));
            return;
        }
        // A blocked lock must not occupy the FUSE dispatcher that processes
        // the matching unlock. All workers are cancelled and joined on teardown.
        let cancelled = self.cancelled.clone();
        let worker = std::thread::Builder::new()
            .name("overlay-lock".into())
            .spawn(move || {
                loop {
                    if cancelled.is_cancelled() {
                        reply.error(Errno::EINTR);
                        break;
                    }
                    match file.set_lock(requested, flock) {
                        Err(error)
                            if matches!(
                                error.raw_os_error(),
                                Some(libc::EAGAIN | libc::EACCES)
                            ) =>
                        {
                            cancelled.wait(Duration::from_millis(10));
                        }
                        result => {
                            empty(reply, result);
                            break;
                        }
                    }
                }
            });
        // If thread creation fails, dropping its reply sends the native EIO.
        if let Ok(worker) = worker {
            let mut workers = self.lock_workers.lock();
            let mut index = 0;
            while index < workers.len() {
                if workers[index].is_finished() {
                    // A completed reply has no remaining caller to report a panic to.
                    let _ = workers.swap_remove(index).join();
                } else {
                    index += 1;
                }
            }
            workers.push(worker);
        }
    }
    fn fallocate(
        &self,
        _: &Request,
        _: INodeNo,
        fh: FileHandle,
        offset: u64,
        length: u64,
        mode: i32,
        reply: ReplyEmpty,
    ) {
        empty(
            reply,
            self.file(fh)
                .and_then(|file| file.allocate(offset, length, mode)),
        )
    }
    fn lseek(
        &self,
        _: &Request,
        _: INodeNo,
        fh: FileHandle,
        offset: i64,
        whence: i32,
        reply: ReplyLseek,
    ) {
        match self.file(fh).and_then(|file| file.seek(offset, whence)) {
            Ok(offset) => reply.offset(offset),
            Err(error) => reply.error(error.into()),
        }
    }
    fn ioctl(
        &self,
        _: &Request,
        _: INodeNo,
        fh: FileHandle,
        _: IoctlFlags,
        command: u32,
        argument: u64,
        input: &[u8],
        output_size: u32,
        reply: ReplyIoctl,
    ) {
        match self
            .file(fh)
            .and_then(|file| file.ioctl(command, argument, input, output_size))
        {
            Ok((result, data)) => reply.ioctl(result, &data),
            Err(error) => reply.error(error.into()),
        }
    }
}
impl Drop for Filesystem {
    fn drop(&mut self) {
        self.cancelled.cancel();
        // Best-effort teardown after native unmount; there is no live request
        // owner to receive a worker panic once the filesystem is destroyed.
        for worker in self.lock_workers.get_mut().drain(..) {
            let _ = worker.join();
        }
    }
}
