//! A real kernel round-trip establishes mount permissions before filesystem integration.
use fuser::{Config, FileAttr, FileHandle, FileType, Filesystem, INodeNo, ReplyAttr, Request};
use std::{
    fs,
    os::unix::fs::MetadataExt,
    time::{Duration, SystemTime},
};

struct Root(FileAttr);
impl Filesystem for Root {
    fn getattr(&self, _: &Request, _: INodeNo, _: Option<FileHandle>, reply: ReplyAttr) {
        reply.attr(&Duration::ZERO, &self.0);
    }
}

#[test]
fn native_mount_getattr_and_ordinary_unmount() {
    fs::create_dir_all(".tmp").unwrap();
    let fixture = tempfile::tempdir_in(".tmp").unwrap();
    let target = fs::canonicalize(fixture.path()).unwrap();
    let metadata = fs::metadata(&target).unwrap();
    let attr = FileAttr {
        ino: INodeNo(1),
        size: metadata.size(),
        blocks: metadata.blocks(),
        atime: metadata.accessed().unwrap(),
        mtime: metadata.modified().unwrap(),
        ctime: SystemTime::UNIX_EPOCH,
        crtime: SystemTime::UNIX_EPOCH,
        kind: FileType::Directory,
        perm: metadata.mode() as u16 & 0o7777,
        nlink: metadata.nlink() as u32,
        uid: metadata.uid(),
        gid: metadata.gid(),
        rdev: metadata.rdev() as u32,
        blksize: metadata.blksize() as u32,
        flags: 0,
    };
    let session = fuser::Session::new(Root(attr), &target, &Config::default()).unwrap();
    let server = session.spawn().unwrap();
    assert!(fs::metadata(&target).unwrap().is_dir());
    let output = std::process::Command::new("fusermount3")
        .arg("-u")
        .arg("--")
        .arg(&target)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    server.join().unwrap();
}

#[test]
fn busy_native_mount_is_not_lazily_detached() {
    fs::create_dir_all(".tmp").unwrap();
    let fixture = tempfile::tempdir_in(".tmp").unwrap();
    let target = fs::canonicalize(fixture.path()).unwrap();
    let metadata = fs::metadata(&target).unwrap();
    let attr = FileAttr {
        ino: INodeNo(1),
        size: metadata.size(),
        blocks: metadata.blocks(),
        atime: metadata.accessed().unwrap(),
        mtime: metadata.modified().unwrap(),
        ctime: SystemTime::UNIX_EPOCH,
        crtime: SystemTime::UNIX_EPOCH,
        kind: FileType::Directory,
        perm: metadata.mode() as u16 & 0o7777,
        nlink: metadata.nlink() as u32,
        uid: metadata.uid(),
        gid: metadata.gid(),
        rdev: metadata.rdev() as u32,
        blksize: metadata.blksize() as u32,
        flags: 0,
    };
    let session = fuser::Session::new(Root(attr), &target, &Config::default()).unwrap();
    let server = session.spawn().unwrap();
    let held = fs::File::open(&target).unwrap();
    let (release, wait) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        let _ = wait.recv_timeout(Duration::from_secs(2));
        drop(held);
    });
    let outcome = server.umount_and_join();
    let _ = release.send(());
    holder.join().unwrap();
    // The upstream unmount helper used lazy detach and reported success.
    assert!(outcome.is_err(), "busy native mount must remain mounted");
    let output = std::process::Command::new("fusermount3")
        .arg("-u")
        .arg("--")
        .arg(&target)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
