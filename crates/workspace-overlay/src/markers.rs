//! Comment-delimited contributions and strict, byte-preserving document saves.
use crate::{
    Result,
    config::{MarkerRule, pattern},
};
use globset::GlobMatcher;
use parking_lot::Mutex;
use std::{
    collections::HashMap,
    io,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    sync::Arc,
};

pub const MAX_MERGED_SIZE: usize = 64 << 20;
const MATCH_LIMIT: usize = 1024;

/// One source contribution. The project is first, including an empty project
/// contribution when several overlays supply a document without a project copy.
#[derive(Debug, Clone)]
pub struct Section {
    pub layer: usize,
    pub name: String,
    pub path: PathBuf,
    pub data: Vec<u8>,
}
impl Section {
    pub fn new(layer: usize, name: impl Into<String>, path: impl AsRef<Path>, data: &[u8]) -> Self {
        Self {
            layer,
            name: name.into(),
            path: path.as_ref().into(),
            data: data.into(),
        }
    }
}

#[derive(Debug)]
pub struct Rule {
    matcher: GlobMatcher,
    start: String,
    end: String,
}

/// Compiled custom rules precede the built-in comment syntaxes. Decisions are
/// bounded; changing file contents never invalidates a path-only rule decision.
pub struct Markers {
    rules: Vec<Arc<Rule>>,
    matches: Mutex<HashMap<PathBuf, Option<Arc<Rule>>>>,
}
impl Markers {
    pub fn new(custom: &[MarkerRule]) -> Result<Self> {
        let defaults = [
            (
                "*.{md,markdown,html,htm,xml,svg}",
                "<!-- BEGIN WORKSPACE-OVERLAY: {name} ({path}) -->",
                "<!-- END WORKSPACE-OVERLAY: {name} ({path}) -->",
            ),
            (
                "*.{toml,yaml,yml,sh,bash,zsh,py,rb,env,env.*,*ignore}",
                "# BEGIN WORKSPACE-OVERLAY: {name} ({path})",
                "# END WORKSPACE-OVERLAY: {name} ({path})",
            ),
            (
                "{Dockerfile,Makefile}*",
                "# BEGIN WORKSPACE-OVERLAY: {name} ({path})",
                "# END WORKSPACE-OVERLAY: {name} ({path})",
            ),
            (
                "*.{go,ts,js,tsx,jsx,rs,c,cpp,h,hpp,java,css,scss,less}",
                "// BEGIN WORKSPACE-OVERLAY: {name} ({path})",
                "// END WORKSPACE-OVERLAY: {name} ({path})",
            ),
        ];
        let mut rules = Vec::new();
        for rule in custom {
            rules.push(Arc::new(Rule {
                matcher: pattern(&rule.glob)?,
                start: rule.start.clone(),
                end: rule.end.clone(),
            }));
        }
        for (glob, start, end) in defaults {
            rules.push(Arc::new(Rule {
                matcher: pattern(glob)?,
                start: start.into(),
                end: end.into(),
            }));
        }
        Ok(Self {
            rules,
            matches: Mutex::new(HashMap::new()),
        })
    }
    pub fn find(&self, path: &Path) -> Option<Arc<Rule>> {
        if let Some(rule) = self.matches.lock().get(path) {
            return rule.clone();
        }
        let rule = self
            .rules
            .iter()
            .find(|r| {
                r.matcher.is_match(path)
                    || path
                        .file_name()
                        .is_some_and(|name| r.matcher.is_match(Path::new(name)))
            })
            .cloned();
        let mut matches = self.matches.lock();
        if matches.len() < MATCH_LIMIT {
            matches.insert(path.into(), rule.clone());
        }
        rule
    }
}

fn denied() -> io::Error {
    io::Error::from_raw_os_error(libc::EPERM)
}
fn too_large() -> io::Error {
    io::Error::from_raw_os_error(libc::EFBIG)
}

fn substitute(template: &str, section: &Section) -> Vec<u8> {
    // Unix filenames are byte strings. Replacing the path via lossy UTF-8 would
    // silently change marker ownership for valid non-UTF-8 source names.
    let text = template.replace("{name}", &section.name);
    let mut bytes = Vec::new();
    let mut rest = text.as_bytes();
    while let Some(at) = rest.windows(6).position(|p| p == b"{path}") {
        bytes.extend_from_slice(&rest[..at]);
        bytes.extend_from_slice(section.path.as_os_str().as_bytes());
        rest = &rest[at + 6..];
    }
    bytes.extend_from_slice(rest);
    bytes
}

impl Rule {
    pub fn delimiters(&self, section: &Section) -> (Vec<u8>, Vec<u8>) {
        (
            substitute(&self.start, section),
            substitute(&self.end, section),
        )
    }
    /// Calculate output size before allocating or reading source contents.
    pub fn size(&self, sections: &[(Section, u64, Option<u8>)]) -> io::Result<u64> {
        let mut size = 0u64;
        for (index, (section, length, tail)) in sections.iter().enumerate() {
            size = size.checked_add(*length).ok_or_else(too_large)?;
            if *length > 0 && *tail != Some(b'\n') && (index > 0 || sections.len() > 1) {
                size = size.checked_add(1).ok_or_else(too_large)?;
            }
            if index > 0 {
                let (start, end) = self.delimiters(section);
                size = size
                    .checked_add((start.len() + end.len() + 2) as u64)
                    .ok_or_else(too_large)?;
            }
        }
        Ok(size)
    }
    pub fn render(&self, sections: &[Section]) -> io::Result<Vec<u8>> {
        let first = sections.first().ok_or_else(denied)?;
        let mut output = Vec::new();
        let append = |output: &mut Vec<u8>, data: &[u8]| -> io::Result<()> {
            if output
                .len()
                .checked_add(data.len())
                .is_none_or(|n| n > MAX_MERGED_SIZE)
            {
                return Err(too_large());
            }
            output.extend_from_slice(data);
            Ok(())
        };
        append(&mut output, &first.data)?;
        for section in &sections[1..] {
            if output.last().is_some_and(|b| *b != b'\n') {
                append(&mut output, b"\n")?;
            }
            let (start, end) = self.delimiters(section);
            append(&mut output, &start)?;
            append(&mut output, b"\n")?;
            append(&mut output, &section.data)?;
            if section.data.last().is_some_and(|b| *b != b'\n') {
                append(&mut output, b"\n")?;
            }
            append(&mut output, &end)?;
            append(&mut output, b"\n")?;
        }
        Ok(output)
    }
    /// Recognize complete marker lines in source order. Unlike prefix matching,
    /// this cannot accept another overlay's name or a damaged path/suffix.
    pub fn parse(&self, data: &[u8], sections: &[Section]) -> io::Result<Vec<(usize, Vec<u8>)>> {
        let first = sections.first().ok_or_else(denied)?;
        if sections.len() == 1 {
            return Ok(vec![(first.layer, data.into())]);
        }
        let delimiters: Vec<_> = sections[1..].iter().map(|s| self.delimiters(s)).collect();
        let mut spans = Vec::new();
        let mut expected = 0;
        let mut opening = None;
        let mut offset = 0;
        for line in data.split_inclusive(|b| *b == b'\n') {
            let text = line.strip_suffix(b"\n").unwrap_or(line);
            if let Some((start, end)) = delimiters.get(expected) {
                if opening.is_none() && text == start {
                    if !line.ends_with(b"\n") {
                        return Err(denied());
                    }
                    opening = Some((offset, offset + line.len()));
                } else if opening.is_some() && text == end {
                    let (start, content) = opening.take().expect("open section");
                    spans.push((start, content, offset, offset + line.len()));
                    expected += 1;
                } else if delimiters
                    .iter()
                    .any(|(start, end)| text == start || text == end)
                {
                    return Err(denied());
                }
            } else if delimiters
                .iter()
                .any(|(start, end)| text == start || text == end)
            {
                return Err(denied());
            }
            offset += line.len();
        }
        if expected != delimiters.len() || opening.is_some() {
            return Err(denied());
        }
        let mut parsed = vec![(first.layer, data[..spans[0].0].into())];
        for (index, span) in spans.iter().enumerate() {
            let following = spans.get(index + 1).map_or(data.len(), |s| s.0);
            let mut contribution = data[span.1..span.2].to_vec();
            contribution.extend_from_slice(&data[span.3..following]);
            parsed.push((sections[index + 1].layer, contribution));
        }
        Ok(parsed)
    }
}
