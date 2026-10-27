use notify::Watcher;
use std::{fs, process::Command, sync::mpsc, time::Duration};
use workspace_overlay::{Cancellation, Error, subprocess};

#[test]
fn native_output_exit_and_pre_cancelled_command() {
    let token = Cancellation::default();
    let mut command = Command::new("sh");
    command.args(["-c", r"printf '\377output'; printf diagnostic >&2"]);
    assert_eq!(
        subprocess::output(&mut command, &token).unwrap(),
        b"\xffoutput"
    );
    let mut failed = Command::new("sh");
    failed.args(["-c", "printf partial; printf useful >&2; exit 7"]);
    match subprocess::output(&mut failed, &token).unwrap_err() {
        Error::Command {
            status,
            stdout,
            stderr,
            ..
        } => {
            assert_eq!(status.code(), Some(7));
            assert_eq!(stdout, b"partial");
            assert_eq!(stderr, b"useful");
        }
        other => panic!("{other}"),
    }
    fs::create_dir_all(".tmp").unwrap();
    let temp = tempfile::tempdir_in(".tmp").unwrap();
    let path = temp.path().join("unwanted");
    token.cancel();
    let mut command = Command::new("touch");
    command.arg(&path);
    assert!(matches!(
        subprocess::output(&mut command, &token),
        Err(Error::Cancelled)
    ));
    assert!(!path.exists());
}

#[test]
fn cancellation_terminates_and_reaps_live_command_group() {
    fs::create_dir_all(".tmp").unwrap();
    let temp = tempfile::tempdir_in(".tmp").unwrap();
    let ready = temp.path().join("ready");
    let (events, received) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |event| {
        let _ = events.send(event);
    })
    .unwrap();
    watcher
        .watch(temp.path(), notify::RecursiveMode::NonRecursive)
        .unwrap();
    let token = Cancellation::default();
    let running = token.clone();
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg("printf '%s' $$ > \"$1.next\"; mv -- \"$1.next\" \"$1\"; sleep 60 & wait")
        .arg("test")
        .arg(&ready);
    let (finished, done) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let result = subprocess::output(&mut command, &running);
        finished.send(result).unwrap();
    });
    while !ready.exists() {
        received
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
    }
    let pid: libc::pid_t = fs::read_to_string(&ready).unwrap().parse().unwrap();
    token.cancel();
    assert!(matches!(
        done.recv_timeout(Duration::from_secs(2)).unwrap(),
        Err(Error::Cancelled)
    ));
    worker.join().unwrap();
    // The direct child has been reaped; cancellation does not leave a zombie.
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
}

#[test]
fn cancellation_wakes_all_waiters_and_wait_timeout_is_finite() {
    let token = Cancellation::default();
    assert!(!token.wait(Duration::ZERO));
    let (sent, received) = mpsc::channel();
    let workers: Vec<_> = (0..3)
        .map(|_| {
            let token = token.clone();
            let sent = sent.clone();
            std::thread::spawn(move || {
                sent.send(token.wait(Duration::from_secs(30))).unwrap();
            })
        })
        .collect();
    token.cancel();
    for _ in 0..3 {
        assert!(received.recv_timeout(Duration::from_secs(1)).unwrap());
    }
    for worker in workers {
        worker.join().unwrap();
    }
}

#[test]
fn cancellation_children_inherit_parent_without_cancelling_siblings() {
    let parent = Cancellation::default();
    let first = parent.child();
    let second = parent.child();
    first.cancel();
    assert!(!parent.is_cancelled());
    assert!(!second.is_cancelled());
    let grandchild = second.child();
    parent.cancel();
    assert!(second.is_cancelled());
    assert!(grandchild.is_cancelled());
    assert!(parent.child().is_cancelled());
}

#[test]
#[cfg(target_os = "linux")]
fn exited_leader_remains_owned_until_inherited_output_closes() {
    fs::create_dir_all(".tmp").unwrap();
    let temp = tempfile::tempdir_in(".tmp").unwrap();
    let ready = temp.path().join("ready");
    let (events, received) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |event| {
        let _ = events.send(event);
    })
    .unwrap();
    watcher
        .watch(temp.path(), notify::RecursiveMode::NonRecursive)
        .unwrap();
    let token = Cancellation::default();
    let running = token.clone();
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg("printf '%s' $$ > \"$1.next\"; mv -- \"$1.next\" \"$1\"; sleep 60 & exit 0")
        .arg("test")
        .arg(&ready);
    let worker = std::thread::spawn(move || subprocess::output(&mut command, &running));
    while !ready.exists() {
        received
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
    }
    let pid: libc::pid_t = fs::read_to_string(&ready).unwrap().parse().unwrap();
    let until = std::time::Instant::now() + Duration::from_millis(100);
    let mut observed_exit = false;
    let retained = loop {
        match fs::read_to_string(format!("/proc/{pid}/stat")) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break false,
            Ok(stat) => {
                observed_exit |= stat.rsplit_once(") ").unwrap().1.starts_with('Z');
            }
            _ => {}
        }
        if std::time::Instant::now() > until {
            break observed_exit;
        }
        std::thread::yield_now();
    };
    token.cancel();
    assert!(matches!(worker.join().unwrap(), Err(Error::Cancelled)));
    assert!(
        retained,
        "reaping the leader before draining inherited pipes allows its process-group ID to be reused"
    );
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
}
