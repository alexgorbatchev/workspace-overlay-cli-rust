//! Strict TOML configuration, ancestor discovery, and ordered overlay selection.
use crate::{Error, Result};
use globset::{GlobBuilder, GlobMatcher};
use path_clean::PathClean;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

pub const NAME: &str = "workspace-overlay.toml";

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Options {
    pub collision: String,
    pub write: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Project {
    pub path: PathBuf,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Overlay {
    pub name: String,
    pub source: PathBuf,
    pub projects: Vec<String>,
    pub glob: String,
    pub collision: String,
    pub write: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct MarkerRule {
    pub glob: String,
    pub start: String,
    pub end: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Configuration {
    pub version: u32,
    pub defaults: Options,
    pub markers: Vec<MarkerRule>,
    pub projects: BTreeMap<String, Project>,
    pub overlays: Vec<Overlay>,
    #[serde(skip)]
    pub root: PathBuf,
}

#[derive(Debug, Clone)]
pub struct Source {
    pub name: String,
    pub path: PathBuf,
    pub glob: String,
}

#[derive(Debug, Clone)]
pub struct Selection {
    pub root: PathBuf,
    pub project: String,
    pub target: PathBuf,
    pub markers: Vec<MarkerRule>,
    pub sources: Vec<Source>,
}

fn invalid(message: impl Into<String>) -> Error {
    Error::Invalid(message.into())
}

/// Resolve an existing path's symbolic links, otherwise clean it lexically.
pub fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.clean())
}

/// Compile shell-style globs with separators requiring literal `/` and `**` recursive.
pub fn pattern(text: &str) -> Result<GlobMatcher> {
    Ok(GlobBuilder::new(text)
        .literal_separator(true)
        .backslash_escape(true)
        .empty_alternates(true)
        .build()?
        .compile_matcher())
}

/// Locate the config while retaining the filename when it is itself a symlink.
pub fn config_path_from(name: Option<&Path>, cwd: &Path) -> Result<PathBuf> {
    let file = if let Some(name) = name.filter(|p| !p.as_os_str().is_empty()) {
        if name.is_absolute() {
            name.clean()
        } else {
            cwd.join(name).clean()
        }
    } else {
        let mut dir = cwd.to_path_buf();
        loop {
            let candidate = dir.join(NAME);
            match fs::metadata(&candidate) {
                Ok(_) => break candidate,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(err.into()),
            }
            if !dir.pop() {
                return Err(invalid(format!("{NAME} not found; specify --config")));
            }
        }
    };
    let parent = file
        .parent()
        .ok_or_else(|| invalid("configuration has no parent directory"))?;
    let filename = file
        .file_name()
        .ok_or_else(|| invalid("configuration must name a file"))?;
    Ok(canonical(parent).join(filename))
}

pub fn config_path(name: Option<&Path>) -> Result<PathBuf> {
    config_path_from(name, &std::env::current_dir()?)
}

fn policies(collision: &str, write: &str) -> Result<()> {
    if collision != "concat" {
        return Err(invalid(format!(
            "unsupported collision policy {collision:?}; use concat"
        )));
    }
    if write != "most-specific" {
        return Err(invalid(format!(
            "unsupported write policy {write:?}; use most-specific"
        )));
    }
    Ok(())
}

fn absolute(root: &Path, path: &Path) -> PathBuf {
    canonical(&if path.is_absolute() {
        path.clean()
    } else {
        root.join(path).clean()
    })
}

impl Configuration {
    pub fn load(path: &Path) -> Result<Self> {
        Self::discover(Some(path))
    }

    pub fn discover(path: Option<&Path>) -> Result<Self> {
        let file = config_path(path)?;
        let mut config: Self = toml::from_str(&fs::read_to_string(&file)?)?;
        config.root = file
            .parent()
            .expect("located file has parent")
            .to_path_buf();
        config.validate()?;
        Ok(config)
    }

    fn validate(&mut self) -> Result<()> {
        if self.version != 1 {
            return Err(invalid("version must be 1"));
        }
        if self.projects.is_empty() {
            return Err(invalid("at least one project is required"));
        }
        if self.defaults.collision.is_empty() {
            self.defaults.collision = "concat".into();
        }
        if self.defaults.write.is_empty() {
            self.defaults.write = "most-specific".into();
        }
        policies(&self.defaults.collision, &self.defaults.write)?;
        for (i, marker) in self.markers.iter().enumerate() {
            if marker.glob.is_empty() {
                return Err(invalid(format!("marker {i} requires glob")));
            }
            if marker.start.is_empty() || marker.end.is_empty() {
                return Err(invalid(format!("marker {i} requires start and end")));
            }
            pattern(&marker.glob)?;
        }
        for (name, project) in &mut self.projects {
            if name.is_empty() || [".", ".."].contains(&name.as_str()) || name.contains(['/', '\\'])
            {
                return Err(invalid(format!(
                    "project {name:?}: project must be a directory name"
                )));
            }
            if project.path.as_os_str().is_empty() {
                return Err(invalid(format!("project {name}: path is required")));
            }
            project.path = absolute(&self.root, &project.path);
        }
        let mut names = BTreeSet::new();
        for overlay in &mut self.overlays {
            if overlay.name.is_empty() || !names.insert(overlay.name.clone()) {
                return Err(invalid(format!(
                    "overlay names must be nonempty and unique: {:?}",
                    overlay.name
                )));
            }
            if overlay.source.as_os_str().is_empty() {
                return Err(invalid(format!(
                    "overlay {}: source is required",
                    overlay.name
                )));
            }
            overlay.source = absolute(&self.root, &overlay.source);
            if overlay.projects.is_empty() {
                return Err(invalid(format!(
                    "overlay {}: projects selectors are required",
                    overlay.name
                )));
            }
            for selector in &overlay.projects {
                pattern(selector)?;
            }
            if overlay.glob.is_empty() {
                overlay.glob = "**/*".into();
            }
            if overlay.glob.starts_with('/')
                || overlay.glob.contains('\\')
                || overlay.glob.split('/').any(|s| s == "." || s == "..")
            {
                return Err(invalid(format!(
                    "overlay {}: glob must be relative without . or .. components",
                    overlay.name
                )));
            }
            pattern(&overlay.glob)?;
            if overlay.collision.is_empty() {
                overlay.collision.clone_from(&self.defaults.collision);
            }
            if overlay.write.is_empty() {
                overlay.write.clone_from(&self.defaults.write);
            }
            policies(&overlay.collision, &overlay.write)?;
        }
        Ok(())
    }

    pub fn selections(&self, project: Option<&str>) -> Result<Vec<Selection>> {
        let project = project.filter(|p| !p.is_empty());
        if let Some(name) = project
            && !self.projects.contains_key(name)
        {
            return Err(invalid(format!("project {name:?} is not configured")));
        }
        self.projects
            .iter()
            .filter(|(name, _)| project.is_none_or(|p| *name == p))
            .map(|(name, value)| {
                let mut sources = Vec::new();
                for overlay in &self.overlays {
                    for selector in &overlay.projects {
                        if pattern(selector)?.is_match(name) {
                            sources.push(Source {
                                name: overlay.name.clone(),
                                path: overlay.source.clone(),
                                glob: overlay.glob.clone(),
                            });
                            break;
                        }
                    }
                }
                Ok(Selection {
                    root: self.root.clone(),
                    project: name.clone(),
                    target: value.path.clone(),
                    markers: self.markers.clone(),
                    sources,
                })
            })
            .collect()
    }
}
