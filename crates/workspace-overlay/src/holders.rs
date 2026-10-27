//! Mount holders with retained Linux pidfds, safe against PID reuse during confirmation.
use crate::{Cancellation, Error, Result, mounts, subprocess};
use rustix::{
    event::{PollFd, PollFlags, poll},
    process::{Pid, PidfdFlags, Signal, pidfd_open, pidfd_send_signal},
    time::Timespec,
};
use std::{
    ffi::OsString,
    fs, io,
    os::{fd::OwnedFd, unix::ffi::OsStringExt},
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

#[derive(Debug)]
pub struct Process {
    pub pid: u32,
    pub command: OsString,
    pub directory: Option<PathBuf>,
    handle: OwnedFd,
}
impl Process {
    /// Acquire the exact process before reading `/proc`; exited processes return `None`.
    pub fn open(pid: u32) -> io::Result<Option<Self>> {
        let raw = i32::try_from(pid).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
        let native =
            Pid::from_raw(raw).ok_or_else(|| io::Error::from_raw_os_error(libc::EINVAL))?;
        let handle = match pidfd_open(native, PidfdFlags::empty()) {
            Ok(handle) => handle,
            Err(rustix::io::Errno::SRCH) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let mut process = Self {
            pid,
            command: OsString::new(),
            directory: None,
            handle,
        };
        if process.exited()? {
            return Ok(None);
        }
        let path = PathBuf::from(format!("/proc/{pid}"));
        let command = match fs::read(path.join("cmdline")) {
            Ok(data) => data,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let command = command
            .into_iter()
            .map(|byte| if byte == 0 { b' ' } else { byte })
            .collect::<Vec<_>>();
        process.command = OsString::from_vec(if command.trim_ascii().is_empty() {
            fs::read(path.join("comm"))?.trim_ascii().to_owned()
        } else {
            command.trim_ascii().to_owned()
        });
        process.directory = fs::read_link(path.join("cwd")).ok();
        Ok(if process.exited()? {
            None
        } else {
            Some(process)
        })
    }
    fn exited(&self) -> io::Result<bool> {
        let mut descriptors = [PollFd::new(&self.handle, PollFlags::IN)];
        loop {
            match poll(
                &mut descriptors,
                Some(&Timespec {
                    tv_sec: 0,
                    tv_nsec: 0,
                }),
            ) {
                Ok(_) => {
                    let events = descriptors[0].revents();
                    return if events.intersects(PollFlags::NVAL | PollFlags::ERR) {
                        Err(io::Error::from_raw_os_error(libc::EBADF))
                    } else {
                        Ok(events.intersects(PollFlags::IN | PollFlags::HUP))
                    };
                }
                Err(rustix::io::Errno::INTR) => continue,
                Err(error) => return Err(error.into()),
            }
        }
    }
    fn signal(&self, signal: Signal) -> Result<()> {
        match pidfd_send_signal(&self.handle, signal) {
            Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
            Err(error) => {
                Err(Error::from(io::Error::from(error))
                    .context(format!("signal process {}", self.pid)))
            }
        }
    }
}
/// Processes using an overlay mounted exactly at `target`, checked before and after discovery.
pub fn find(target: &Path, cancel: &Cancellation) -> Result<Vec<Process>> {
    if mounts::mounted_type(target, cancel)?.as_deref()
        != Some(std::ffi::OsStr::new(mounts::FILESYSTEM_TYPE))
    {
        return Ok(Vec::new());
    }
    let mut command = Command::new("fuser");
    command.args(["--ismountpoint", "--mount"]).arg(target);
    let data = match subprocess::output(&mut command, cancel) {
        Ok(data) => data,
        Err(Error::Command { status, stdout, .. })
            if status.code() == Some(1) && stdout.trim_ascii().is_empty() =>
        {
            Vec::new()
        }
        Err(error) => {
            return Err(error.context(format!(
                "list processes using {}; fuser must be installed",
                target.display()
            )));
        }
    };
    let mut found = Vec::new();
    for field in data
        .split(u8::is_ascii_whitespace)
        .filter(|field| !field.is_empty())
    {
        let pid = std::str::from_utf8(field)
            .ok()
            .and_then(|field| field.parse::<u32>().ok())
            .filter(|pid| *pid > 0)
            .ok_or_else(|| {
                Error::Invalid(format!(
                    "unexpected fuser process ID {:?}",
                    String::from_utf8_lossy(field)
                ))
            })?;
        if pid != std::process::id()
            && !found.iter().any(|process: &Process| process.pid == pid)
            && let Some(process) = Process::open(pid)?
        {
            found.push(process);
        }
    }
    if mounts::mounted_type(target, cancel)?.as_deref()
        != Some(std::ffi::OsStr::new(mounts::FILESYSTEM_TYPE))
    {
        found.clear();
    }
    Ok(found)
}
fn await_exit<'a>(
    processes: Vec<&'a Process>,
    deadline: Instant,
    cancel: &Cancellation,
    errors: &mut Vec<Error>,
) -> Vec<&'a Process> {
    let mut running = processes;
    loop {
        running.retain(|process| match process.exited() {
            Ok(exited) => !exited,
            Err(error) => {
                errors.push(Error::from(error).context(format!("inspect process {}", process.pid)));
                false
            }
        });
        if running.is_empty() || cancel.is_cancelled() || Instant::now() >= deadline {
            return running;
        }
        cancel.wait(
            deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(50)),
        );
    }
}
/// TERM, bounded grace, KILL, bounded grace. Zombies count as exited; cancellation still kills retained handles.
pub fn stop(processes: &[Process], grace: Duration, cancel: &Cancellation) -> Result<()> {
    let deadline = Instant::now().checked_add(grace).ok_or_else(|| {
        Error::Invalid("process grace period exceeds the native clock range".into())
    })?;
    let mut errors = Vec::new();
    let mut running = Vec::new();
    for process in processes {
        match process.signal(Signal::TERM) {
            Ok(()) => running.push(process),
            Err(error) => errors.push(error),
        }
    }
    let running = await_exit(running, deadline, cancel, &mut errors);
    for process in &running {
        if let Err(error) = process.signal(Signal::KILL) {
            errors.push(error);
        }
    }
    // Cancellation ends the grace period, not the obligation to observe forced cleanup.
    let cleanup = Cancellation::default();
    let forced_deadline = Instant::now().checked_add(grace).unwrap_or(deadline);
    for process in await_exit(running, forced_deadline, &cleanup, &mut errors) {
        errors.push(Error::Invalid(format!(
            "process {} is still running",
            process.pid
        )));
    }
    if cancel.is_cancelled() {
        errors.push(Error::Cancelled);
    }
    crate::failures(errors)
}
