use crate::sniff;
use cap_std::fs::Dir;
use std::{fs, io, os::unix::fs::PermissionsExt, path::Path};

#[test]
fn fresh_bytes_replacement_eviction_and_native_permissions() {
    fs::create_dir_all(".tmp").unwrap();
    let temp = tempfile::tempdir_in(".tmp").unwrap();
    let root = Dir::open_ambient_dir(temp.path(), cap_std::ambient_authority()).unwrap();
    let cache = sniff::Cache::new();
    for i in 0..80 {
        let name = format!("{i}.txt");
        root.write(&name, b"text").unwrap();
        let mut data = [0; 512];
        assert_eq!(
            cache
                .read(
                    &root,
                    0,
                    Path::new(&name),
                    &root.symlink_metadata(&name).unwrap(),
                    &mut data,
                    0
                )
                .unwrap(),
            4
        );
    }
    let name = Path::new("0.txt");
    let mut data = [0; 512];
    let read = |data: &mut [u8]| {
        cache.read(
            &root,
            0,
            name,
            &root.symlink_metadata(name).unwrap(),
            data,
            0,
        )
    };
    assert_eq!(read(&mut data).unwrap(), 4);
    root.write(name, b"\x00binary").unwrap();
    assert_eq!(read(&mut data).unwrap(), 7);
    assert!(sniff::binary(&data[..7]));
    root.write("replacement", b"replaced").unwrap();
    root.rename("replacement", &root, name).unwrap();
    assert_eq!(read(&mut data).unwrap(), 8);
    assert_eq!(&data[..8], b"replaced");
    fs::set_permissions(temp.path().join(name), fs::Permissions::from_mode(0o200)).unwrap();
    if unsafe { libc::geteuid() } != 0 {
        assert_eq!(
            read(&mut data).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }
    fs::set_permissions(temp.path().join(name), fs::Permissions::from_mode(0o600)).unwrap();
    cache.clear();
    assert_eq!(read(&mut data).unwrap(), 8);
    assert_eq!(
        cache
            .read(
                &root,
                0,
                name,
                &root.symlink_metadata(name).unwrap(),
                &mut data,
                999
            )
            .unwrap(),
        0
    );
}

#[test]
fn mime_magic_and_512_byte_text_policy() {
    for text in [
        b"".as_slice(),
        b"ordinary\n\ttext",
        b"\xef\xbb\xbf\0",
        b"\xff\xfe\0\0",
        b" <HtMl>\0",
        b"\n<?xml\0",
    ] {
        assert!(!sniff::binary(text), "{text:?}");
    }
    for data in [
        b"\0".as_slice(),
        b"text\x0b",
        b"\x0b<HTML>\0",
        b"%PDF-1.7",
        b"%!PS-Adobe-3.0",
        b"GIF89a",
        b"\x89PNG\r\n\x1a\n",
        b"PK\x03\x04",
    ] {
        assert!(sniff::binary(data), "{data:?}");
    }
    let mut text = vec![b'a'; 512];
    text.push(0);
    assert!(!sniff::binary(&text));
}
