use std::{
    fs,
    os::unix::ffi::{OsStrExt, OsStringExt},
    process::Command,
};

#[test]
fn developer_fixture_runner_creates_reuses_and_validates_native_workspace() {
    fs::create_dir_all(".tmp").unwrap();
    let temp = tempfile::tempdir_in(".tmp").unwrap();
    let root = fs::canonicalize(temp.path())
        .unwrap()
        .join(std::ffi::OsString::from_vec(b"fixture-\xff".to_vec()));
    for _ in 0..2 {
        let output = Command::new(env!("CARGO_BIN_EXE_workspace-overlay-fixture"))
            .arg(&root)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let mut expected = root
            .join("workspace-overlay.toml")
            .as_os_str()
            .as_bytes()
            .to_owned();
        expected.push(b'\n');
        assert_eq!(output.stdout, expected);
        assert!(output.stderr.is_empty());
        let config = workspace_overlay::config::Configuration::discover(Some(
            &root.join("workspace-overlay.toml"),
        ))
        .unwrap();
        assert_eq!(config.selections(None).unwrap().len(), 2);
    }
    fs::write(root.join("workspace-overlay.toml"), b"invalid").unwrap();
    let failed = Command::new(env!("CARGO_BIN_EXE_workspace-overlay-fixture"))
        .arg(&root)
        .output()
        .unwrap();
    assert_eq!(failed.status.code(), Some(1));
    assert!(failed.stdout.is_empty());
    assert_eq!(
        String::from_utf8(failed.stderr)
            .unwrap()
            .matches("ERR:")
            .count(),
        1
    );
}

#[test]
fn developer_runner_exposes_offline_help_skill_and_raw_version() {
    for agent in ["0", "1"] {
        for arguments in [
            vec!["--help"],
            vec!["help"],
            vec!["help", "help"],
            vec!["help", "skill"],
        ] {
            let output = Command::new(env!("CARGO_BIN_EXE_workspace-overlay-fixture"))
                .args(arguments)
                .env("AGENT", agent)
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            assert_eq!(output.stdout.starts_with(b"ALERT:"), agent == "1");
        }
        let output = Command::new(env!("CARGO_BIN_EXE_workspace-overlay-fixture"))
            .arg("skill")
            .env("AGENT", agent)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, workspace_overlay_cli::SKILL.as_bytes());
        let output = Command::new(env!("CARGO_BIN_EXE_workspace-overlay-fixture"))
            .arg("--version")
            .env("AGENT", agent)
            .output()
            .unwrap();
        assert_eq!(output.stdout, b"0.1.0\n");
    }
    let output = Command::new(env!("CARGO_BIN_EXE_workspace-overlay-fixture"))
        .args(["one", "two"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8(output.stderr).unwrap().contains("Usage:"));
}
