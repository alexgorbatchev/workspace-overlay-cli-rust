//! Mount-table inspection in an owned subprocess, avoiding serving-process recursion.
use crate::{
    Cancellation, Error, Result,
    holders::{self, Process},
    subprocess,
};
use std::{
    ffi::{OsStr, OsString},
    os::unix::ffi::OsStringExt,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

pub const FILESYSTEM_TYPE: &str = "fuse.workspace-overlay";
/// Optional permission to terminate the particular retained processes holding a mount.
pub type Confirm<'a> = dyn Fn(&Path, &[Process]) -> bool + Send + Sync + 'a;

fn query(command: &mut Command, cancel: &Cancellation) -> Result<Vec<u8>> {
    match subprocess::output(command, cancel) {
        Err(Error::Command {
            status,
            stdout,
            stderr,
            ..
        }) if status.code() == Some(1) && stdout.is_empty() && stderr.is_empty() => Ok(Vec::new()),
        result => result,
    }
}
fn absolute(path: &Path) -> std::io::Result<PathBuf> {
    use path_clean::PathClean;
    Ok(if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    }
    .clean())
}
fn selected_type(path: &Path, selector: &str, cancel: &Cancellation) -> Result<Option<OsString>> {
    let path = absolute(path)?;
    let mut command = Command::new("findmnt");
    command
        .args([
            "--nocanonicalize",
            "--raw",
            "--noheadings",
            "--output",
            "FSTYPE",
            selector,
        ])
        .arg(&path);
    let data = query(&mut command, cancel)?;
    let data = data.trim_ascii();
    if data.is_empty() {
        Ok(None)
    } else if data.contains(&b'\n') {
        Err(Error::Invalid(
            "findmnt returned multiple filesystem types".into(),
        ))
    } else {
        Ok(Some(OsString::from_vec(decode(data)?)))
    }
}
/// Type mounted exactly at this lexical path, or `None` when it is not a mountpoint.
pub fn mounted_type(path: &Path, cancel: &Cancellation) -> Result<Option<OsString>> {
    selected_type(path, "--mountpoint", cancel)
}
/// Type containing this lexical path, without opening it in the serving process.
pub fn holding_type(path: &Path, cancel: &Cancellation) -> Result<Option<OsString>> {
    selected_type(path, "--target", cancel)
}
/// All active overlay targets, preserving native path bytes.
pub fn overlay_targets(cancel: &Cancellation) -> Result<Vec<PathBuf>> {
    let mut command = Command::new("findmnt");
    command.args([
        "--nocanonicalize",
        "--raw",
        "--noheadings",
        "--types",
        FILESYSTEM_TYPE,
        "--output",
        "TARGET",
    ]);
    query(&mut command, cancel)?
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            let path = PathBuf::from(OsString::from_vec(decode(line)?));
            if !path.is_absolute() {
                return Err(Error::Invalid(
                    "findmnt returned a relative mount target".into(),
                ));
            }
            Ok(path)
        })
        .collect()
}
fn decode(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut result = Vec::with_capacity(bytes.len());
    let mut cursor = 0;
    while cursor < bytes.len() {
        let byte = if bytes[cursor] == b'\\' {
            if bytes.get(cursor + 1) != Some(&b'x') || cursor + 3 >= bytes.len() {
                return Err(Error::Invalid("invalid findmnt raw escape".into()));
            }
            let digit = |byte: u8| match byte {
                b'0'..=b'9' => Some(byte - b'0'),
                b'a'..=b'f' => Some(byte - b'a' + 10),
                b'A'..=b'F' => Some(byte - b'A' + 10),
                _ => None,
            };
            let value = digit(bytes[cursor + 2])
                .zip(digit(bytes[cursor + 3]))
                .map(|(high, low)| high * 16 + low)
                .ok_or_else(|| Error::Invalid("invalid findmnt raw escape".into()))?;
            cursor += 4;
            value
        } else {
            let value = bytes[cursor];
            cursor += 1;
            value
        };
        if byte == 0 {
            return Err(Error::Invalid("findmnt returned a NUL byte".into()));
        }
        result.push(byte);
    }
    Ok(result)
}
/// An ordinary unmount failed while the named process handles retained the mount.
#[derive(Debug)]
pub struct BusyError {
    pub target: PathBuf,
    pub holders: Vec<Process>,
    pub error: Box<Error>,
}
impl std::fmt::Display for BusyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}; {} is in use by:", self.error, self.target.display())?;
        for process in &self.holders {
            write!(
                f,
                "\n  pid {}: {}",
                process.pid,
                process.command.to_string_lossy()
            )?;
            if let Some(path) = &process.directory {
                write!(f, " (in {})", path.display())?;
            }
        }
        Ok(())
    }
}
impl std::error::Error for BusyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&*self.error)
    }
}
pub(crate) fn release(
    target: &Path,
    confirm: Option<&Confirm<'_>>,
    cancel: &Cancellation,
    mut unmount: impl FnMut() -> Result<()>,
) -> Result<()> {
    let failure = match unmount() {
        Ok(()) => return Ok(()),
        Err(error) => error,
    };
    let found = match holders::find(target, cancel) {
        Ok(found) => found,
        Err(error) => return crate::failures(vec![failure, error]),
    };
    if found.is_empty() {
        return Err(failure);
    }
    if confirm.is_none_or(|confirm| !confirm(target, &found)) {
        return Err(BusyError {
            target: target.into(),
            holders: found,
            error: Box::new(failure),
        }
        .into());
    }
    let stopped = holders::stop(&found, Duration::from_secs(3), cancel);
    match unmount() {
        Ok(()) => Ok(()),
        Err(error) => crate::failures(std::iter::once(error).chain(stopped.err()).collect()),
    }
}
/// Ordinary, retryable unmount. Other filesystem types are always refused.
pub fn unmount(target: &Path, confirm: Option<&Confirm<'_>>, cancel: &Cancellation) -> Result<()> {
    match mounted_type(target, cancel)? {
        None => return Ok(()),
        Some(kind) if kind != OsStr::new(FILESYSTEM_TYPE) => {
            return Err(Error::Invalid(format!(
                "refusing to unmount {}: filesystem is {}",
                target.display(),
                kind.to_string_lossy()
            )));
        }
        _ => {}
    }
    let target = absolute(target)?;
    release(&target, confirm, cancel, || {
        let mut command = Command::new("fusermount3");
        command.args(["-u", "--"]).arg(&target);
        subprocess::output(&mut command, cancel).map(|_| ())
    })
}
