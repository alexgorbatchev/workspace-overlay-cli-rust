use std::{
    fs,
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    time::Duration,
};
use workspace_overlay::{
    Cancellation,
    holders::{self, Process},
};

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn child(script: &str, path: &std::path::Path) -> ChildGuard {
    let mut process = Command::new("sh")
        .args(["-c", script])
        .current_dir(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut ready = String::new();
    BufReader::new(process.stdout.take().unwrap())
        .read_line(&mut ready)
        .unwrap();
    assert_eq!(ready, "ready\n");
    ChildGuard(process)
}

#[test]
fn native_pidfd_descriptions_stop_live_processes_and_treat_unreaped_exit_as_complete() {
    fs::create_dir_all(".tmp").unwrap();
    let temp = tempfile::tempdir_in(".tmp").unwrap();
    let directory = fs::canonicalize(temp.path()).unwrap();
    for script in [
        "echo ready; read value",
        "trap '' TERM; echo ready; read value",
    ] {
        let mut child = child(script, &directory);
        let process = Process::open(child.0.id()).unwrap().unwrap();
        assert_eq!(process.pid, child.0.id());
        assert_eq!(
            process.command,
            std::ffi::OsString::from(format!("sh -c {script}"))
        );
        assert_eq!(process.directory.as_deref(), Some(directory.as_path()));
        holders::stop(
            &[process],
            Duration::from_millis(20),
            &Cancellation::default(),
        )
        .unwrap();
        child.0.wait().unwrap();
        assert!(Process::open(child.0.id()).unwrap().is_none());
    }
    let mut child = child("echo ready; exec sleep 60", &directory);
    let process = Process::open(child.0.id()).unwrap().unwrap();
    child.0.kill().unwrap();
    holders::stop(&[process], Duration::from_secs(2), &Cancellation::default()).unwrap();
    child.0.wait().unwrap();
    assert!(Process::open(0).is_err());
}

#[test]
fn ordinary_directory_never_names_disk_processes_and_cancelled_stop_still_kills() {
    fs::create_dir_all(".tmp").unwrap();
    let temp = tempfile::tempdir_in(".tmp").unwrap();
    assert!(
        holders::find(temp.path(), &Cancellation::default())
            .unwrap()
            .is_empty()
    );
    let mut child = child("trap '' TERM; echo ready; exec sleep 60", temp.path());
    let process = Process::open(child.0.id()).unwrap().unwrap();
    let cancel = Cancellation::default();
    cancel.cancel();
    assert!(matches!(
        holders::stop(&[process], Duration::from_secs(10), &cancel),
        Err(workspace_overlay::Error::Cancelled)
    ));
    child.0.wait().unwrap();
}

#[test]
fn unrepresentable_grace_is_rejected_before_signalling() {
    fs::create_dir_all(".tmp").unwrap();
    let temp = tempfile::tempdir_in(".tmp").unwrap();
    let mut child = child("echo ready; exec sleep 60", temp.path());
    let process = Process::open(child.0.id()).unwrap().unwrap();
    assert!(matches!(
        holders::stop(&[process], Duration::MAX, &Cancellation::default()),
        Err(workspace_overlay::Error::Invalid(_))
    ));
    assert!(child.0.try_wait().unwrap().is_none());
}
