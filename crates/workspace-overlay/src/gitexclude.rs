//! Independently owned, advisory-locked Git info/exclude blocks.
use crate::{Error, Result};
use cap_std::fs::{Dir, OpenOptions, OpenOptionsExt};
use std::{
    ffi::OsStr,
    io::{self, Read, Seek, SeekFrom, Write},
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};

/// Escape a native relative pathname for a root-anchored Git ignore pattern.
pub fn pattern(path: &OsStr) -> Result<Vec<u8>> {
    let mut output = vec![b'/'];
    for &byte in path.as_bytes() {
        if matches!(byte, b'\n' | b'\r') {
            return Err(Error::Invalid(format!(
                "path {path:?} cannot be written as a Git exclude pattern"
            )));
        }
        if b"\\*?[]!# ".contains(&byte) {
            output.push(b'\\');
        }
        output.push(byte);
    }
    Ok(output)
}
fn marker() -> io::Result<Vec<u8>> {
    let mut random = [0; 32];
    getrandom::fill(&mut random).map_err(io::Error::other)?;
    let mut marker = b"workspace-overlay ".to_vec();
    for byte in random {
        marker.extend(format!("{byte:02x}").bytes());
    }
    Ok(marker)
}
/// A preopened directory capability; updates never recurse through a project mount.
type UpdateHook = Box<dyn Fn(&[u8]) -> Result<()> + Send + Sync>;
pub struct Exclude {
    root: Dir,
    path: PathBuf,
    marker: Vec<u8>,
    block: Vec<u8>,
    on_update: Option<UpdateHook>,
}
impl Exclude {
    /// Locate and preopen a checkout's shared exclusion file before mounting.
    pub fn open(project: &Path, cancel: &crate::Cancellation) -> Result<Option<Self>> {
        if !crate::git::has_metadata(project)? {
            return Ok(None);
        }
        let path = crate::git::metadata_path(
            project,
            &["--path-format=absolute", "--git-path", "info/exclude"],
            cancel,
        )?;
        std::fs::create_dir_all(
            path.parent()
                .ok_or_else(|| io::Error::from_raw_os_error(libc::EINVAL))?,
        )?;
        Ok(Some(Self::at(&path)?))
    }
    /// Open an already located info/exclude path before mounting the project.
    pub fn at(path: &Path) -> io::Result<Self> {
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EINVAL))?;
        Ok(Self {
            root: Dir::open_ambient_dir(parent, cap_std::ambient_authority())?,
            path: path.into(),
            marker: marker()?,
            block: vec![],
            on_update: None,
        })
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn block(&self) -> &[u8] {
        &self.block
    }
    pub fn fork(&self) -> io::Result<Self> {
        Ok(Self {
            root: self.root.try_clone()?,
            path: self.path.clone(),
            marker: marker()?,
            block: vec![],
            on_update: None,
        })
    }
    /// Observe each durable block change, for persistent session recovery records.
    pub fn on_update(&mut self, hook: impl Fn(&[u8]) -> Result<()> + Send + Sync + 'static) {
        self.on_update = Some(Box::new(hook));
    }
    pub fn update(&mut self, patterns: &[Vec<u8>]) -> Result<()> {
        let mut block = Vec::new();
        if !patterns.is_empty() {
            block.extend(b"\n# begin ");
            block.extend(&self.marker);
            block.push(b'\n');
            for pattern in patterns {
                if pattern.contains(&b'\n') || pattern.contains(&b'\r') {
                    return Err(Error::Invalid(
                        "invalid multiline Git exclusion pattern".into(),
                    ));
                }
                block.extend(pattern);
                block.push(b'\n');
            }
            block.extend(b"# end ");
            block.extend(&self.marker);
            block.push(b'\n');
        }
        if block == self.block {
            return Ok(());
        }
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .create(true)
            .mode(0o644)
            .custom_flags(libc::O_NOFOLLOW);
        let name = self
            .path
            .file_name()
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EINVAL))?;
        let mut file = self.root.open_with(name, &options)?.into_std();
        rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive)
            .map_err(io::Error::from)?;
        let mut data = Vec::new();
        file.read_to_end(&mut data)?;
        if !self.block.is_empty() {
            let mut matches = data
                .windows(self.block.len())
                .enumerate()
                .filter(|(_, bytes)| *bytes == self.block);
            let Some((start, _)) = matches.next() else {
                return Err(Error::Invalid(
                    "managed Git exclude block changed; refusing to overwrite it".into(),
                ));
            };
            if matches.next().is_some() {
                return Err(Error::Invalid(
                    "managed Git exclude block changed; refusing to overwrite it".into(),
                ));
            }
            data.drain(start..start + self.block.len());
        }
        data.extend(&block);
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&data)?;
        file.set_len(data.len() as u64)?;
        // Retain the exact written block even when sync reports uncertainty.
        self.block = block;
        file.sync_all()?;
        if let Some(hook) = &self.on_update {
            hook(&self.block)?;
        }
        Ok(())
    }
    pub fn close(&mut self) -> Result<()> {
        self.update(&[])
    }
    pub fn restore(path: &Path, block: &[u8]) -> Result<()> {
        let mut file = Self::at(path)?;
        file.block = block.into();
        file.close()
    }
}
