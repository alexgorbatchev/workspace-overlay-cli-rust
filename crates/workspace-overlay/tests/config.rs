use std::{fs, os::unix::fs::symlink};
use workspace_overlay::config::{Configuration, config_path_from};

fn fixture() -> tempfile::TempDir {
    fs::create_dir_all(".tmp").unwrap();
    tempfile::tempdir_in(".tmp").unwrap()
}

#[test]
fn discovers_link_location_and_selects_ordered_native_globs() {
    let temp = fixture();
    let root = fs::canonicalize(temp.path()).unwrap();
    fs::create_dir_all(root.join("workspace/web/child")).unwrap();
    fs::create_dir(root.join("stored")).unwrap();
    let text = "version=1\n[projects.web]\npath='web'\n[projects.api]\npath='api'\n[[overlays]]\nname='common'\nsource='shared'\nprojects=['*','web']\n[[overlays]]\nname='web'\nsource='specific'\nprojects=['w*']\nglob='**/*.{md,rs}'\n";
    fs::write(root.join("stored/config.toml"), text).unwrap();
    symlink(
        root.join("stored/config.toml"),
        root.join("workspace/workspace-overlay.toml"),
    )
    .unwrap();
    symlink(root.join("workspace"), root.join("alias")).unwrap();
    let located = config_path_from(None, &root.join("alias/web/child")).unwrap();
    assert_eq!(located, root.join("workspace/workspace-overlay.toml"));
    let config = Configuration::load(&located).unwrap();
    let selected = config.selections(None).unwrap();
    assert_eq!(
        selected
            .iter()
            .map(|s| s.project.as_str())
            .collect::<Vec<_>>(),
        ["api", "web"]
    );
    assert_eq!(selected[1].target, root.join("workspace/web"));
    assert_eq!(selected[1].sources.len(), 2);
    assert_eq!(selected[1].sources[0].name, "common");
    assert_eq!(selected[1].sources[0].glob, "**/*");
    assert!(config.selections(Some("missing")).is_err());
    assert_eq!(config.selections(Some("web")).unwrap().len(), 1);
    assert_eq!(
        config_path_from(Some(&root.join("alias/workspace-overlay.toml")), &root).unwrap(),
        located
    );
    assert!(config_path_from(None, &root.join("stored")).is_err());
}

#[test]
fn rejects_invalid_config_before_filesystem_mounts() {
    let root = fixture();
    let valid = "version=1\n[projects.web]\npath='web'\n";
    let overlay = "[[overlays]]\nname='common'\nsource='shared'\nprojects=['*']\n";
    let cases = [
        "version=[".to_owned(),
        "version=2\n[projects.web]\npath='web'".into(),
        "version=1".into(),
        "version=1\n[projects.web]\npath=''".into(),
        "version=1\n[projects.'a/b']\npath='web'".into(),
        format!("{valid}typo=true"),
        format!("{valid}{overlay}{overlay}"),
        format!("{valid}{}", overlay.replace("source='shared'", "source=''")),
        format!(
            "{valid}{}",
            overlay.replace("projects=['*']", "projects=[]")
        ),
        format!(
            "{valid}{}",
            overlay.replace("projects=['*']", "projects=['[']")
        ),
        format!("{valid}{overlay}collision='replace'"),
        format!("{valid}{overlay}write='base'"),
        format!("{valid}\n[[markers]]\nglob='['\nstart='a'\nend='b'"),
        format!("{valid}\n[[markers]]\nglob='*.md'\nstart=''\nend='b'"),
    ];
    let path = root.path().join("config.toml");
    for text in cases {
        fs::write(&path, &text).unwrap();
        assert!(Configuration::load(&path).is_err(), "accepted {text}");
    }
    for glob in ["[", "/**", "a\\b", "./*.md", "../*.md"] {
        fs::write(&path, format!("{valid}{overlay}glob='{glob}'")).unwrap();
        assert!(Configuration::load(&path).is_err(), "accepted {glob}");
    }
    fs::write(
        &path,
        "version=1\n[defaults]\nwrite='base'\n[projects.web]\npath='web'",
    )
    .unwrap();
    assert!(Configuration::load(&path).is_err());
    assert!(Configuration::load(&root.path().join("missing")).is_err());
}
