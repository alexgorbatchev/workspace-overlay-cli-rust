use cli_command_kit::Mode;
use std::{ffi::OsString, fs, io};
use workspace_overlay::Cancellation;
use workspace_overlay::registry::Store;
use workspace_overlay_cli::{command, help_options, run};

fn invoke(args: &[&str], mode: Mode) -> (workspace_overlay_cli::Result<()>, String, String) {
    let state = tempfile::tempdir_in(".tmp").unwrap();
    let mut output = Vec::new();
    let mut diagnostic = Vec::new();
    let result = run(
        args.iter().map(OsString::from),
        mode,
        &Cancellation::default(),
        Some(&Store::new(state.path().to_owned()).unwrap()),
        None,
        &mut output,
        &mut diagnostic,
    );
    (
        result,
        String::from_utf8(output).unwrap(),
        String::from_utf8(diagnostic).unwrap(),
    )
}

#[test]
fn skill_and_all_help_paths_are_offline_and_mode_aware() {
    for mode in [Mode::Human, Mode::Agent] {
        let (result, text, error) = invoke(&["skill"], mode);
        result.unwrap();
        assert_eq!(text, include_str!("../skills/workspace-overlay/SKILL.md"));
        assert!(error.is_empty());
        for args in [
            vec![],
            vec!["--help"],
            vec!["overlay", "--help"],
            vec!["overlay", "mount", "--help"],
            vec!["overlay", "unmount", "--help"],
            vec!["overlay", "status", "--help"],
            vec!["skill", "--help"],
            vec!["help"],
            vec!["help", "overlay", "status"],
            vec!["help", "help"],
        ] {
            let (result, text, error) = invoke(&args, mode);
            result.unwrap();
            assert!(text.contains("workspace-overlay"), "{args:?}: {text}");
            assert_eq!(text.starts_with("ALERT: Agents must read `AGENT=1 workspace-overlay skill` before using this tool.\n"), mode == Mode::Agent, "{args:?}: {text}");
            assert!(error.is_empty());
        }
        assert_eq!(invoke(&["--version"], mode).1, "0.1.0\n");
        assert_eq!(invoke(&["-v"], mode).1, "0.1.0\n");
    }
}

#[test]
fn parser_rejects_arguments_before_loading_files_and_runtime_errors_omit_usage() {
    for args in [
        vec!["skill", "extra"],
        vec!["overlay", "mount", "extra"],
        vec!["overlay", "mount", "--worktrees=invalid"],
        vec!["overlay", "status", "--replace"],
        vec!["completion"],
        vec!["unknown"],
    ] {
        let (result, output, _) = invoke(&args, Mode::Agent);
        let error = result.unwrap_err().to_string();
        assert!(error.contains("Usage:"), "{args:?}: {error}");
        assert!(output.is_empty());
    }
    let error = invoke(
        &[
            "overlay",
            "status",
            "--config",
            "/nonexistent/workspace-overlay.toml",
        ],
        Mode::Agent,
    )
    .0
    .unwrap_err()
    .to_string();
    assert!(!error.contains("Usage:"));
}

#[test]
fn real_configuration_status_and_unmount_preserve_native_paths_and_empty_selection() {
    let directory = tempfile::Builder::new()
        .prefix("overlay-cli-")
        .tempdir_in(".tmp")
        .unwrap();
    fs::create_dir(directory.path().join("alpha")).unwrap();
    fs::write(
        directory.path().join("workspace-overlay.toml"),
        "version = 1\n[projects.alpha]\npath = 'alpha'\n",
    )
    .unwrap();
    for action in ["status", "unmount"] {
        let mut output = Vec::new();
        let result = run(
            [
                OsString::from("overlay"),
                action.into(),
                "--config".into(),
                directory
                    .path()
                    .join("workspace-overlay.toml")
                    .into_os_string(),
                "--worktrees=false".into(),
            ],
            Mode::Agent,
            &Cancellation::default(),
            Some(&Store::new(directory.path().join("state")).unwrap()),
            None,
            &mut output,
            &mut Vec::new(),
        );
        result.unwrap();
        if action == "status" {
            assert_eq!(
                String::from_utf8(output).unwrap(),
                format!(
                    "{}\tunmounted\n",
                    cli_command_kit::display_path(
                        &fs::canonicalize(directory.path().join("alpha")).unwrap()
                    )
                )
            );
        } else {
            assert!(output.is_empty());
        }
    }
    let config = directory.path().join("workspace-overlay.toml");
    let args = [
        "overlay",
        "status",
        "--project",
        "absent",
        "--config",
        config.to_str().unwrap(),
    ];
    assert!(
        invoke(&args, Mode::Agent)
            .0
            .unwrap_err()
            .to_string()
            .contains("not configured")
    );
}

struct Broken;
impl io::Write for Broken {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::from_raw_os_error(libc::ENOSPC))
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn output_errors_and_live_skill_interface_are_checked() {
    for args in [vec!["skill"], vec!["--help"], vec!["--version"]] {
        assert!(
            run(
                args,
                Mode::Human,
                &Cancellation::default(),
                Some(&Store::new(std::path::PathBuf::from(".tmp/test-state")).unwrap()),
                None,
                &mut Broken,
                &mut Vec::new()
            )
            .is_err()
        );
    }
    let options = help_options();
    let root = command(Mode::Agent).unwrap();
    let missing = cli_command_kit::skill_omissions(
        &root,
        &options,
        include_str!("../skills/workspace-overlay/SKILL.md"),
    );
    assert!(missing.is_empty(), "{missing:?}");
}

#[test]
fn all_generated_help_queries_and_foreign_filesystem_failures_are_preserved() {
    for mode in [Mode::Human, Mode::Agent] {
        for args in [
            vec!["overlay"],
            vec!["help", "overlay"],
            vec!["help", "overlay", "mount"],
            vec!["help", "overlay", "unmount"],
            vec!["help", "skill"],
            vec!["overlay", "help"],
            vec!["overlay", "help", "help"],
            vec!["overlay", "help", "mount"],
            vec!["overlay", "help", "unmount"],
            vec!["overlay", "help", "status"],
        ] {
            let (result, shown, diagnostic) = invoke(&args, mode);
            result.unwrap();
            assert!(!shown.is_empty());
            assert_eq!(
                shown.starts_with("ALERT:"),
                mode == Mode::Agent,
                "{args:?}: {shown}"
            );
            assert!(diagnostic.is_empty());
        }
    }
    let temp = tempfile::tempdir_in(".tmp").unwrap();
    let config = temp.path().join("workspace-overlay.toml");
    fs::write(
        &config,
        "version = 1\n[projects.proc]\npath = '/proc'\n[projects.sys]\npath = '/sys'\n",
    )
    .unwrap();
    for selected in [None, Some("proc")] {
        let mut args = vec![
            OsString::from("overlay"),
            "unmount".into(),
            "--worktrees=false".into(),
            "--config".into(),
            config.clone().into_os_string(),
        ];
        if let Some(project) = selected {
            args.extend(["--project".into(), project.into()]);
        }
        let store = Store::new(temp.path().join("state")).unwrap();
        let error = run(
            args,
            Mode::Agent,
            &Cancellation::default(),
            Some(&store),
            None,
            &mut Vec::new(),
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("/proc"), "{error}");
        if selected.is_none() {
            assert!(error.to_string().contains("/sys"), "{error}");
        }
        assert!(fs::metadata("/proc/self").is_ok());
        assert!(fs::metadata("/sys/devices").is_ok());
    }
}
