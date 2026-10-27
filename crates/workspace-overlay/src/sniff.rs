use cap_std::fs::{Dir, Metadata, MetadataExt, OpenOptions, OpenOptionsExt};
use lru::LruCache;
use parking_lot::Mutex;
use std::{
    fs::File,
    io,
    num::NonZeroUsize,
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
    sync::Arc,
};

const FILE_LIMIT: usize = 64;
const SNIFF_LENGTH: usize = 512;
#[derive(PartialEq, Eq)]
struct Fingerprint {
    dev: u64,
    ino: u64,
    ctime: i64,
    nanos: i64,
    mode: u32,
    uid: u32,
    gid: u32,
}
impl From<&Metadata> for Fingerprint {
    fn from(info: &Metadata) -> Self {
        Self {
            dev: info.dev(),
            ino: info.ino(),
            ctime: info.ctime(),
            nanos: info.ctime_nsec(),
            mode: info.mode(),
            uid: info.uid(),
            gid: info.gid(),
        }
    }
}
struct Entry {
    fingerprint: Fingerprint,
    file: File,
}
pub(crate) struct Cache {
    files: Mutex<LruCache<(usize, PathBuf), Arc<Entry>>>,
}
impl Cache {
    pub(crate) fn new() -> Self {
        Self {
            files: Mutex::new(LruCache::new(
                NonZeroUsize::new(FILE_LIMIT).expect("positive descriptor limit"),
            )),
        }
    }
    pub(crate) fn clear(&self) {
        self.files.lock().clear();
    }
    pub(crate) fn read(
        &self,
        root: &Dir,
        layer: usize,
        path: &Path,
        info: &Metadata,
        bytes: &mut [u8],
        offset: u64,
    ) -> io::Result<usize> {
        let key = (layer, path.to_path_buf());
        let fingerprint = Fingerprint::from(info);
        let hit = self
            .files
            .lock()
            .get(&key)
            .filter(|entry| entry.fingerprint == fingerprint)
            .cloned();
        let entry = if let Some(hit) = hit {
            hit
        } else {
            // Opening outside the LRU lock keeps unrelated native reads concurrent.
            // Arc pins descriptors during eviction; only contents are read afresh.
            let mut options = OpenOptions::new();
            options.read(true).custom_flags(libc::O_NONBLOCK);
            let opened = Arc::new(Entry {
                fingerprint,
                file: root.open_with(path, &options)?.into_std(),
            });
            let mut files = self.files.lock();
            if let Some(hit) = files
                .get(&key)
                .filter(|entry| entry.fingerprint == opened.fingerprint)
            {
                hit.clone()
            } else {
                files.put(key.clone(), opened.clone());
                opened
            }
        };
        match entry.file.read_at(bytes, offset) {
            Ok(count) => Ok(count),
            Err(err) => {
                let mut files = self.files.lock();
                if files
                    .peek(&key)
                    .is_some_and(|current| Arc::ptr_eq(current, &entry))
                {
                    files.pop(&key);
                }
                Err(err)
            }
        }
    }
}

/// The original first-512-byte text policy, with native maintained magic
/// detection protecting recognized binary types. HTML/XML and BOM text have
/// priority, as in HTTP sniffing; unknown data is judged by control bytes.
pub(crate) fn binary(bytes: &[u8]) -> bool {
    let bytes = &bytes[..bytes.len().min(SNIFF_LENGTH)];
    let content = bytes.trim_ascii_start();
    let tags: [&[u8]; 17] = [
        b"<!DOCTYPE HTML",
        b"<HTML",
        b"<HEAD",
        b"<SCRIPT",
        b"<IFRAME",
        b"<H1",
        b"<DIV",
        b"<FONT",
        b"<TABLE",
        b"<A",
        b"<STYLE",
        b"<TITLE",
        b"<B",
        b"<BODY",
        b"<BR",
        b"<P",
        b"<!--",
    ];
    if content.starts_with(b"<?xml")
        || tags.iter().any(|tag| {
            content.len() > tag.len()
                && content[..tag.len()].eq_ignore_ascii_case(tag)
                && b" >".contains(&content[tag.len()])
        })
    {
        return false;
    }
    if bytes.len() >= 4
        && (bytes.starts_with(b"\xef\xbb\xbf")
            || bytes.starts_with(b"\xfe\xff")
            || bytes.starts_with(b"\xff\xfe"))
    {
        return false;
    }
    if bytes.starts_with(b"%!PS-Adobe-")
        || infer::get(bytes).is_some_and(|kind| !kind.mime_type().starts_with("text/"))
    {
        return true;
    }
    bytes.iter().any(|b| matches!(*b,0..=8|11|14..=26|28..=31))
}
