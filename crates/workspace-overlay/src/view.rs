//! Preopened source directories, visibility, and stable native identities.
use crate::{
    Result,
    config::{Selection, pattern},
    markers::{Markers, Section},
    sniff,
};
use cap_std::fs::{Dir, FileType, Metadata, MetadataExt};
use globset::GlobMatcher;
use parking_lot::{Mutex, RwLock};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    ffi::OsString,
    io,
    path::{Component, Path, PathBuf},
    sync::{
        Weak,
        atomic::{AtomicBool, Ordering},
    },
};

pub(crate) fn errno(code: i32) -> io::Error {
    io::Error::from_raw_os_error(code)
}
pub(crate) fn relative(path: &Path) -> io::Result<PathBuf> {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => result.push(name),
            Component::CurDir => {}
            _ => return Err(errno(libc::EPERM)),
        }
    }
    if result.as_os_str().is_empty() {
        result.push(".");
    }
    Ok(result)
}
pub(crate) fn git_metadata(path: &Path) -> bool {
    path.starts_with(".git")
}

pub(crate) struct Layer {
    pub(crate) name: String,
    pub(crate) path: PathBuf,
    pub(crate) root: Dir,
    matcher: Option<GlobMatcher>,
    glob: String,
    visible: RwLock<BTreeSet<PathBuf>>,
}
impl Layer {
    fn includes(&self, path: &Path) -> bool {
        self.matcher.is_none() || self.visible.read().contains(path)
    }
    fn refresh(&self) -> io::Result<()> {
        let Some(matcher) = &self.matcher else {
            return Ok(());
        };
        let mut visible = BTreeSet::from([PathBuf::from(".")]);
        let mut directories = vec![PathBuf::from(".")];
        while let Some(parent) = directories.pop() {
            for entry in self.root.read_dir(&parent)? {
                let entry = entry?;
                let child = relative(&parent.join(entry.file_name()))?;
                if git_metadata(&child) {
                    continue;
                }
                if entry.file_type()?.is_dir() {
                    directories.push(child.clone());
                }
                if matcher.is_match(&child) {
                    let mut name = child.as_path();
                    while !name.as_os_str().is_empty() {
                        visible.insert(name.into());
                        let Some(parent) = name.parent() else {
                            break;
                        };
                        name = parent;
                    }
                }
            }
        }
        *self.visible.write() = visible;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Contribution {
    pub(crate) layer: usize,
    pub(crate) info: Option<Metadata>,
}
impl Contribution {
    pub(crate) fn metadata(&self) -> &Metadata {
        self.info
            .as_ref()
            .expect("resolved contribution has native metadata")
    }
}
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub(crate) struct Identity {
    pub(crate) layer: usize,
    device: u64,
    inode: u64,
}
impl Contribution {
    pub(crate) fn identity(&self) -> Identity {
        let info = self.metadata();
        Identity {
            layer: self.layer,
            device: info.dev(),
            inode: info.ino(),
        }
    }
}

#[derive(Debug)]
pub struct DirectoryEntry {
    pub name: OsString,
    pub kind: FileType,
}
#[derive(Default)]
pub(crate) struct Records {
    pub(crate) created: HashSet<Identity>,
    pub(crate) vacated: HashMap<PathBuf, u32>,
}

/// A view owns native directory capabilities, so mounting over the original
/// project pathname never redirects source operations into its own FUSE mount.
pub struct View {
    pub(crate) layers: Vec<Layer>,
    pub(crate) markers: Markers,
    pub(crate) mutation: Mutex<()>,
    pub(crate) records: Mutex<Records>,
    identities: Mutex<HashMap<Identity, u64>>,
    pub(crate) sniffs: sniff::Cache,
    pub(crate) writers: Mutex<HashMap<PathBuf, Vec<Weak<crate::file::OpenFile>>>>,
    exclude: Mutex<Option<crate::gitexclude::Exclude>>,
    exclude_clean: AtomicBool,
    pub(crate) cache: RwLock<Weak<crate::metadata::Cache>>,
}
impl View {
    pub fn open(selection: &Selection) -> Result<Self> {
        let mut layers = vec![Layer {
            name: selection.project.clone(),
            path: selection.target.clone(),
            root: Dir::open_ambient_dir(&selection.target, cap_std::ambient_authority())?,
            matcher: None,
            glob: String::new(),
            visible: RwLock::new(BTreeSet::new()),
        }];
        for source in &selection.sources {
            layers.push(Layer {
                name: source.name.clone(),
                path: source.path.clone(),
                root: Dir::open_ambient_dir(&source.path, cap_std::ambient_authority())?,
                matcher: Some(pattern(&source.glob)?),
                glob: source.glob.clone(),
                visible: RwLock::new(BTreeSet::new()),
            });
        }
        let view = Self {
            layers,
            markers: Markers::new(&selection.markers)?,
            mutation: Mutex::new(()),
            records: Mutex::new(Records::default()),
            identities: Mutex::new(HashMap::new()),
            sniffs: sniff::Cache::new(),
            writers: Mutex::new(HashMap::new()),
            exclude: Mutex::new(None),
            exclude_clean: AtomicBool::new(false),
            cache: RwLock::new(Weak::new()),
        };
        view.refresh_paths()?;
        Ok(view)
    }
    pub fn refresh_paths(&self) -> io::Result<()> {
        self.sniffs.clear();
        if let Some(cache) = self.cache.read().upgrade() {
            cache.flush();
        }
        for layer in &self.layers {
            layer.refresh()?;
        }
        self.exclude_clean.store(false, Ordering::Release);
        self.refresh_exclude().map_err(io::Error::other)
    }
    pub(crate) fn changed(&self, layer: usize) -> io::Result<()> {
        self.sniffs.clear();
        if let Some(cache) = self.cache.read().upgrade() {
            cache.flush();
        }
        self.exclude_clean.store(false, Ordering::Release);
        if layer > 0 {
            self.refresh_paths()?;
        }
        Ok(())
    }
    /// Retrieve a watcher or invalidation failure. Failure disables metadata
    /// timeouts and flushes entries already published to the kernel.
    pub fn take_cache_warning(&self) -> Option<crate::Error> {
        self.cache
            .read()
            .upgrade()
            .and_then(|cache| cache.warning())
    }
    /// Attach the preopened exclusion file and its durable recovery hook.
    pub fn attach_exclude(
        &self,
        mut exclude: crate::gitexclude::Exclude,
        hook: impl Fn(&[u8]) -> Result<()> + Send + Sync + 'static,
    ) -> Result<()> {
        let mut attached = self.exclude.lock();
        if attached.is_some() {
            return Err(crate::Error::Invalid(
                "Git exclusions are already attached".into(),
            ));
        }
        exclude.on_update(hook);
        *attached = Some(exclude);
        drop(attached);
        self.exclude_clean.store(false, Ordering::Release);
        self.refresh_exclude()
    }
    /// Refresh only after a backing path change; no scan is needed for file data writes.
    pub fn refresh_exclude(&self) -> Result<()> {
        let mut attached = self.exclude.lock();
        let Some(exclude) = attached.as_mut() else {
            return Ok(());
        };
        if self.exclude_clean.load(Ordering::Acquire) {
            return Ok(());
        }
        let mut patterns = BTreeSet::new();
        for layer in &self.layers[1..] {
            let mut directories = vec![PathBuf::from(".")];
            while let Some(parent) = directories.pop() {
                for entry in layer.root.read_dir(&parent)? {
                    let entry = entry?;
                    let path = relative(&parent.join(entry.file_name()))?;
                    if git_metadata(&path) || !layer.includes(&path) {
                        continue;
                    }
                    match self.layers[0].root.symlink_metadata(&path) {
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {
                            patterns.insert(crate::gitexclude::pattern(path.as_os_str())?);
                        }
                        Err(error) => return Err(error.into()),
                        Ok(_) => {
                            if entry.file_type()?.is_dir() {
                                directories.push(path);
                            }
                        }
                    }
                }
            }
        }
        exclude.update(&patterns.into_iter().collect::<Vec<_>>())?;
        self.exclude_clean.store(true, Ordering::Release);
        Ok(())
    }
    /// Remove this view's block, retaining it for another attempt on error.
    pub fn close_exclude(&self) -> Result<()> {
        let mut attached = self.exclude.lock();
        if let Some(exclude) = attached.as_mut() {
            exclude.close()?;
        }
        attached.take();
        Ok(())
    }
    pub(crate) fn resolve(&self, path: &Path) -> io::Result<Vec<Contribution>> {
        let path = relative(path)?;
        let mut parts: Vec<Contribution> = Vec::new();
        for (index, layer) in self.layers.iter().enumerate() {
            if index > 0 && (git_metadata(&path) || !layer.includes(&path)) {
                continue;
            }
            let info = match layer.root.symlink_metadata(&path) {
                Ok(info) => info,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            if let Some(first) = parts.first()
                && (first.metadata().mode() & libc::S_IFMT != info.mode() & libc::S_IFMT
                    || !(info.is_file() || info.is_dir()))
            {
                return Err(errno(libc::EIO));
            }
            parts.push(Contribution {
                layer: index,
                info: Some(info),
            });
        }
        if parts.is_empty() {
            return Err(errno(libc::ENOENT));
        }
        Ok(parts)
    }
    pub(crate) fn stable(&self, part: &Contribution) -> u64 {
        let mut identities = self.identities.lock();
        let next = identities.len() as u64 + 2;
        *identities.entry(part.identity()).or_insert(next)
    }
    pub fn entries(&self, path: &Path) -> io::Result<Vec<DirectoryEntry>> {
        let path = relative(path)?;
        let parts = self.resolve(&path)?;
        if !parts[0].metadata().is_dir() {
            return Err(errno(libc::ENOTDIR));
        }
        let mut entries = BTreeMap::new();
        for part in parts {
            let layer = &self.layers[part.layer];
            for entry in layer.root.read_dir(&path)? {
                let entry = entry?;
                let name = entry.file_name();
                let child = relative(&path.join(&name))?;
                if part.layer > 0 && (git_metadata(&child) || !layer.includes(&child)) {
                    continue;
                }
                entries.insert(
                    name.clone(),
                    DirectoryEntry {
                        name,
                        kind: entry.file_type()?,
                    },
                );
            }
        }
        Ok(entries.into_values().collect())
    }
    pub(crate) fn destination(&self, path: &Path) -> io::Result<usize> {
        let path = relative(path)?;
        match self.resolve(&path) {
            Ok(parts) => return Ok(parts.last().expect("nonempty resolution").layer),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let parents = self.resolve(parent)?;
        if !parents[0].metadata().is_dir() {
            return Err(errno(libc::ENOTDIR));
        }
        if parents[0].layer == 0 {
            return Ok(0);
        }
        let index = parents.last().expect("nonempty resolution").layer;
        if self.layers[index]
            .matcher
            .as_ref()
            .is_some_and(|matcher| !matcher.is_match(path))
        {
            return Err(errno(libc::EPERM));
        }
        Ok(index)
    }
    pub fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
        let path = relative(path)?;
        let parts = self.resolve(&path)?;
        self.layers[parts.last().expect("nonempty resolution").layer]
            .root
            .read_link_contents(path)
    }
    pub(crate) fn document(&self, path: &Path, mut parts: Vec<Contribution>) -> Vec<Contribution> {
        let mut records = self.records.lock();
        if parts[0].layer == 0 {
            records.vacated.remove(path);
            return parts;
        }
        if parts.len() > 1 || records.vacated.contains_key(path) {
            parts.insert(
                0,
                Contribution {
                    layer: 0,
                    info: None,
                },
            );
        }
        parts
    }
    pub(crate) fn section(&self, path: &Path, part: &Contribution) -> Section {
        let layer = &self.layers[part.layer];
        let location = layer.path.join(path);
        Section::new(
            part.layer,
            &layer.name,
            pathdiff::diff_paths(&location, &self.layers[0].path).unwrap_or(location),
            b"",
        )
    }
    pub(crate) fn project_namespace(&self, path: &Path) -> bool {
        let Ok(path) = relative(path) else {
            return false;
        };
        let path = path.as_path();
        if git_metadata(path) {
            return true;
        }
        for layer in &self.layers[1..] {
            let glob = &layer.glob;
            let prefix = if let Some(at) = glob.find(['*', '?', '[', '{', '\\']) {
                let Some(slash) = glob[..at].rfind('/') else {
                    return false;
                };
                &glob[..slash]
            } else {
                glob
            };
            let prefix = Path::new(prefix);
            if path == Path::new(".")
                || prefix.as_os_str().is_empty()
                || path.starts_with(prefix)
                || prefix.starts_with(path)
            {
                return false;
            }
        }
        true
    }
    pub(crate) fn notice(
        &self,
        path: &Path,
        parts: &[Contribution],
    ) -> io::Result<Option<Vec<u8>>> {
        if parts.len() < 2 {
            return Ok(None);
        }
        let mut kinds = Vec::new();
        let mut any = false;
        for part in parts {
            let mut head = [0; 512];
            let count = self.sniffs.read(
                &self.layers[part.layer].root,
                part.layer,
                path,
                part.metadata(),
                &mut head,
                0,
            )?;
            let binary = sniff::binary(&head[..count]);
            any |= binary;
            kinds.push(binary);
        }
        if !any {
            return Ok(None);
        }
        let mut notice = format!(
            "workspace-overlay cannot show {:?}.\n\nThis path exists in more than one layer and at least one copy is a binary\nfile. Colliding text files are joined end to end, but binary files cannot be\njoined, so this path shows this message until the collision is resolved.\n\nCopies of this path:\n",
            path.as_os_str()
        );
        for (part, binary) in parts.iter().zip(kinds) {
            let location = self.layers[part.layer].path.join(path);
            // Display only: raw native paths remain authoritative in operations.
            notice.push_str(&format!(
                "  - {} ({}, {})\n",
                location.display(),
                if part.layer == 0 {
                    "project"
                } else {
                    "overlay"
                },
                if binary { "binary" } else { "text" }
            ));
        }
        notice.push_str("\nLimitations:\n  - No copy can be read or changed through this path while they collide.\n    Each copy is intact at the location listed above.\n  - A copy counts as binary when its first 512 bytes do not look like text.\n\nTo resolve:\n  - Keep one copy: delete or rename the others at the locations listed above.\n  - Or narrow the overlay's \"glob\" in workspace-overlay.toml so that it no\n    longer selects this path.\nThen open the file again.\n");
        Ok(Some(notice.into_bytes()))
    }
}

pub(crate) fn is_git_caller(pid: u32) -> bool {
    use std::io::Read;
    let Ok(mut file) = std::fs::File::open(format!("/proc/{pid}/comm")) else {
        return false;
    };
    let mut bytes = [0; 16];
    let mut count = 0;
    while count < bytes.len() {
        match file.read(&mut bytes[count..]) {
            Ok(0) => break,
            Ok(n) => count += n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return false,
        }
    }
    let comm = bytes[..count].trim_ascii();
    comm == b"git" || comm.starts_with(b"git-")
}
