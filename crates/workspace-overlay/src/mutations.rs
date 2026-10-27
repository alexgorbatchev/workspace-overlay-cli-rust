//! Source-preserving mutations and atomic editor-save splitting.
use crate::{
    file::{OpenFile, close_native, finish_io, open_native, read_bounded},
    markers::MAX_MERGED_SIZE,
    view::{Contribution, View, errno, is_git_caller, relative},
};
use cap_std::fs::{DirBuilder, DirBuilderExt, Metadata, MetadataExt};
use std::{
    io::{self, Write},
    path::Path,
    sync::Arc,
};

struct Save {
    original: Vec<u8>,
    earlier: Vec<(usize, Vec<u8>)>,
}

impl View {
    fn require_absent(&self, path: &Path) -> io::Result<()> {
        match self.resolve(path) {
            Ok(_) => Err(errno(libc::EEXIST)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
    fn remember(&self, layer: usize, path: &Path) -> io::Result<()> {
        if layer > 0 {
            let info = self.layers[layer].root.symlink_metadata(path)?;
            self.records.lock().created.insert(
                Contribution {
                    layer,
                    info: Some(info),
                }
                .identity(),
            );
        }
        Ok(())
    }
    pub(crate) fn create_file(
        self: &Arc<Self>,
        path: &Path,
        flags: i32,
        mode: u32,
        pid: u32,
    ) -> io::Result<Arc<OpenFile>> {
        let path = relative(path)?;
        let _mutation = self.mutation.lock();
        let layer = if is_git_caller(pid) {
            match self.layers[0].root.symlink_metadata(&path) {
                Ok(_) => return Err(errno(libc::EEXIST)),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            0
        } else {
            self.require_absent(&path)?;
            self.destination(&path)?
        };
        let file = open_native(
            self,
            layer,
            &path,
            flags | libc::O_CREAT | libc::O_EXCL,
            mode,
        )?;
        let info = Metadata::from_just_metadata(file.metadata()?);
        let ino = self.stable(&Contribution {
            layer,
            info: Some(info),
        });
        self.remember(layer, &path)?;
        self.changed(layer)?;
        Ok(Arc::new(OpenFile::new_native(
            self.clone(),
            path,
            file,
            ino,
            flags,
        )))
    }
    pub(crate) fn mkdir(&self, path: &Path, mode: u32) -> io::Result<()> {
        let path = relative(path)?;
        let _mutation = self.mutation.lock();
        self.require_absent(&path)?;
        let layer = self.destination(&path)?;
        let mut builder = DirBuilder::new();
        builder.mode(mode & 0o7777);
        self.layers[layer].root.create_dir_with(&path, &builder)?;
        self.remember(layer, &path)?;
        self.changed(layer)
    }
    pub(crate) fn symlink(&self, target: &Path, path: &Path) -> io::Result<()> {
        let path = relative(path)?;
        let _mutation = self.mutation.lock();
        self.require_absent(&path)?;
        let layer = self.destination(&path)?;
        // Contents are intentionally unconstrained; dereferencing through the
        // capability later still enforces the source directory boundary.
        self.layers[layer].root.symlink_contents(target, &path)?;
        self.remember(layer, &path)?;
        self.changed(layer)
    }
    pub(crate) fn unlink(&self, path: &Path) -> io::Result<()> {
        let path = relative(path)?;
        let _mutation = self.mutation.lock();
        let parts = self.resolve(&path)?;
        if parts[0].metadata().is_dir() {
            return Err(errno(libc::EISDIR));
        }
        if parts.len() > 1 && parts[0].layer == 0 {
            self.layers[0].root.remove_file(&path)?;
            if self.markers.find(&path).is_some() {
                self.records
                    .lock()
                    .vacated
                    .insert(path, parts[0].metadata().mode() & 0o7777);
            }
            return self.changed(0);
        }
        let last = parts.last().expect("nonempty resolution");
        let key = last.identity();
        if last.layer > 0 && (parts.len() != 1 || !self.records.lock().created.contains(&key)) {
            return Err(errno(libc::EPERM));
        }
        self.layers[last.layer].root.remove_file(&path)?;
        self.records.lock().created.remove(&key);
        self.changed(last.layer)
    }
    pub(crate) fn rmdir(&self, path: &Path) -> io::Result<()> {
        let path = relative(path)?;
        let _mutation = self.mutation.lock();
        if !self.entries(&path)?.is_empty() {
            return Err(errno(libc::ENOTEMPTY));
        }
        let parts = self.resolve(&path)?;
        let first = &parts[0];
        let key = first.identity();
        if parts.len() != 1 || (first.layer > 0 && !self.records.lock().created.contains(&key)) {
            return Err(errno(libc::EPERM));
        }
        self.layers[first.layer].root.remove_dir(&path)?;
        self.records.lock().created.remove(&key);
        self.changed(first.layer)
    }
    pub(crate) fn hard_link(&self, source: &Path, destination: &Path) -> io::Result<()> {
        let source = relative(source)?;
        let destination = relative(destination)?;
        let _mutation = self.mutation.lock();
        let parts = self.resolve(&source)?;
        if parts.len() != 1 {
            return Err(errno(libc::EXDEV));
        }
        // Never replace an overlay-only name hidden in the same project
        // namespace; the native operation must agree with the visible view.
        self.require_absent(&destination)?;
        let root = &self.layers[parts[0].layer].root;
        root.hard_link(&source, root, &destination)?;
        self.changed(parts[0].layer)
    }
    fn prepare_save(
        &self,
        layer: usize,
        source: &Path,
        destination: &Path,
    ) -> io::Result<Option<Save>> {
        let mut parts = match self.resolve(destination) {
            Ok(parts) => parts,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        if !parts[0].metadata().is_file() {
            return Ok(None);
        }
        let rule = self.markers.find(destination);
        if rule.is_some() {
            parts = self.document(destination, parts);
        }
        if parts.len() == 1 {
            return Ok(None);
        }
        let real: Vec<_> = parts.iter().filter(|p| p.info.is_some()).cloned().collect();
        if self.notice(destination, &real)?.is_some() {
            return Err(errno(libc::EPERM));
        }
        let original = read_bounded(open_native(self, layer, source, libc::O_RDONLY, 0)?)?;
        let (earlier, data) = if let Some(rule) = rule {
            let sections: Vec<_> = parts
                .iter()
                .map(|part| self.section(destination, part))
                .collect();
            let mut parsed = rule.parse(&original, &sections)?;
            let (_, last) = parsed.pop().expect("nonempty document");
            (parsed, last)
        } else {
            let mut prefix = Vec::new();
            for part in &parts[..parts.len() - 1] {
                let data = read_bounded(open_native(
                    self,
                    part.layer,
                    destination,
                    libc::O_RDONLY,
                    0,
                )?)?;
                if prefix.len() + data.len() > MAX_MERGED_SIZE {
                    return Err(errno(libc::EFBIG));
                }
                prefix.extend(data);
            }
            (
                Vec::new(),
                original
                    .strip_prefix(prefix.as_slice())
                    .ok_or_else(|| errno(libc::EPERM))?
                    .to_vec(),
            )
        };
        let mut file = open_native(self, layer, source, libc::O_WRONLY | libc::O_TRUNC, 0)?;
        let written = file.write_all(&data);
        let prepared = finish_io(written, close_native(file));
        if let Err(error) = prepared {
            let restored = (|| {
                let mut file = open_native(self, layer, source, libc::O_WRONLY | libc::O_TRUNC, 0)?;
                let written = file.write_all(&original);
                finish_io(written, close_native(file))
            })();
            finish_io(Err(error), restored)?;
        }
        Ok(Some(Save { original, earlier }))
    }
    pub(crate) fn rename(&self, source: &Path, destination: &Path, flags: u32) -> io::Result<()> {
        if flags != 0 {
            return Err(errno(libc::ENOTSUP));
        }
        let source = relative(source)?;
        let destination = relative(destination)?;
        if source == destination {
            return Ok(());
        }
        let _mutation = self.mutation.lock();
        let parts = self.resolve(&source)?;
        if parts[0].metadata().is_dir() && parts.len() > 1 {
            return Err(errno(libc::EXDEV));
        }
        let last = parts.last().expect("nonempty resolution");
        let key = last.identity();
        let destination_existed = match self.resolve(&destination) {
            Ok(dest) => {
                if last.layer > 0
                    && (parts[0].metadata().is_dir()
                        || !dest[0].metadata().is_file()
                        || dest.last().expect("nonempty destination").layer == 0)
                {
                    return Err(errno(libc::EPERM));
                }
                true
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(error) => return Err(error),
        };
        if last.layer > 0 && !self.records.lock().created.contains(&key) {
            return Err(errno(libc::EPERM));
        }
        let dest_layer = self.destination(&destination)?;
        let save = self.prepare_save(last.layer, &source, &destination)?;
        if let Err(error) = self.layers[last.layer].root.rename(
            &source,
            &self.layers[dest_layer].root,
            &destination,
        ) {
            if let Some(save) = save {
                // Preserve the full staged document if the atomic native
                // rename fails after preparing its final contribution.
                let restored = (|| {
                    let mut file =
                        open_native(self, last.layer, &source, libc::O_WRONLY | libc::O_TRUNC, 0)?;
                    let written = file.write_all(&save.original);
                    finish_io(written, close_native(file))
                })();
                return finish_io(Err(error), restored);
            }
            return Err(error);
        }
        if let Some(save) = save {
            self.save_sections_locked(&destination, &save.earlier)?;
        }
        let mut records = self.records.lock();
        records.created.remove(&key);
        if last.layer > 0 && !destination_existed && dest_layer > 0 {
            let mut moved = key;
            moved.layer = dest_layer;
            records.created.insert(moved);
        }
        drop(records);
        self.changed(last.layer.max(dest_layer))
    }
}
