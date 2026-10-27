use cli_command_kit::Mode;
use std::{fs, io, path::Path};
use workspace_overlay::holders::Process;
use workspace_overlay_cli::prompt;

#[test]
fn partial_affirmative_input_followed_by_native_read_failure_does_not_authorize() {
    use io::Read;
    fs::create_dir_all(".tmp").unwrap();
    let directory = fs::File::open(".tmp").unwrap();
    let bytes = io::Cursor::new(b"yes").chain(directory);
    let mut input = io::BufReader::new(bytes);
    let error = prompt::ask(
        Path::new("/srv/alpha"),
        &[],
        &mut input,
        &mut Vec::new(),
        80,
    )
    .unwrap_err();
    assert_eq!(error.raw_os_error(), Some(libc::EISDIR));
}

#[test]
fn answers_report_actual_retained_process_and_fit_a_narrow_table() {
    let mut process = Process::open(std::process::id()).unwrap().unwrap();
    process.command = format!("editor {}", "--flag=value ".repeat(40)).into();
    let target = std::env::current_dir().unwrap();
    process.directory = Some(target.clone());
    for (typed, expected) in [
        ("y\n", true),
        (" YES \n", true),
        ("n\n", false),
        ("\n", false),
        ("maybe\n", false),
        ("", false),
    ] {
        let mut shown = Vec::new();
        assert_eq!(
            prompt::ask(
                &target,
                std::slice::from_ref(&process),
                &mut typed.as_bytes(),
                &mut shown,
                60
            )
            .unwrap(),
            expected
        );
        let shown = String::from_utf8(shown).unwrap();
        assert!(shown.contains(&process.pid.to_string()));
        assert!(shown.contains("editor"));
        assert!(shown.contains(&cli_command_kit::display_path(&target)));
        let table = shown
            .split_once("These processes are using it:\n")
            .unwrap()
            .1
            .split_once("Stop these processes")
            .unwrap()
            .0;
        assert!(
            table.lines().all(|line| line.chars().count() <= 60),
            "{shown}"
        );
        assert_eq!(shown.contains("stays mounted"), !expected);
    }
    process.directory = None;
    assert!(
        !prompt::ask(
            Path::new("/srv/alpha"),
            &[process],
            &mut &b"no\n"[..],
            &mut Vec::new(),
            usize::MAX
        )
        .unwrap()
    );
}

#[test]
fn prompting_requires_a_human_at_both_terminals_and_write_failures_propagate() {
    for mode in [Mode::Human, Mode::Agent] {
        for input in [false, true] {
            for output in [false, true] {
                assert_eq!(
                    prompt::available(mode, input, output),
                    mode == Mode::Human && input && output
                );
            }
        }
    }
    assert!(prompt::confirmation(Mode::Agent).is_none());
    // A real read-only descriptor fails writing the initial diagnostic.
    fs::create_dir_all(".tmp").unwrap();
    let temp = tempfile::tempdir_in(".tmp").unwrap();
    fs::write(temp.path().join("readonly"), b"keep").unwrap();
    let mut readonly = fs::File::open(temp.path().join("readonly")).unwrap();
    let error = prompt::ask(
        Path::new("/srv/alpha"),
        &[],
        &mut &b"yes\n"[..],
        &mut readonly,
        0,
    )
    .unwrap_err();
    assert_eq!(error.raw_os_error(), Some(libc::EBADF));
    struct FlushFailure;
    impl io::Write for FlushFailure {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            Ok(data.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::from_raw_os_error(libc::ENOSPC))
        }
    }
    assert_eq!(
        prompt::ask(
            Path::new("/srv/alpha"),
            &[],
            &mut &b"yes\n"[..],
            &mut FlushFailure,
            80
        )
        .unwrap_err()
        .raw_os_error(),
        Some(libc::ENOSPC)
    );
}
