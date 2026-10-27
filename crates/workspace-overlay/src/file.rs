pub(crate) use crate::operations::Lock;
use crate::{
    markers::{MAX_MERGED_SIZE, Rule, Section},
    view::{View, errno, is_git_caller, relative},
};
use cap_std::fs::{Metadata, MetadataExt, OpenOptions, OpenOptionsExt};
use fuser::{FileAttr, FileType, INodeNo};
use parking_lot::{Mutex, RwLock};
use std::{
    fs::File,
    io::{self, Read, Write},
    os::{fd::IntoRawFd, unix::fs::FileExt},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub(crate) fn close_native(file: File) -> io::Result<()> {
    let descriptor = file.into_raw_fd();
    // SAFETY: into_raw_fd transfers exactly one owned descriptor. A failed
    // Linux close must not be retried because its number can already be reused.
    if unsafe { libc::close(descriptor) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
pub(crate) fn finish_io(operation: io::Result<()>, cleanup: io::Result<()>) -> io::Result<()> {
    match (operation, cleanup) {
        (Ok(()), result) | (result, Ok(())) => result,
        (Err(operation), Err(cleanup)) => Err(io::Error::other(crate::Failures(vec![
            operation.into(),
            cleanup.into(),
        ]))),
    }
}

fn timestamp(seconds: i64, nanos: i64) -> SystemTime {
    if seconds >= 0 {
        UNIX_EPOCH + Duration::new(seconds as u64, nanos as u32)
    } else {
        UNIX_EPOCH - Duration::from_secs(seconds.unsigned_abs())
            + Duration::from_nanos(nanos as u64)
    }
}
pub(crate) fn attributes(info: &Metadata, ino: u64) -> FileAttr {
    let kind = match info.mode() & libc::S_IFMT {
        libc::S_IFDIR => FileType::Directory,
        libc::S_IFLNK => FileType::Symlink,
        libc::S_IFIFO => FileType::NamedPipe,
        libc::S_IFCHR => FileType::CharDevice,
        libc::S_IFBLK => FileType::BlockDevice,
        libc::S_IFSOCK => FileType::Socket,
        _ => FileType::RegularFile,
    };
    FileAttr {
        ino: INodeNo(ino),
        size: info.len(),
        blocks: info.blocks(),
        atime: timestamp(info.atime(), info.atime_nsec()),
        mtime: timestamp(info.mtime(), info.mtime_nsec()),
        ctime: timestamp(info.ctime(), info.ctime_nsec()),
        crtime: info
            .created()
            .map(cap_std::time::SystemTime::into_std)
            .unwrap_or(UNIX_EPOCH),
        kind,
        perm: (info.mode() & 0o7777) as u16,
        nlink: info.nlink() as u32,
        uid: info.uid(),
        gid: info.gid(),
        rdev: info.rdev() as u32,
        blksize: info.blksize() as u32,
        flags: 0,
    }
}
pub(crate) fn open_native(
    view: &View,
    layer: usize,
    path: &Path,
    flags: i32,
    mode: u32,
) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options
        .read(flags & libc::O_ACCMODE != libc::O_WRONLY)
        .write(flags & libc::O_ACCMODE != libc::O_RDONLY)
        .create(flags & libc::O_CREAT != 0)
        .create_new(flags & libc::O_EXCL != 0)
        .truncate(flags & libc::O_TRUNC != 0)
        .mode(mode)
        .custom_flags(
            flags
                & !(libc::O_ACCMODE
                    | libc::O_CREAT
                    | libc::O_EXCL
                    | libc::O_TRUNC
                    | libc::O_APPEND),
        );
    Ok(view.layers[layer]
        .root
        .open_with(path, &options)?
        .into_std())
}
pub(crate) fn read_bounded(file: File) -> io::Result<Vec<u8>> {
    let mut data = Vec::new();
    file.take((MAX_MERGED_SIZE + 1) as u64)
        .read_to_end(&mut data)?;
    if data.len() > MAX_MERGED_SIZE {
        return Err(errno(libc::EFBIG));
    }
    Ok(data)
}

struct Staged {
    sections: Vec<Section>,
    rule: Option<Arc<Rule>>,
    prefix: Vec<u8>,
    data: Vec<u8>,
    dirty: bool,
    failed: Option<i32>,
}
enum Kind {
    Native,
    Merged(Mutex<Staged>),
    Notice { data: Vec<u8>, attr: FileAttr },
}
pub(crate) struct OpenFile {
    view: Arc<View>,
    pub(crate) path: RwLock<PathBuf>,
    file: RwLock<Option<File>>,
    kind: Kind,
    ino: u64,
    readable: bool,
    writable: bool,
    append: bool,
    closed: AtomicBool,
}
impl View {
    pub(crate) fn open_file(
        self: &Arc<Self>,
        path: &Path,
        flags: i32,
        pid: u32,
    ) -> io::Result<Arc<OpenFile>> {
        let _mutation = self.mutation.lock();
        self.open_file_locked(path, flags, pid)
    }
    pub(crate) fn open_file_locked(
        self: &Arc<Self>,
        path: &Path,
        flags: i32,
        pid: u32,
    ) -> io::Result<Arc<OpenFile>> {
        let path = relative(path)?;
        let mut parts = self.resolve(&path)?;
        let last = parts.last().expect("nonempty resolution");
        let ino = self.stable(last);
        if last.metadata().is_dir() {
            return Err(errno(libc::EISDIR));
        }
        if !last.metadata().is_file() {
            return Err(errno(libc::ENOTSUP));
        }
        let original_attr = attributes(last.metadata(), ino);
        let git = (parts.len() != 1 || parts[0].layer != 0) && is_git_caller(pid);
        if git && parts[0].layer != 0 {
            return Err(errno(libc::ENOENT));
        }
        let mut rule = self.markers.find(&path);
        if !git && rule.is_some() {
            parts = self.document(&path, parts);
        }
        let (kind, file) = if git || parts.len() == 1 {
            let index = if git { 0 } else { parts[0].layer };
            (
                Kind::Native,
                Some(open_native(self, index, &path, flags, 0)?),
            )
        } else {
            let real: Vec<_> = parts.iter().filter(|p| p.info.is_some()).cloned().collect();
            if let Some(notice) = self.notice(&path, &real).map_err(|_| errno(libc::EIO))? {
                log::warn!(
                    "cannot merge {}: binary collision; serving an explanation instead of file content",
                    path.display()
                );
                (
                    Kind::Notice {
                        data: notice,
                        attr: original_attr,
                    },
                    None,
                )
            } else {
                let mut sections: Vec<_> =
                    parts.iter().map(|part| self.section(&path, part)).collect();
                let size = if let Some(rule) = &rule {
                    let mut sizes = Vec::new();
                    for (section, part) in sections.iter().zip(&parts) {
                        let size = part.info.as_ref().map_or(0, Metadata::len);
                        let mut tail = [0];
                        let last = if size > 0 {
                            if self.sniffs.read(
                                &self.layers[part.layer].root,
                                part.layer,
                                &path,
                                part.metadata(),
                                &mut tail,
                                size - 1,
                            )? != 1
                            {
                                return Err(io::ErrorKind::UnexpectedEof.into());
                            }
                            Some(tail[0])
                        } else {
                            None
                        };
                        sizes.push((section.clone(), size, last));
                    }
                    rule.size(&sizes)?
                } else {
                    parts.iter().try_fold(0u64, |size, part| {
                        size.checked_add(part.metadata().len())
                            .ok_or_else(|| errno(libc::EFBIG))
                    })?
                };
                if size > MAX_MERGED_SIZE as u64 {
                    return Err(errno(libc::EFBIG));
                }
                for (section, part) in sections.iter_mut().zip(&parts) {
                    if part.info.is_some() {
                        section.data =
                            read_bounded(open_native(self, part.layer, &path, libc::O_RDONLY, 0)?)?;
                    }
                }
                let mut prefix = Vec::new();
                let mut data = if let Some(rule) = &rule {
                    rule.render(&sections)?
                } else {
                    for section in &sections[..sections.len() - 1] {
                        prefix.extend_from_slice(&section.data);
                    }
                    let mut data = prefix.clone();
                    data.extend_from_slice(&sections.last().expect("nonempty sections").data);
                    if data.len() > MAX_MERGED_SIZE {
                        return Err(errno(libc::EFBIG));
                    }
                    data
                };
                let file = open_native(
                    self,
                    parts.last().expect("nonempty resolution").layer,
                    &path,
                    flags & !(libc::O_TRUNC | libc::O_APPEND),
                    0,
                )?;
                if flags & libc::O_TRUNC != 0 {
                    data.clear();
                }
                (
                    Kind::Merged(Mutex::new(Staged {
                        sections,
                        rule: rule.take(),
                        prefix,
                        data,
                        dirty: flags & libc::O_TRUNC != 0,
                        failed: None,
                    })),
                    Some(file),
                )
            }
        };
        let handle = Arc::new(OpenFile {
            view: self.clone(),
            path: RwLock::new(path.clone()),
            file: RwLock::new(file),
            kind,
            ino,
            readable: flags & libc::O_ACCMODE != libc::O_WRONLY,
            writable: flags & libc::O_ACCMODE != libc::O_RDONLY,
            append: flags & libc::O_APPEND != 0,
            closed: AtomicBool::new(false),
        });
        if handle.writable && matches!(handle.kind, Kind::Merged(_)) {
            let mut writers = self.writers.lock();
            let writers = writers.entry(path).or_default();
            writers.retain(|writer| writer.strong_count() > 0);
            writers.push(Arc::downgrade(&handle));
        }
        Ok(handle)
    }
    pub(crate) fn metadata(&self, path: &Path, pid: u32) -> io::Result<FileAttr> {
        let path = relative(path)?;
        let mut parts = self.resolve(&path)?;
        if (parts.len() != 1 || parts[0].layer != 0) && is_git_caller(pid) {
            return if parts[0].layer == 0 {
                Ok(attributes(
                    parts[0].metadata(),
                    self.stable(parts.last().expect("nonempty resolution")),
                ))
            } else {
                Err(errno(libc::ENOENT))
            };
        }
        let writers = self.writers.lock().get(&path).cloned().unwrap_or_default();
        for writer in writers {
            if let Some(writer) = writer.upgrade()
                && let Some(attr) = writer.pending_attributes()?
            {
                return Ok(attr);
            }
        }
        let last = parts.last().expect("nonempty resolution");
        let mut attr = attributes(last.metadata(), self.stable(last));
        if !last.metadata().is_file() {
            return Ok(attr);
        }
        if let Ok(Some(notice)) = self.notice(&path, &parts) {
            attr.size = notice.len() as u64;
            return Ok(attr);
        }
        let rule = self.markers.find(&path);
        if rule.is_some() {
            parts = self.document(&path, parts);
        }
        if parts.len() > 1 {
            attr.nlink = parts.len() as u32;
            attr.size = parts.iter().try_fold(0u64, |size, part| {
                size.checked_add(part.info.as_ref().map_or(0, Metadata::len))
                    .ok_or_else(|| errno(libc::EFBIG))
            })?;
            if let Some(rule) = rule {
                let size = (|| -> io::Result<u64> {
                    let mut sizes = Vec::new();
                    for part in &parts {
                        let length = part.info.as_ref().map_or(0, Metadata::len);
                        let mut tail = [0];
                        let last = if length > 0 {
                            if self.sniffs.read(
                                &self.layers[part.layer].root,
                                part.layer,
                                &path,
                                part.metadata(),
                                &mut tail,
                                length - 1,
                            )? != 1
                            {
                                return Err(io::ErrorKind::UnexpectedEof.into());
                            }
                            Some(tail[0])
                        } else {
                            None
                        };
                        sizes.push((self.section(&path, part), length, last));
                    }
                    rule.size(&sizes)
                })();
                if let Ok(size) = size {
                    attr.size = size;
                }
            }
        }
        Ok(attr)
    }
    pub(crate) fn save_sections_locked(
        &self,
        path: &Path,
        sections: &[(usize, Vec<u8>)],
    ) -> io::Result<()> {
        for (index, data) in sections {
            let root = &self.layers[*index].root;
            let current = match root.read(path) {
                Ok(current) => current,
                Err(error) if *index == 0 && error.kind() == io::ErrorKind::NotFound => {
                    if !data.is_empty() {
                        let mode = self
                            .records
                            .lock()
                            .vacated
                            .get(path)
                            .copied()
                            .unwrap_or(0o644);
                        let mut file = open_native(
                            self,
                            0,
                            path,
                            libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC,
                            mode,
                        )
                        .map_err(|error| {
                            if error.kind() == io::ErrorKind::NotFound {
                                errno(libc::EPERM)
                            } else {
                                error
                            }
                        })?;
                        let written = file.write_all(data);
                        finish_io(written, close_native(file))?;
                        self.records.lock().vacated.remove(path);
                        self.changed(0)?;
                    }
                    continue;
                }
                Err(error) => return Err(error),
            };
            if current == *data {
                continue;
            }
            let mut file = open_native(self, *index, path, libc::O_WRONLY | libc::O_TRUNC, 0)?;
            let written = file.write_all(data);
            finish_io(written, close_native(file))?;
        }
        self.sniffs.clear();
        Ok(())
    }
}
impl OpenFile {
    pub(crate) fn new_native(
        view: Arc<View>,
        path: PathBuf,
        file: File,
        ino: u64,
        flags: i32,
    ) -> Self {
        Self {
            view,
            path: RwLock::new(path),
            file: RwLock::new(Some(file)),
            kind: Kind::Native,
            ino,
            readable: flags & libc::O_ACCMODE != libc::O_WRONLY,
            writable: flags & libc::O_ACCMODE != libc::O_RDONLY,
            append: false,
            closed: AtomicBool::new(false),
        }
    }
    fn check_open(&self) -> io::Result<()> {
        if self.closed.load(Ordering::Acquire) {
            Err(errno(libc::EBADF))
        } else {
            Ok(())
        }
    }
    pub(crate) fn direct_io(&self) -> bool {
        !matches!(self.kind, Kind::Native)
    }
    pub(crate) fn native<T>(
        &self,
        operation: impl FnOnce(&File) -> io::Result<T>,
    ) -> io::Result<T> {
        let file = self.file.read();
        operation(file.as_ref().ok_or_else(|| errno(libc::EBADF))?)
    }
    pub(crate) fn is_notice(&self) -> bool {
        matches!(self.kind, Kind::Notice { .. })
    }
    pub(crate) fn read(&self, offset: u64, size: usize) -> io::Result<Vec<u8>> {
        self.check_open()?;
        if !self.readable {
            return Err(errno(libc::EBADF));
        }
        let slice = |data: &[u8]| {
            let start = usize::try_from(offset)
                .unwrap_or(usize::MAX)
                .min(data.len());
            data[start..start.saturating_add(size).min(data.len())].to_vec()
        };
        match &self.kind {
            Kind::Native => self.native(|file| {
                let mut data = vec![0; size];
                let count = file.read_at(&mut data, offset)?;
                data.truncate(count);
                Ok(data)
            }),
            Kind::Merged(stage) => Ok(slice(&stage.lock().data)),
            Kind::Notice { data, .. } => Ok(slice(data)),
        }
    }
    pub(crate) fn write(&self, offset: u64, data: &[u8]) -> io::Result<usize> {
        self.check_open()?;
        if matches!(self.kind, Kind::Notice { .. }) {
            return Err(errno(libc::EPERM));
        }
        if !self.writable {
            return Err(errno(libc::EBADF));
        }
        match &self.kind {
            Kind::Native => self.native(|file| file.write_at(data, offset)),
            Kind::Merged(stage) => {
                let mut stage = stage.lock();
                let offset = if self.append {
                    stage.data.len() as u64
                } else {
                    offset
                };
                if offset > MAX_MERGED_SIZE as u64
                    || data.len() as u64 > MAX_MERGED_SIZE as u64 - offset
                {
                    return Err(errno(libc::EFBIG));
                }
                let start = offset as usize;
                let end = start + data.len();
                if stage.rule.is_none() && start < stage.prefix.len() {
                    let prefix_end = end.min(stage.prefix.len());
                    if data[..prefix_end - start] != stage.prefix[start..prefix_end] {
                        stage.failed = Some(libc::EPERM);
                        return Err(errno(libc::EPERM));
                    }
                }
                if end > stage.data.len() {
                    stage.data.resize(end, 0);
                }
                stage.data[start..end].copy_from_slice(data);
                stage.dirty = true;
                Ok(data.len())
            }
            Kind::Notice { .. } => unreachable!("notice rejected before writing"),
        }
    }
    pub(crate) fn truncate(&self, length: u64) -> io::Result<()> {
        self.check_open()?;
        if matches!(self.kind, Kind::Notice { .. }) {
            return Err(errno(libc::EPERM));
        }
        if !self.writable {
            return Err(errno(libc::EBADF));
        }
        match &self.kind {
            Kind::Native => self.native(|file| file.set_len(length)),
            Kind::Merged(stage) => {
                if length > MAX_MERGED_SIZE as u64 {
                    return Err(errno(libc::EFBIG));
                }
                let mut stage = stage.lock();
                stage.data.resize(length as usize, 0);
                stage.dirty = true;
                Ok(())
            }
            Kind::Notice { .. } => unreachable!("notice rejected before truncating"),
        }
    }
    pub(crate) fn metadata(&self) -> io::Result<FileAttr> {
        self.check_open()?;
        let mut attr = match &self.kind {
            Kind::Notice { attr, .. } => *attr,
            _ => self.native(|file| {
                Ok(attributes(
                    &Metadata::from_just_metadata(file.metadata()?),
                    self.ino,
                ))
            })?,
        };
        match &self.kind {
            Kind::Merged(stage) => {
                let stage = stage.lock();
                attr.size = stage.data.len() as u64;
                attr.nlink = stage.sections.len() as u32;
            }
            Kind::Notice { data, .. } => attr.size = data.len() as u64,
            Kind::Native => {}
        }
        Ok(attr)
    }
    fn pending_attributes(&self) -> io::Result<Option<FileAttr>> {
        if let Kind::Merged(stage) = &self.kind {
            let stage = stage.lock();
            if stage.dirty {
                let mut attr = self.native(|file| {
                    Ok(attributes(
                        &Metadata::from_just_metadata(file.metadata()?),
                        self.ino,
                    ))
                })?;
                attr.size = stage.data.len() as u64;
                attr.nlink = stage.sections.len() as u32;
                return Ok(Some(attr));
            }
        }
        Ok(None)
    }
    fn commit_locked(&self, stage: &mut Staged) -> io::Result<()> {
        if let Some(code) = stage.failed {
            return Err(errno(code));
        }
        if !stage.dirty {
            return Ok(());
        }
        if let Some(rule) = &stage.rule {
            if stage.data.is_empty() {
                // The documented zero truncation clears only the final source;
                // the Go implementation silently discarded this pending write.
                self.native(|file| file.set_len(0))?;
                stage
                    .sections
                    .last_mut()
                    .expect("nonempty sections")
                    .data
                    .clear();
                stage.data = rule.render(&stage.sections)?;
            } else {
                let sections = rule.parse(&stage.data, &stage.sections)?;
                self.view
                    .save_sections_locked(&self.path.read(), &sections)?;
                for (section, (_, data)) in stage.sections.iter_mut().zip(sections) {
                    section.data = data;
                }
            }
        } else {
            let contribution = if stage.data.is_empty() {
                &[][..]
            } else {
                stage
                    .data
                    .strip_prefix(stage.prefix.as_slice())
                    .ok_or_else(|| errno(libc::EPERM))?
            };
            self.native(|file| {
                file.write_all_at(contribution, 0)?;
                file.set_len(contribution.len() as u64)
            })?;
            let mut data = stage.prefix.clone();
            data.extend_from_slice(contribution);
            stage.data = data;
        }
        stage.dirty = false;
        self.view.sniffs.clear();
        Ok(())
    }
    pub(crate) fn flush(&self) -> io::Result<()> {
        self.check_open()?;
        let _mutation = self.view.mutation.lock();
        if let Kind::Merged(stage) = &self.kind {
            self.commit_locked(&mut stage.lock())?;
        }
        if matches!(self.kind, Kind::Native) {
            self.native(|file| {
                let duplicate = file.try_clone()?.into_raw_fd();
                // SAFETY: try_clone transferred one exclusively owned descriptor.
                if unsafe { libc::close(duplicate) } < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            })?;
        }
        Ok(())
    }
    pub(crate) fn fsync(&self, data_only: bool) -> io::Result<()> {
        self.flush()?;
        if matches!(self.kind, Kind::Notice { .. }) {
            return Ok(());
        }
        self.native(|file| {
            if data_only {
                file.sync_data()
            } else {
                file.sync_all()
            }
        })
    }
    pub(crate) fn release(&self) -> io::Result<()> {
        let commit = self.flush();
        self.closed.store(true, Ordering::Release);
        let close = self.file.write().take().map_or(Ok(()), close_native);
        finish_io(commit, close)
    }
}
