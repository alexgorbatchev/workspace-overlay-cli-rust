//! Human-only permission to terminate retained mount holders.
use cli_command_kit::{Mode, display_message, display_path};
use comfy_table::{ContentArrangement, Row, Table};
use rustix::event::{PollFd, PollFlags, poll};
use signal_hook::{
    consts::{SIGINT, SIGTERM},
    iterator::{backend::SignalDelivery, exfiltrator::SignalOnly},
};
use std::{
    io::{self, BufRead, IsTerminal, Read, Write},
    os::{fd::AsFd, unix::net::UnixStream},
    path::Path,
    sync::Mutex,
};
use workspace_overlay::{holders::Process, mounts::Confirm};

pub fn available(mode: Mode, input_terminal: bool, output_terminal: bool) -> bool {
    mode == Mode::Human && input_terminal && output_terminal
}

pub fn confirmation(mode: Mode) -> Option<Box<Confirm<'static>>> {
    if !available(mode, io::stdin().is_terminal(), io::stderr().is_terminal()) {
        return None;
    }
    let turn = Mutex::new(());
    Some(Box::new(move |target, holders| {
        let Ok(_turn) = turn.lock() else {
            return false;
        };
        let Ok(input) = SignalInput::new(io::stdin()) else {
            return false;
        };
        ask(
            target,
            holders,
            &mut io::BufReader::new(input),
            &mut io::stderr().lock(),
            cli_command_kit::terminal_width(),
        )
        .unwrap_or(false)
    }))
}

#[derive(Debug, thiserror::Error)]
#[error("confirmation interrupted by signal {0}")]
struct ShutdownSignal(i32);

/// Register independently for signals arriving after this prompt starts. The
/// first interrupt may already have requested session cleanup, which is why a
/// previously cancelled session token must not automatically reject a prompt.
struct SignalInput<R> {
    input: R,
    signals: SignalDelivery<UnixStream, SignalOnly>,
}
impl<R: AsFd> SignalInput<R> {
    fn new(input: R) -> io::Result<Self> {
        let (read, write) = UnixStream::pair()?;
        let signals = SignalDelivery::with_pipe(read, write, SignalOnly, [SIGINT, SIGTERM])?;
        Ok(Self { input, signals })
    }
}
impl<R: AsFd> Read for SignalInput<R> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        loop {
            let mut descriptors = [
                PollFd::new(&self.input, PollFlags::IN),
                PollFd::new(self.signals.get_read(), PollFlags::IN),
            ];
            match poll(&mut descriptors, None) {
                Err(rustix::io::Errno::INTR) => continue,
                result => {
                    result?;
                }
            }
            if let Some(signal) = self.signals.pending().next() {
                // BufRead::read_line retries ErrorKind::Interrupted. A typed
                // terminal cancellation must instead end the entire answer.
                return Err(io::Error::other(ShutdownSignal(signal)));
            }
            match rustix::io::read(self.input.as_fd(), &mut *bytes) {
                Err(rustix::io::Errno::INTR) => continue,
                result => return result.map_err(Into::into),
            }
        }
    }
}

pub fn ask(
    target: &Path,
    holders: &[Process],
    input: &mut impl BufRead,
    output: &mut impl Write,
    width: usize,
) -> io::Result<bool> {
    writeln!(
        output,
        "\n{} is in use and cannot be unmounted. These processes are using it:",
        display_path(target)
    )?;
    let mut table = Table::new();
    table
        .set_content_arrangement(ContentArrangement::Dynamic)
        .use_stderr()
        .set_header(["PID", "Command", "Directory"]);
    if let Ok(width) = u16::try_from(width)
        && width > 0
    {
        table.set_width(width);
    }
    for process in holders {
        let mut row = Row::from(vec![
            process.pid.to_string(),
            display_message(&process.command.to_string_lossy()),
            process
                .directory
                .as_deref()
                .map(display_path)
                .unwrap_or_default(),
        ]);
        row.max_height(1);
        table.add_row(row);
    }
    writeln!(output, "{table}")?;
    write!(output, "Stop these processes and unmount? [y/N] ")?;
    output.flush()?;
    let mut answer = String::new();
    input.read_line(&mut answer)?;
    if matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        return Ok(true);
    }
    writeln!(
        output,
        "{} stays mounted. Run `workspace-overlay overlay unmount` to be asked again.",
        display_path(target)
    )?;
    Ok(false)
}
