//! Native subprocess output with cancellation and deterministic child ownership.
use crate::{Cancellation, Error, Result};
use rustix::{
    event::{PollFd, PollFlags, poll},
    fs::{OFlags, fcntl_getfl, fcntl_setfl},
    time::Timespec,
};
use std::{
    io::{self, Read},
    os::unix::process::CommandExt,
    process::{Child, Command, Stdio},
    time::Duration,
};

struct Group {
    child: Child,
    complete: bool,
}
impl Group {
    fn kill(&mut self) -> io::Result<()> {
        let pid = i32::try_from(self.child.id())
            .map_err(|_| io::Error::from_raw_os_error(libc::EOVERFLOW))?;
        // SAFETY: Command::process_group(0) created this child's private group.
        // The child is retained until the kill/wait cleanup finishes.
        if unsafe { libc::kill(-pid, libc::SIGKILL) } < 0
            && io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
        {
            return Err(io::Error::last_os_error());
        }
        self.child.wait()?;
        self.complete = true;
        Ok(())
    }
}
impl Drop for Group {
    fn drop(&mut self) {
        if !self.complete {
            let _ = self.kill();
        }
    }
}

fn drain(reader: &mut impl Read, data: &mut Vec<u8>) -> io::Result<bool> {
    let mut buffer = [0; 16384];
    // Bound each pass so cancellation remains observable under continuous output.
    for _ in 0..64 {
        match reader.read(&mut buffer) {
            Ok(0) => return Ok(true),
            Ok(count) => data.extend_from_slice(&buffer[..count]),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}

/// Run an owned command in a private process group and return its raw stdout.
/// Nonzero status retains the native status and both raw streams in [`Error::Command`].
pub fn output(command: &mut Command, cancel: &Cancellation) -> Result<Vec<u8>> {
    cancel.check()?;
    let program = command.get_program().to_owned();
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut group = Group {
        child: command.spawn()?,
        complete: false,
    };
    let mut stdout = group.child.stdout.take().expect("piped stdout");
    let mut stderr = group.child.stderr.take().expect("piped stderr");
    fcntl_setfl(
        &stdout,
        fcntl_getfl(&stdout).map_err(io::Error::from)? | OFlags::NONBLOCK,
    )
    .map_err(io::Error::from)?;
    fcntl_setfl(
        &stderr,
        fcntl_getfl(&stderr).map_err(io::Error::from)? | OFlags::NONBLOCK,
    )
    .map_err(io::Error::from)?;
    let mut data = Vec::new();
    let mut diagnostic = Vec::new();
    let mut out_closed = false;
    let mut err_closed = false;
    loop {
        if !out_closed {
            out_closed = drain(&mut stdout, &mut data)?;
        }
        if !err_closed {
            err_closed = drain(&mut stderr, &mut diagnostic)?;
        }
        // Retain the unreaped leader while descendants hold its pipes. Its PID
        // reserves the private group ID until cancellation can safely signal it.
        if out_closed
            && err_closed
            && let Some(status) = group.child.try_wait()?
        {
            group.complete = true;
            return if status.success() {
                Ok(data)
            } else {
                Err(Error::Command {
                    program,
                    status,
                    stdout: data,
                    stderr: diagnostic,
                })
            };
        }
        if cancel.is_cancelled() {
            group.kill()?;
            return Err(Error::Cancelled);
        }
        if out_closed && err_closed {
            cancel.wait(Duration::from_millis(10));
            continue;
        }
        let mut descriptors = Vec::with_capacity(2);
        if !out_closed {
            descriptors.push(PollFd::new(&stdout, PollFlags::IN));
        }
        if !err_closed {
            descriptors.push(PollFd::new(&stderr, PollFlags::IN));
        }
        match poll(
            &mut descriptors,
            Some(&Timespec {
                tv_sec: 0,
                tv_nsec: 10_000_000,
            }),
        ) {
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => {}
            Err(error) => return Err(io::Error::from(error).into()),
        }
    }
}
