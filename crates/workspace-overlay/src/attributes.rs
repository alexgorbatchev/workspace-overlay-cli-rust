//! Metadata-only operations use native capabilities without reading contents.
use crate::{
    file::OpenFile,
    view::{View, errno, relative},
};
use cap_fs_ext::{DirExt, SystemTimeSpec};
use cap_std::fs::{Permissions, PermissionsExt};
use fuser::{FileAttr, TimeOrNow};
use rustix::{
    fs::{AtFlags, Mode, Timespec, Timestamps, UTIME_NOW, UTIME_OMIT},
    process::{Gid, Uid},
};
use std::{io, path::Path, sync::Arc, time::UNIX_EPOCH};

#[derive(Default)]
pub(crate) struct Change {
    pub(crate) mode: Option<u32>,
    pub(crate) uid: Option<u32>,
    pub(crate) gid: Option<u32>,
    pub(crate) size: Option<u64>,
    pub(crate) atime: Option<TimeOrNow>,
    pub(crate) mtime: Option<TimeOrNow>,
}
impl Change {
    fn validate(&self) -> io::Result<()> {
        if self.uid == Some(u32::MAX) || self.gid == Some(u32::MAX) {
            return Err(errno(libc::EINVAL));
        }
        Ok(())
    }
}
fn spec(time: &TimeOrNow) -> SystemTimeSpec {
    match time {
        TimeOrNow::Now => SystemTimeSpec::SymbolicNow,
        TimeOrNow::SpecificTime(time) => {
            SystemTimeSpec::Absolute(cap_std::time::SystemTime::from_std(*time))
        }
    }
}
fn timestamp(time: Option<&TimeOrNow>) -> io::Result<Timespec> {
    match time {
        None => Ok(Timespec {
            tv_sec: 0,
            tv_nsec: UTIME_OMIT,
        }),
        Some(TimeOrNow::Now) => Ok(Timespec {
            tv_sec: 0,
            tv_nsec: UTIME_NOW,
        }),
        Some(TimeOrNow::SpecificTime(time)) => {
            let (seconds, nanos) = match time.duration_since(UNIX_EPOCH) {
                Ok(duration) => (
                    i64::try_from(duration.as_secs()).map_err(|_| errno(libc::EOVERFLOW))?,
                    i64::from(duration.subsec_nanos()),
                ),
                Err(error) => {
                    let duration = error.duration();
                    let seconds =
                        i64::try_from(duration.as_secs()).map_err(|_| errno(libc::EOVERFLOW))?;
                    if duration.subsec_nanos() == 0 {
                        (-seconds, 0)
                    } else {
                        (
                            -seconds - 1,
                            1_000_000_000 - i64::from(duration.subsec_nanos()),
                        )
                    }
                }
            };
            Ok(Timespec {
                tv_sec: seconds,
                tv_nsec: nanos,
            })
        }
    }
}
impl OpenFile {
    pub(crate) fn set_attributes(&self, change: &Change) -> io::Result<FileAttr> {
        change.validate()?;
        if self.is_notice() {
            return Err(errno(libc::EPERM));
        }
        if let Some(length) = change.size {
            self.truncate(length)?;
        }
        self.native(|file| {
            if change.uid.is_some() || change.gid.is_some() {
                rustix::fs::fchown(
                    file,
                    change.uid.map(Uid::from_raw),
                    change.gid.map(Gid::from_raw),
                )?;
            }
            if let Some(mode) = change.mode {
                rustix::fs::fchmod(file, Mode::from_bits_truncate(mode))?;
            }
            if change.atime.is_some() || change.mtime.is_some() {
                rustix::fs::futimens(
                    file,
                    &Timestamps {
                        last_access: timestamp(change.atime.as_ref())?,
                        last_modification: timestamp(change.mtime.as_ref())?,
                    },
                )?;
            }
            Ok(())
        })?;
        self.metadata()
    }
}
impl View {
    pub(crate) fn set_attributes(
        self: &Arc<Self>,
        path: &Path,
        change: &Change,
        pid: u32,
    ) -> io::Result<FileAttr> {
        change.validate()?;
        let path = relative(path)?;
        let parts = self.resolve(&path)?;
        if parts[0].metadata().is_file() && change.size.is_some() {
            let writers: Vec<_> = self
                .writers
                .lock()
                .get(&path)
                .into_iter()
                .flatten()
                .filter_map(std::sync::Weak::upgrade)
                .collect();
            if !writers.is_empty() {
                let mut attr = None;
                for writer in writers {
                    attr = Some(writer.set_attributes(change)?);
                }
                return Ok(attr.expect("nonempty writers"));
            }
            let file = self.open_file(&path, libc::O_RDWR, pid)?;
            let attr = file.set_attributes(change);
            let release = file.release();
            return match attr {
                Ok(attr) => {
                    release?;
                    Ok(attr)
                }
                Err(error) => Err(error),
            };
        }
        let last = parts.last().expect("nonempty resolution");
        let root = &self.layers[last.layer].root;
        if change.uid.is_some() || change.gid.is_some() {
            // Resolve the parent through the directory capability; chownat
            // with NOFOLLOW modifies a final symlink without traversing it.
            let parent = root.open_dir(
                path.parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or(Path::new(".")),
            )?;
            let name = path
                .file_name()
                .unwrap_or_else(|| std::ffi::OsStr::new("."));
            rustix::fs::chownat(
                &parent,
                name,
                change.uid.map(Uid::from_raw),
                change.gid.map(Gid::from_raw),
                AtFlags::SYMLINK_NOFOLLOW,
            )?;
        }
        if let Some(mode) = change.mode {
            root.set_permissions(&path, Permissions::from_mode(mode & 0o7777))?;
        }
        if change.atime.is_some() || change.mtime.is_some() {
            root.set_times(
                &path,
                change.atime.as_ref().map(spec),
                change.mtime.as_ref().map(spec),
            )?;
        }
        self.sniffs.clear();
        self.metadata(&path, pid)
    }
}
