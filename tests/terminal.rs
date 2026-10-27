use std::{
    fs::File,
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::process::ExitStatusExt,
    },
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Terminal {
    child: Child,
    master: File,
}
impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Terminal {
    fn spawn() -> Self {
        let mut master = -1;
        let mut slave = -1;
        let size = libc::winsize {
            ws_row: 24,
            ws_col: 60,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: openpty initializes both descriptor outputs and reads a valid size.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &size,
                )
            },
            0
        );
        // SAFETY: each descriptor was freshly initialized and has one owner.
        let master = unsafe { File::from_raw_fd(master) };
        let slave = unsafe { File::from_raw_fd(slave) };
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "confirmation_terminal_helper",
                "--ignored",
                "--nocapture",
            ])
            .stdin(slave.try_clone().unwrap())
            .stderr(slave)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        Self { child, master }
    }
    fn prompt(&mut self) -> String {
        let until = Instant::now() + Duration::from_secs(5);
        let mut shown = Vec::new();
        while Instant::now() < until {
            let mut poll = libc::pollfd {
                fd: self.master.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: poll has exactly one valid initialized element.
            if unsafe { libc::poll(&mut poll, 1, 100) } <= 0 {
                continue;
            }
            let mut bytes = [0; 4096];
            let count = self.master.read(&mut bytes).unwrap();
            shown.extend_from_slice(&bytes[..count]);
            if shown.ends_with(b"[y/N] ") {
                return String::from_utf8(shown).unwrap();
            }
        }
        panic!(
            "prompt was not printed: {}",
            String::from_utf8_lossy(&shown)
        );
    }
    fn success(&mut self) {
        let until = Instant::now() + Duration::from_secs(3);
        while Instant::now() < until {
            if let Some(status) = self.child.try_wait().unwrap() {
                let mut output = String::new();
                self.child
                    .stdout
                    .take()
                    .unwrap()
                    .read_to_string(&mut output)
                    .unwrap();
                assert!(
                    status.success(),
                    "{status:?} signal={:?}: {output}",
                    status.signal()
                );
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("confirmation remained blocked after terminal input or shutdown signal");
    }
}

#[test]
fn native_terminal_confirmation_accepts_input_and_shutdown_signals_interrupt_waiting() {
    let mut terminal = Terminal::spawn();
    assert!(terminal.prompt().contains("in use"));
    terminal.master.write_all(b"no\n").unwrap();
    terminal.success();
    for signal in [libc::SIGINT, libc::SIGTERM] {
        let mut terminal = Terminal::spawn();
        terminal.prompt();
        // SAFETY: the child is owned, live and waiting for input.
        assert_eq!(unsafe { libc::kill(terminal.child.id() as i32, signal) }, 0);
        terminal.success();
    }
}

#[test]
#[ignore = "real PTY child invoked by native terminal test"]
fn confirmation_terminal_helper() {
    let cancel = workspace_overlay::Cancellation::default();
    let mut signals = workspace_overlay_cli::signals::SignalsGuard::new(cancel).unwrap();
    let confirmation =
        workspace_overlay_cli::prompt::confirmation(cli_command_kit::Mode::Human).unwrap();
    assert!(!confirmation(std::path::Path::new("/srv/alpha"), &[]));
    signals.close().unwrap();
    io::stdout().flush().unwrap();
}
