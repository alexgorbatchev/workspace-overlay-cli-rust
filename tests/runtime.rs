use std::{
    fs,
    io::{BufRead, BufReader, Read},
    os::{fd::OwnedFd, unix::net::UnixStream},
    path::PathBuf,
    process::{Child, Command, Output, Stdio},
    time::{Duration, Instant},
};
use workspace_overlay::{Cancellation, config::Configuration, management, registry::Store};

const WAIT: Duration = Duration::from_secs(5);
struct Workspace {
    root: PathBuf,
    _temp: tempfile::TempDir,
    owner: Option<Child>,
    holder: Option<Child>,
}
impl Workspace {
    fn new() -> Self {
        fs::create_dir_all(".tmp").unwrap();
        let temp = tempfile::tempdir_in(".tmp").unwrap();
        let root = fs::canonicalize(temp.path()).unwrap().join("workspace");
        workspace_overlay::fixture::create(&root, &Cancellation::default()).unwrap();
        Self {
            root,
            _temp: temp,
            owner: None,
            holder: None,
        }
    }
    fn command(&self, action: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_workspace-overlay"));
        command
            .env("AGENT", "1")
            .env("XDG_STATE_HOME", self.root.join("state"))
            .args(["overlay", action, "--config"])
            .arg(self.root.join("workspace-overlay.toml"));
        command
    }
    fn output(&self, action: &str, more: &[&str]) -> Output {
        self.command(action).args(more).output().unwrap()
    }
    fn mount(&mut self, more: &[&str], expected: usize) -> BufReader<UnixStream> {
        let (reader, writer) = UnixStream::pair().unwrap();
        reader.set_read_timeout(Some(WAIT)).unwrap();
        self.owner = Some(
            self.command("mount")
                .args(more)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::from(OwnedFd::from(writer)))
                .spawn()
                .unwrap(),
        );
        let mut events = BufReader::new(reader);
        let mut mounted = 0;
        while mounted < expected {
            let mut line = String::new();
            assert!(
                events.read_line(&mut line).unwrap() > 0,
                "owner exited before mount"
            );
            assert!(!line.starts_with("ERR:"), "{line}");
            if line.starts_with("Mounted ") {
                mounted += 1;
            }
        }
        events
    }
    fn wait_owner(&mut self) -> std::process::ExitStatus {
        let until = Instant::now() + WAIT;
        while Instant::now() < until {
            if let Some(status) = self.owner.as_mut().unwrap().try_wait().unwrap() {
                self.owner.take();
                return status;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("owner did not stop");
    }
    fn signal(&self, signal: i32) {
        // SAFETY: the owner is a live child retained by this guard.
        assert_eq!(
            unsafe { libc::kill(self.owner.as_ref().unwrap().id() as i32, signal) },
            0
        );
    }
    fn hold(&mut self) {
        let (reader, writer) = UnixStream::pair().unwrap();
        reader.set_read_timeout(Some(WAIT)).unwrap();
        self.holder = Some(
            Command::new("sh")
                .args(["-c", "printf 'ready\\n'; read value"])
                .current_dir(self.root.join("alpha"))
                .stdin(Stdio::piped())
                .stdout(Stdio::from(OwnedFd::from(writer)))
                .spawn()
                .unwrap(),
        );
        let mut ready = String::new();
        BufReader::new(reader).read_line(&mut ready).unwrap();
        assert_eq!(ready, "ready\n");
    }
}
impl Drop for Workspace {
    fn drop(&mut self) {
        if let Some(mut child) = self.holder.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(child) = self.owner.as_mut() {
            // SAFETY: this is our retained child PID, not an external process.
            unsafe {
                libc::kill(child.id() as i32, libc::SIGTERM);
            }
            let until = Instant::now() + WAIT;
            while child.try_wait().ok().flatten().is_none() && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(10));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        let config =
            Configuration::discover(Some(&self.root.join("workspace-overlay.toml"))).unwrap();
        let store = Store::new(self.root.join("state/workspace-overlay")).unwrap();
        for selection in config.selections(None).unwrap() {
            let _ = management::unmount(&store, &selection, true, None, &Cancellation::default());
        }
    }
}

#[test]
fn binary_collision_warning_reaches_stderr_for_each_native_open() {
    let mut workspace = Workspace::new();
    fs::write(workspace.root.join("alpha/logo.png"), b"project").unwrap();
    fs::write(workspace.root.join(".ai/workspace/logo.png"), b"\0binary").unwrap();
    let mut events = workspace.mount(&["--project", "alpha", "--worktrees=false"], 1);
    for _ in 0..2 {
        assert!(
            fs::read_to_string(workspace.root.join("alpha/logo.png"))
                .unwrap()
                .contains("Copies of this path:")
        );
        let mut warning = String::new();
        events.read_line(&mut warning).unwrap();
        assert!(
            warning.starts_with("WARN: cannot merge logo.png: binary collision"),
            "{warning}"
        );
    }
    workspace.signal(libc::SIGTERM);
    assert!(workspace.wait_owner().success());
}

#[test]
fn real_binary_serves_primary_and_worktree_and_management_stops_it_cleanly() {
    let mut workspace = Workspace::new();
    let mut events = workspace.mount(&["--project", "alpha"], 2);
    let text = fs::read_to_string(workspace.root.join("alpha/AGENTS.md")).unwrap();
    assert!(text.contains("WORKSPACE-OVERLAY"));
    let status = workspace.output("status", &["--project", "alpha"]);
    assert!(status.status.success(), "{status:?}");
    let status = String::from_utf8(status.stdout).unwrap();
    assert_eq!(status.lines().count(), 2);
    assert!(
        status
            .lines()
            .all(|row| row.ends_with("\tfuse.workspace-overlay")),
        "{status}"
    );
    assert!(!status.contains("/home/alex/"), "{status}");
    let document = workspace.root.join("alpha/AGENTS.md");
    let edited = text
        .replace(
            "This contribution belongs to the backing project.",
            "Edited project contribution.",
        )
        .replace(
            "This contribution is shared by Alpha and Beta and their worktrees.",
            "Edited shared contribution.",
        );
    fs::write(&document, edited).unwrap();
    let git = Command::new("git")
        .arg("-C")
        .arg(workspace.root.join("alpha"))
        .args(["status", "--porcelain=v1", "-z"])
        .output()
        .unwrap();
    assert!(git.status.success(), "{git:?}");
    assert_eq!(git.stdout, b" M AGENTS.md\0");
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(workspace.root.join("alpha"))
            .args([
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.com",
                "commit",
                "-am",
                "Edit isolated project guidance",
                "-q"
            ])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        fs::read_to_string(workspace.root.join(".ai/workspace/AGENTS.md"))
            .unwrap()
            .contains("Edited shared contribution.")
    );
    fs::remove_file(&document).unwrap();
    let fallback = fs::read_to_string(&document).unwrap();
    assert!(fallback.contains("Edited shared contribution."));
    assert!(!fallback.contains("Edited project contribution."));
    assert_eq!(
        fs::remove_file(&document).unwrap_err().raw_os_error(),
        Some(libc::EPERM)
    );
    let stopped = workspace.output("unmount", &["--project", "alpha"]);
    assert!(stopped.status.success(), "{stopped:?}");
    assert!(workspace.wait_owner().success());
    assert!(!document.exists());
    let mut remaining = String::new();
    events.read_to_string(&mut remaining).unwrap();
    assert!(!remaining.contains("ERR:"), "{remaining}");
    assert!(
        workspace
            .output("status", &["--project", "alpha"])
            .stdout
            .ends_with(b"\tunmounted\n")
    );
}

#[test]
fn busy_shutdown_is_noninteractive_in_agent_mode_and_recovery_retains_the_holder() {
    let mut workspace = Workspace::new();
    let mut events = workspace.mount(&["--project", "alpha", "--worktrees=false"], 1);
    workspace.hold();
    let pid = workspace.holder.as_ref().unwrap().id();
    workspace.signal(libc::SIGINT);
    assert_eq!(workspace.wait_owner().code(), Some(1));
    let mut error = String::new();
    events.read_to_string(&mut error).unwrap();
    assert_eq!(error.matches("ERR:").count(), 1, "{error}");
    assert!(error.contains(&pid.to_string()), "{error}");
    assert!(!error.contains("[y/N]"));
    assert!(
        workspace
            .holder
            .as_mut()
            .unwrap()
            .try_wait()
            .unwrap()
            .is_none()
    );
    let mut holder = workspace.holder.take().unwrap();
    holder.kill().unwrap();
    holder.wait().unwrap();
    let recovered = workspace.output("unmount", &["--project", "alpha", "--worktrees=false"]);
    assert!(recovered.status.success(), "{recovered:?}");
    assert!(
        workspace
            .output("status", &["--project", "alpha"])
            .stdout
            .ends_with(b"\tunmounted\n")
    );
}

#[test]
fn replacement_and_second_signal_clean_stop_are_real_native_operations() {
    let mut workspace = Workspace::new();
    let _events = workspace.mount(&["--worktrees=false"], 2);
    let rejected = workspace.output("mount", &["--project", "alpha", "--worktrees=false"]);
    assert_eq!(rejected.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(rejected.stderr)
            .unwrap()
            .matches("ERR:")
            .count(),
        1
    );
    let mut original = workspace.owner.take().unwrap();
    let _replacement = workspace.mount(&["--replace", "--worktrees=false"], 2);
    let until = Instant::now() + WAIT;
    loop {
        if let Some(status) = original.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(10));
    }
    workspace.signal(libc::SIGTERM);
    assert!(workspace.wait_owner().success());
    let status = workspace.output("status", &["--worktrees=false"]);
    assert!(status.status.success(), "{status:?}");
    assert_eq!(
        String::from_utf8(status.stdout)
            .unwrap()
            .lines()
            .filter(|row| row.ends_with("\tunmounted"))
            .count(),
        2
    );
}

#[test]
fn binary_reports_parser_usage_and_runtime_failures_once_on_stderr() {
    let workspace = Workspace::new();
    let parse = workspace.output("mount", &["extra"]);
    assert_eq!(parse.status.code(), Some(1));
    assert!(parse.stdout.is_empty());
    let parse = String::from_utf8(parse.stderr).unwrap();
    assert!(parse.find("Usage:").unwrap() < parse.find("ERR:").unwrap());
    assert_eq!(parse.matches("ERR:").count(), 1);
    let runtime = workspace.output("status", &["--project", "absent"]);
    assert_eq!(runtime.status.code(), Some(1));
    let runtime = String::from_utf8(runtime.stderr).unwrap();
    assert!(!runtime.contains("Usage:"));
    assert_eq!(runtime.matches("ERR:").count(), 1);
}
