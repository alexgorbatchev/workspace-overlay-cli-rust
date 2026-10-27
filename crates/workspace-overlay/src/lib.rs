#![doc = include_str!("../README.md")]
mod attributes;
pub mod config;
mod file;
mod filesystem;
pub mod fixture;
pub mod markers;
mod mount;
mod mutations;
mod operations;
mod sniff;
pub mod view;
pub use mount::Mounted;
mod cancellation;
pub mod git;
pub mod gitexclude;
pub mod holders;
pub mod management;
mod metadata;
pub mod mounts;
mod notifications;
pub mod registry;
pub mod session;
pub mod subprocess;
pub use cancellation::Cancellation;
#[cfg(test)]
#[path = "../tests/unit/attributes.rs"]
mod attribute_tests;
#[cfg(test)]
#[path = "../tests/unit/file.rs"]
mod file_tests;
#[cfg(test)]
#[path = "../tests/unit/mutations.rs"]
mod mutation_tests;
#[cfg(test)]
#[path = "../tests/unit/notifications.rs"]
mod notification_tests;
#[cfg(test)]
#[path = "../tests/unit/operations.rs"]
mod operation_tests;
#[cfg(test)]
#[path = "../tests/unit/sniff.rs"]
mod sniff_tests;
#[cfg(test)]
#[path = "../tests/unit/view.rs"]
mod view_tests;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("operation cancelled")]
    Cancelled,
    #[error(transparent)]
    Busy(#[from] mounts::BusyError),
    #[error("{context}: {source}")]
    Context {
        context: String,
        #[source]
        source: Box<Error>,
    },
    #[error(transparent)]
    Failures(#[from] Failures),
    #[error("{program:?} failed ({status}): {}", String::from_utf8_lossy(stderr).trim())]
    Command {
        program: std::ffi::OsString,
        status: std::process::ExitStatus,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    },
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Toml(#[from] toml::de::Error),
    #[error(transparent)]
    Glob(#[from] globset::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Watch(#[from] notify::Error),
}
pub type Result<T> = std::result::Result<T, Error>;

/// Independent failures retained when cleanup must continue after an error.
#[derive(Debug)]
pub struct Failures(pub Vec<Error>);
impl std::fmt::Display for Failures {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (index, error) in self.0.iter().enumerate() {
            if index > 0 {
                write!(f, "; ")?;
            }
            write!(f, "{error}")?;
        }
        Ok(())
    }
}
impl std::error::Error for Failures {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.first().map(|error| error as _)
    }
}
pub(crate) fn failures(mut errors: Vec<Error>) -> Result<()> {
    match errors.len() {
        0 => Ok(()),
        1 => Err(errors.remove(0)),
        _ => Err(Failures(errors).into()),
    }
}
impl Error {
    pub(crate) fn context(self, context: impl Into<String>) -> Self {
        Self::Context {
            context: context.into(),
            source: Box::new(self),
        }
    }
}
