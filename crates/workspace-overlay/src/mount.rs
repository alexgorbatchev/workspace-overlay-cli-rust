//! Explicit, retryable ownership of a native FUSE mount.
use crate::{filesystem::Filesystem, view::View};
use std::{io, sync::Arc};

/// An active mount. Call [`Mounted::unmount`] to observe cleanup failures.
pub struct Mounted {
    server: Option<fuser::BackgroundSession>,
}
impl View {
    /// Mount over the project whose backing directory was opened by `View::open`.
    /// The serving process never accesses its own mounted pathname here.
    pub fn mount(self: &Arc<Self>) -> io::Result<Mounted> {
        let mut config = fuser::Config::default();
        config.mount_options = vec![
            fuser::MountOption::FSName("workspace-overlay".into()),
            fuser::MountOption::Subtype("workspace-overlay".into()),
            fuser::MountOption::DefaultPermissions,
        ];
        let filesystem = Filesystem::new(self.clone())?;
        let cache = filesystem.cache.clone();
        let session = fuser::Session::new(filesystem, &self.layers[0].path, &config)?;
        cache.bind(session.notifier());
        Ok(Mounted {
            server: Some(session.spawn()?),
        })
    }
}
impl Mounted {
    /// Ordinary unmount and server join. On failure the mount remains owned and retryable.
    pub fn unmount(&mut self) -> io::Result<()> {
        if let Some(server) = self.server.as_mut() {
            server.unmount()?;
        }
        if let Some(server) = self.server.take() {
            server.join()?;
        }
        Ok(())
    }
    /// Join a server after an external unmount. Blocks while its mount remains active.
    pub fn wait(&mut self) -> io::Result<()> {
        if let Some(server) = self.server.take() {
            server.join()?;
        }
        Ok(())
    }
    /// Whether the native serving thread has finished, including external unmount.
    pub fn is_finished(&self) -> bool {
        self.server
            .as_ref()
            .is_none_or(|server| server.guard.is_finished())
    }
}
