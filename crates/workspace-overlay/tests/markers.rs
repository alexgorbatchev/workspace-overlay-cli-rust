use workspace_overlay::{
    config::MarkerRule,
    markers::{Markers, Section},
};

#[test]
fn render_parse_preserves_sections_and_rejects_corrupt_full_markers() {
    let markers = Markers::new(&[]).unwrap();
    let rule = markers
        .find(std::path::Path::new("nested/AGENTS.md"))
        .unwrap();
    let sections = vec![
        Section::new(0, "project", "AGENTS.md", b"base"),
        Section::new(1, "shared", "../shared/AGENTS.md", b"one"),
        Section::new(2, "shared-two", "../second/AGENTS.md", b"two\n"),
    ];
    let data = rule.render(&sections).unwrap();
    assert!(
        data.starts_with(b"base\n<!-- BEGIN WORKSPACE-OVERLAY: shared (../shared/AGENTS.md) -->\n")
    );
    let parsed = rule.parse(&data, &sections).unwrap();
    assert_eq!(
        parsed,
        vec![
            (0, b"base\n".to_vec()),
            (1, b"one\n".to_vec()),
            (2, b"two\n".to_vec())
        ]
    );
    let mut appended = data.clone();
    appended.extend_from_slice(b"after\n");
    assert_eq!(
        rule.parse(&appended, &sections).unwrap()[2].1,
        b"two\nafter\n"
    );
    for from in [
        "shared (../shared/AGENTS.md)",
        "END WORKSPACE-OVERLAY",
        "BEGIN WORKSPACE-OVERLAY",
    ] {
        let corrupted = String::from_utf8(data.clone())
            .unwrap()
            .replacen(from, "CORRUPTED", 1);
        assert_eq!(
            rule.parse(corrupted.as_bytes(), &sections)
                .unwrap_err()
                .raw_os_error(),
            Some(libc::EPERM)
        );
    }
    let missing = String::from_utf8(data).unwrap().replace("<!-- BEGIN WORKSPACE-OVERLAY: shared (../shared/AGENTS.md) -->\none\n<!-- END WORKSPACE-OVERLAY: shared (../shared/AGENTS.md) -->\n", "");
    assert!(rule.parse(missing.as_bytes(), &sections).is_err());
}

#[test]
fn custom_rules_order_eof_empty_and_binary_path_bytes() {
    use std::{ffi::OsStr, os::unix::ffi::OsStrExt, path::Path};
    let custom = MarkerRule {
        glob: "nested/**/*.txt".into(),
        start: "START {name} {path}".into(),
        end: "END {name}".into(),
    };
    let rules = Markers::new(&[custom]).unwrap();
    assert!(rules.find(Path::new("other/file.txt")).is_none());
    let rule = rules.find(Path::new("nested/a/file.txt")).unwrap();
    let path = Path::new(OsStr::from_bytes(b"../overlay/\xff.txt"));
    let parts = vec![
        Section::new(0, "base", "file.txt", b""),
        Section::new(4, "overlay", path, b"x\n"),
    ];
    let mut rendered = rule.render(&parts).unwrap();
    assert!(rendered.windows(6).any(|s| s == b"\xff.txt\n"));
    rendered.pop();
    assert_eq!(
        rule.parse(&rendered, &parts).unwrap(),
        vec![(0, vec![]), (4, b"x\n".to_vec())]
    );
    assert_eq!(rule.render(&parts[..1]).unwrap(), b"");
    assert_eq!(
        rule.parse(b"unmarked", &parts[..1]).unwrap(),
        vec![(0, b"unmarked".to_vec())]
    );
    assert!(rule.render(&[]).is_err());
    assert!(rule.parse(b"", &[]).is_err());
    assert!(
        Markers::new(&[MarkerRule {
            glob: "[".into(),
            start: "S".into(),
            end: "E".into()
        }])
        .is_err()
    );
    let default = Markers::new(&[]).unwrap();
    for name in [
        "README.md",
        "a.html",
        "a.toml",
        "x.env.local",
        ".gitignore",
        "Dockerfile.prod",
        "Makefile",
        "src/x.rs",
    ] {
        assert!(default.find(Path::new(name)).is_some(), "{name}");
    }
    assert!(default.find(Path::new("plain.txt")).is_none());
}

#[test]
fn repeated_custom_delimiters_follow_section_order_and_size_matches_render() {
    use std::path::Path;
    let markers = Markers::new(&[MarkerRule {
        glob: "*.txt".into(),
        start: "BEGIN".into(),
        end: "END".into(),
    }])
    .unwrap();
    let rule = markers.find(Path::new("shared.txt")).unwrap();
    let parts = vec![
        Section::new(0, "project", "shared.txt", b"project"),
        Section::new(1, "one", "../one/shared.txt", b"first"),
        Section::new(2, "two", "../two/shared.txt", b"second"),
    ];
    let rendered = rule.render(&parts).unwrap();
    assert_eq!(
        rule.parse(&rendered, &parts).unwrap(),
        vec![
            (0, b"project\n".to_vec()),
            (1, b"first\n".to_vec()),
            (2, b"second\n".to_vec()),
        ]
    );
    for count in 1..=parts.len() {
        let sizes: Vec<_> = parts[..count]
            .iter()
            .map(|section| {
                (
                    section.clone(),
                    section.data.len() as u64,
                    section.data.last().copied(),
                )
            })
            .collect();
        assert_eq!(
            rule.size(&sizes).unwrap(),
            rule.render(&parts[..count]).unwrap().len() as u64
        );
    }
    assert!(rule.parse(b"BEGIN\nfirst\nEND\n", &parts).is_err());
    assert!(
        rule.size(&[
            (parts[0].clone(), u64::MAX, Some(b'x')),
            (parts[1].clone(), 1, Some(b'\n'))
        ])
        .is_err()
    );
}
