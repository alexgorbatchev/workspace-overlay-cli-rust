//! Persistent ownership records. Locks are removed while still held.
use crate::{Cancellation, Error, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum Native {
    Text(String),
    Bytes(Vec<u8>),
}
mod native_path {
    use super::*;
    pub(super) fn serialize<S: serde::Serializer>(
        value: &Path,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        match value.to_str() {
            Some(text) => serializer.serialize_str(text),
            None => serializer.serialize_bytes(value.as_os_str().as_bytes()),
        }
    }
    pub(super) fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<PathBuf, D::Error> {
        Ok(match Native::deserialize(deserializer)? {
            Native::Text(text) => text.into(),
            Native::Bytes(bytes) => OsString::from_vec(bytes).into(),
        })
    }
}
mod optional_path {
    use super::*;
    pub(super) fn serialize<S: serde::Serializer>(
        value: &Option<PathBuf>,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        match value {
            None => serializer.serialize_none(),
            Some(path) => match path.to_str() {
                Some(text) => serializer.serialize_some(text),
                None => serializer.serialize_some(path.as_os_str().as_bytes()),
            },
        }
    }
    pub(super) fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Option<PathBuf>, D::Error> {
        Ok(
            Option::<Native>::deserialize(deserializer)?.map(|value| match value {
                Native::Text(text) => text.into(),
                Native::Bytes(bytes) => OsString::from_vec(bytes).into(),
            }),
        )
    }
}
mod native_bytes {
    use super::*;
    pub(super) fn serialize<S: serde::Serializer>(
        value: &[u8],
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        match std::str::from_utf8(value) {
            Ok(text) => serializer.serialize_str(text),
            Err(_) => serializer.serialize_bytes(value),
        }
    }
    pub(super) fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Vec<u8>, D::Error> {
        Ok(match Native::deserialize(deserializer)? {
            Native::Text(text) => text.into_bytes(),
            Native::Bytes(bytes) => bytes,
        })
    }
}

/// One mounted target and its independently owned Git exclusion block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountRecord {
    #[serde(with = "native_path")]
    pub target: PathBuf,
    #[serde(
        default,
        with = "optional_path",
        skip_serializing_if = "Option::is_none"
    )]
    pub exclude: Option<PathBuf>,
    #[serde(default, with = "native_bytes", skip_serializing_if = "Vec::is_empty")]
    pub block: Vec<u8>,
}
/// Versioned recovery state for one configured project.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    pub version: u32,
    #[serde(with = "native_path")]
    pub root: PathBuf,
    pub project: String,
    pub mounts: Vec<MountRecord>,
}
impl State {
    fn validate(&self, root: &Path, project: &str) -> Result<()> {
        if self.version != 1
            || self.root != root
            || self.project != project
            || !root.is_absolute()
            || self.mounts.iter().any(|mount| {
                !mount.target.is_absolute()
                    || mount.exclude.as_ref().is_some_and(|p| !p.is_absolute())
            })
        {
            return Err(Error::Invalid(
                "invalid mount registry identity or paths".into(),
            ));
        }
        Ok(())
    }
}
/// A state directory, explicit for embedding and test isolation.
#[derive(Clone)]
pub struct Store {
    directory: PathBuf,
}
impl Store {
    pub fn new(directory: PathBuf) -> io::Result<Self> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&directory)?;
        Ok(Self {
            directory: fs::canonicalize(directory)?,
        })
    }
    pub fn from_environment() -> Result<Self> {
        let directory = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| dirs::home_dir().map(|home| home.join(".local/state")))
            .ok_or_else(|| Error::Invalid("cannot locate state directory".into()))?;
        Ok(Self::new(directory.join("workspace-overlay"))?)
    }
    pub fn directory(&self) -> &Path {
        &self.directory
    }
    fn file(&self, root: &Path, project: &str) -> PathBuf {
        let mut digest = Sha256::new();
        digest.update(root.as_os_str().as_bytes());
        digest.update([0]);
        digest.update(project.as_bytes());
        self.directory.join(format!("{:x}.json", digest.finalize()))
    }
    pub fn read(&self, root: &Path, project: &str) -> Result<Option<State>> {
        let data = match fs::read(self.file(root, project)) {
            Ok(data) => data,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let state: State = serde_json::from_slice(&data)?;
        state.validate(root, project)?;
        Ok(Some(state))
    }
    pub fn acquire(&self, root: &Path, project: &str) -> Result<Owner> {
        if !root.is_absolute() {
            return Err(Error::Invalid("registry root must be absolute".into()));
        }
        let file = self.file(root, project);
        let lock_path = file.with_extension("json.lock");
        loop {
            let lock = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&lock_path)?;
            rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive)
                .map_err(io::Error::from)?;
            let held = lock.metadata()?;
            let found = match fs::symlink_metadata(&lock_path) {
                Ok(found) => found,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            if held.dev() != found.dev() || held.ino() != found.ino() {
                continue;
            }
            return Ok(Owner {
                stop: file.with_extension("stop"),
                file,
                lock_path,
                lock: Some(lock),
                state: State {
                    version: 1,
                    root: root.into(),
                    project: project.into(),
                    mounts: vec![],
                },
            });
        }
    }
    pub fn open(&self, root: &Path, project: &str) -> Result<Owner> {
        let mut owner = self.acquire(root, project)?;
        match fs::symlink_metadata(&owner.file) {
            Ok(_) => {
                owner.unlock()?;
                return Err(Error::Invalid(format!(
                    "project {project} has an unfinished registry; use overlay unmount or mount --replace to recover"
                )));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                owner.unlock()?;
                return Err(error.into());
            }
        }
        remove(&owner.stop)?;
        Ok(owner)
    }
    /// Ask a live owner to release its mounts, waiting at most three seconds.
    /// `false` means the registry needs explicit recovery. A cancelled request
    /// leaves an already published stop request in place for its owner.
    pub fn request_stop(&self, root: &Path, project: &str, cancel: &Cancellation) -> Result<bool> {
        use std::time::{Duration, Instant};
        cancel.check()?;
        if self.read(root, project)?.is_none() {
            return Ok(true);
        }
        match self.acquire(root, project) {
            Ok(mut abandoned) => {
                abandoned.unlock()?;
                return Ok(false);
            }
            Err(Error::Io(error)) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error),
        }
        cancel.check()?;
        let record = self.file(root, project);
        let stop = record.with_extension("stop");
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&stop)?;
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match fs::symlink_metadata(&record) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    remove(&stop)?;
                    return Ok(true);
                }
                Err(error) => return Err(error.into()),
                Ok(_) => {}
            }
            cancel.check()?;
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(false);
            }
            cancel.wait(remaining.min(Duration::from_millis(25)));
        }
    }
}
/// A process-owned registry lock. Explicit close reports incomplete cleanup.
pub struct Owner {
    file: PathBuf,
    stop: PathBuf,
    lock_path: PathBuf,
    lock: Option<File>,
    state: State,
}
impl Owner {
    pub fn file(&self) -> &Path {
        &self.file
    }
    pub fn stop_file(&self) -> &Path {
        &self.stop
    }
    pub fn state(&self) -> &State {
        &self.state
    }
    fn save(&self) -> Result<()> {
        if self.lock.is_none() {
            return Err(io::Error::from_raw_os_error(libc::EBADF).into());
        }
        let data = serde_json::to_vec(&self.state)?;
        let next = self.file.with_extension("json.next");
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&next)?;
        file.write_all(&data)?;
        file.sync_all()?;
        crate::file::close_native(file)?;
        fs::rename(next, &self.file)?;
        // Persist the rename as well as the file contents for crash recovery.
        File::open(self.file.parent().expect("registry directory"))?.sync_all()?;
        Ok(())
    }
    pub fn record(&mut self, mount: MountRecord) -> Result<()> {
        if !mount.target.is_absolute() || mount.exclude.as_ref().is_some_and(|p| !p.is_absolute()) {
            return Err(Error::Invalid("registry paths must be absolute".into()));
        }
        if let Some(existing) = self
            .state
            .mounts
            .iter_mut()
            .find(|existing| existing.target == mount.target)
        {
            *existing = mount;
        } else {
            self.state.mounts.push(mount);
            self.state.mounts.sort_by(|a, b| a.target.cmp(&b.target));
        }
        self.save()
    }
    pub fn forget(&mut self, target: &Path) -> Result<()> {
        self.state.mounts.retain(|mount| mount.target != target);
        self.save()
    }
    pub fn adopt(&mut self, state: State) -> Result<()> {
        state.validate(&self.state.root, &self.state.project)?;
        self.state = state;
        Ok(())
    }
    pub fn discard(&mut self) -> Result<()> {
        let errors = [
            &self.file,
            &self.stop,
            &self.file.with_extension("json.next"),
        ]
        .into_iter()
        .filter_map(|path| remove(path).err().map(Error::from))
        .collect();
        crate::failures(errors)
    }
    pub fn unlock(&mut self) -> io::Result<()> {
        if let Some(lock) = self.lock.take() {
            let removed = remove(&self.lock_path);
            drop(lock);
            removed?;
        }
        Ok(())
    }
    pub fn close(&mut self) -> Result<()> {
        if !self.state.mounts.is_empty() {
            let mut errors = vec![Error::Invalid(
                "cleanup incomplete; mount registry retained for overlay unmount recovery".into(),
            )];
            errors.extend(self.unlock().err().map(Error::from));
            return crate::failures(errors);
        }
        let discarded = self.discard();
        let unlocked = self.unlock();
        crate::failures(
            discarded
                .err()
                .into_iter()
                .chain(unlocked.err().map(Error::from))
                .collect(),
        )
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        let _ = self.unlock();
    }
}
pub(crate) fn remove(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}
