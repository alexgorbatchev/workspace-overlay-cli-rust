use std::{fs, io};
use workspace_overlay_cli::{Error, finish, report};

#[test]
fn cleanup_retains_both_native_causes_and_report_prints_one_error() {
    fs::create_dir_all(".tmp").unwrap();
    let temp = tempfile::tempdir_in(".tmp").unwrap();
    let operation = fs::remove_file(temp.path()).unwrap_err();
    let cleanup = fs::read(temp.path().join("missing")).unwrap_err();
    let first = operation.to_string();
    let second = cleanup.to_string();
    let error = finish(Err(Error::Io(operation)), Err(cleanup)).unwrap_err();
    let mut output = Vec::new();
    report(error, &mut output).unwrap();
    let text = String::from_utf8(output).unwrap();
    assert_eq!(text.matches("ERR:").count(), 1);
    assert!(text.contains(&first));
    assert!(text.contains(&second));
    finish(Ok(()), Ok(())).unwrap();
    assert!(
        finish(
            Err(Error::Io(io::Error::from_raw_os_error(libc::EIO))),
            Ok(())
        )
        .is_err()
    );
    assert!(finish(Ok(()), Err(io::Error::from_raw_os_error(libc::EBADF))).is_err());
}

#[test]
fn argument_usage_precedes_the_single_error_and_write_errors_are_native() {
    let clap = workspace_overlay_cli::command(cli_command_kit::Mode::Agent)
        .unwrap()
        .try_get_matches_from(["workspace-overlay", "overlay", "mount", "unexpected"])
        .unwrap_err();
    let mut output = Vec::new();
    report(clap.into(), &mut output).unwrap();
    let text = String::from_utf8(output).unwrap();
    assert!(
        text.find("Usage:").unwrap() < text.find("ERR:").unwrap(),
        "{text}"
    );
    assert_eq!(text.matches("Usage:").count(), 1);
    assert_eq!(text.matches("ERR:").count(), 1);
    let temp = tempfile::tempdir_in(".tmp").unwrap();
    fs::write(temp.path().join("readonly"), b"keep").unwrap();
    let mut file = fs::File::open(temp.path().join("readonly")).unwrap();
    assert_eq!(
        report(
            Error::Io(io::Error::from_raw_os_error(libc::EIO)),
            &mut file
        )
        .unwrap_err()
        .raw_os_error(),
        Some(libc::EBADF)
    );
}
