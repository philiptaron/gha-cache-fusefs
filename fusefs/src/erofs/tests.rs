use std::io::Cursor;
use std::time::{Duration, UNIX_EPOCH};

use super::*;

fn meta(mode: u16) -> Meta {
    Meta {
        mode,
        uid: 1001,
        gid: 118,
        mtime: UNIX_EPOCH + Duration::new(1_790_000_000, 123_456_789),
    }
}

fn node(kind: NodeKind, mode: u16) -> Node {
    Node {
        meta: meta(mode),
        kind,
        xattrs: Vec::new(),
    }
}

fn file(data: &[u8]) -> NodeKind {
    NodeKind::File(FileData::Here {
        len: data.len() as u64,
        source: Box::new(Cursor::new(data.to_vec())),
    })
}

/// Deterministic pseudo-random bytes.
fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

fn write(image: Image) -> (Vec<u8>, Written) {
    let mut out = Vec::new();
    let written = image.write_to(&mut out).unwrap();
    assert_eq!(out.len() as u64, written.blocks * BLOCK);
    (out, written)
}

fn entry<'a>(listing: &'a Listing, path: &str) -> &'a Entry {
    listing
        .entries
        .iter()
        .find(|e| e.path == path)
        .unwrap_or_else(|| panic!("{path:?} is missing"))
}

const SIZES: [usize; 9] = [0, 1, 1000, 1024, 1025, 4095, 4096, 4097, 3 << 20];

#[test]
fn a_tree_round_trips() {
    let mut image = Image::new(meta(0o755));
    let mut root = node(NodeKind::Dir, 0o755);
    root.xattrs
        .push(("user.gha-fs.writer".into(), b"test".to_vec()));
    image.insert("", root).unwrap();
    let mut kept = node(NodeKind::Dir, 0o750);
    kept.xattrs
        .push(("user.gha-fs.dir".into(), b"keep".to_vec()));
    kept.xattrs
        .push(("trusted.overlay.opaque".into(), b"y".to_vec()));
    image.insert("d", kept).unwrap();
    for (i, &n) in SIZES.iter().enumerate() {
        image
            .insert(&format!("d/f{n}"), node(file(&noise(n, i as u64)), 0o644))
            .unwrap();
    }
    image
        .insert("d/tool", node(file(b"#!/bin/sh\n"), 0o4755))
        .unwrap();
    image
        .insert("link", node(NodeKind::Symlink("d/f1".into()), 0))
        .unwrap();
    // Too long to fit next to its inode.
    let far = "x/".repeat(2040) + "end";
    image
        .insert("far", node(NodeKind::Symlink(far.clone()), 0))
        .unwrap();
    image.insert("gone", node(NodeKind::Whiteout, 0)).unwrap();
    for dir in ["a", "a/b", "a/b/c"] {
        image.insert(dir, node(NodeKind::Dir, 0o700)).unwrap();
    }
    image
        .insert("a/b/c/deep", node(file(b"deep"), 0o600))
        .unwrap();
    image.insert("empty", node(NodeKind::Dir, 0o755)).unwrap();

    let (bytes, written) = write(image);
    let listing = read(&bytes).unwrap();
    let paths: Vec<&str> = listing.entries.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(paths[0], "");
    assert_eq!(paths[1..5], ["a", "a/b", "a/b/c", "a/b/c/deep"]);
    assert_eq!(listing.entries.len(), 7 + SIZES.len() + 4);

    let root = entry(&listing, "");
    assert_eq!(root.kind, EntryKind::Dir);
    assert_eq!(root.xattr("user.gha-fs.writer"), Some(&b"test"[..]));
    assert_eq!(root.nlink, 2 + 3);
    let d = entry(&listing, "d");
    assert_eq!(d.meta, meta(0o750));
    assert_eq!(d.xattr("user.gha-fs.dir"), Some(&b"keep"[..]));
    assert_eq!(d.xattr("trusted.overlay.opaque"), Some(&b"y"[..]));
    for (i, &n) in SIZES.iter().enumerate() {
        let f = entry(&listing, &format!("d/f{n}"));
        assert_eq!(f.kind, EntryKind::File);
        assert_eq!(f.size, n as u64);
        assert_eq!(f.meta, meta(0o644));
        assert_eq!(
            f.data.bytes(&bytes, f.size).unwrap(),
            noise(n, i as u64),
            "d/f{n}"
        );
    }
    assert_eq!(entry(&listing, "d/tool").meta.mode, 0o4755);
    assert_eq!(
        entry(&listing, "link").kind,
        EntryKind::Symlink("d/f1".into())
    );
    assert_eq!(entry(&listing, "far").kind, EntryKind::Symlink(far));
    let gone = entry(&listing, "gone");
    assert!(gone.is_whiteout());
    assert_eq!(gone.data, Data::None);
    assert_eq!(entry(&listing, "empty").kind, EntryKind::Dir);

    // The metadata alone lists the tree; only file blocks are elsewhere.
    let meta_only = &bytes[..(written.meta_blocks * BLOCK) as usize];
    let again = read(meta_only).unwrap();
    assert_eq!(again.entries, listing.entries);
    let big = entry(&again, &format!("d/f{}", 3 << 20));
    assert!(big.data.bytes(meta_only, big.size).is_err());
    // Files up to INLINE_MAX need no blocks at all.
    let small = entry(&again, "d/f1024");
    assert_eq!(
        small.data.bytes(meta_only, small.size).unwrap(),
        noise(1024, 3)
    );
}

#[test]
fn file_data_follows_the_metadata_in_path_order() {
    let mut image = Image::new(meta(0o755));
    for name in ["c", "a", "b"] {
        image
            .insert(name, node(file(&[name.as_bytes()[0]; 5000]), 0o644))
            .unwrap();
    }
    let (bytes, written) = write(image);
    let listing = read(&bytes).unwrap();
    let starts: Vec<u64> = ["a", "b", "c"]
        .iter()
        .map(|p| match &entry(&listing, p).data {
            Data::Flat { start, .. } => *start,
            other => panic!("{other:?}"),
        })
        .collect();
    let m = written.meta_blocks;
    assert_eq!(starts, [m, m + 2, m + 4]);
}

#[test]
fn large_directories_take_several_blocks() {
    let mut image = Image::new(meta(0o755));
    image.insert("many", node(NodeKind::Dir, 0o755)).unwrap();
    let names: Vec<String> = (0..600)
        .map(|i| format!("{i:05}-a-longish-file-name"))
        .collect();
    for name in &names {
        image
            .insert(&format!("many/{name}"), node(file(name.as_bytes()), 0o644))
            .unwrap();
    }
    let (bytes, written) = write(image);
    let dir = read(&bytes).unwrap();
    let many = entry(&dir, "many");
    assert!(many.size > 4 * BLOCK, "{} bytes of dirents", many.size);
    let listed: Vec<&str> = dir
        .entries
        .iter()
        .filter_map(|e| e.path.strip_prefix("many/"))
        .collect();
    assert_eq!(listed, names);
    // The directory's blocks are part of the metadata.
    let again = read(&bytes[..(written.meta_blocks * BLOCK) as usize]).unwrap();
    assert_eq!(again.entries.len(), dir.entries.len());
}

#[test]
fn files_can_live_on_devices() {
    let mut image = Image::new(meta(0o755));
    let tag = "a".repeat(64);
    let blocks = (40u64 << 20) / BLOCK;
    let dev = image
        .add_device(Device {
            tag: tag.clone(),
            blocks,
        })
        .unwrap();
    assert_eq!(dev, 1);
    image
        .insert(
            "big",
            node(
                NodeKind::File(FileData::Device {
                    len: 40 << 20,
                    device: dev,
                    start: 0,
                }),
                0o644,
            ),
        )
        .unwrap();
    let (bytes, _) = write(image);
    let listing = read(&bytes).unwrap();
    assert_eq!(listing.devices, [(tag, blocks)]);
    let big = entry(&listing, "big");
    assert_eq!(big.size, 40 << 20);
    let chunk = 16 << 20;
    let expected: Vec<Chunk> = [0, 4096, 8192]
        .iter()
        .map(|&start| Chunk {
            device: 1,
            start: Some(start),
        })
        .collect();
    assert_eq!(
        big.data,
        Data::Chunks {
            chunk_size: chunk,
            chunks: expected
        }
    );
}

#[test]
fn bad_trees_are_refused() {
    let mut image = Image::new(meta(0o755));
    for bad in ["a//b", ".", "a/..", "/a", "a/"] {
        assert!(
            image.insert(bad, node(NodeKind::Dir, 0)).is_err(),
            "{bad:?}"
        );
    }
    assert!(image.insert("", node(file(b"x"), 0)).is_err());

    let mut orphan = Image::new(meta(0o755));
    orphan.insert("no/parent", node(file(b"x"), 0o644)).unwrap();
    assert!(orphan.write_to(&mut Vec::new()).is_err());

    let mut under_file = Image::new(meta(0o755));
    under_file.insert("f", node(file(b"x"), 0o644)).unwrap();
    under_file.insert("f/g", node(file(b"x"), 0o644)).unwrap();
    assert!(under_file.write_to(&mut Vec::new()).is_err());

    let mut prefix = Image::new(meta(0o755));
    let mut n = node(file(b"x"), 0o644);
    n.xattrs.push(("com.apple.quarantine".into(), Vec::new()));
    prefix.insert("f", n).unwrap();
    assert!(prefix.write_to(&mut Vec::new()).is_err());

    let mut no_device = Image::new(meta(0o755));
    let f = NodeKind::File(FileData::Device {
        len: 1,
        device: 1,
        start: 0,
    });
    no_device.insert("f", node(f, 0o644)).unwrap();
    assert!(no_device.write_to(&mut Vec::new()).is_err());

    let mut short = Image::new(meta(0o755));
    let lying = NodeKind::File(FileData::Here {
        len: 5000,
        source: Box::new(Cursor::new(vec![0u8; 10])),
    });
    short.insert("f", node(lying, 0o644)).unwrap();
    assert!(short.write_to(&mut Vec::new()).is_err());
}

#[test]
fn damaged_images_are_errors_not_panics() {
    let mut image = Image::new(meta(0o755));
    image.insert("d", node(NodeKind::Dir, 0o755)).unwrap();
    for i in 0..50 {
        image
            .insert(
                &format!("d/{i}"),
                node(file(&noise(i * 97, i as u64)), 0o644),
            )
            .unwrap();
    }
    let (bytes, written) = write(image);
    assert!(matches!(read(&[]), Err(Error::NotErofs)));
    assert!(matches!(read(&bytes[..1100]), Err(Error::NotErofs)));
    let meta_len = (written.meta_blocks * BLOCK) as usize;
    for cut in (1152..meta_len).step_by(97) {
        // Truncated metadata either lists less or fails; it never panics.
        let _ = read(&bytes[..cut]);
    }
    let mut flipped = bytes.clone();
    for i in (1152..meta_len).step_by(13) {
        flipped[i] ^= 0x5a;
        let _ = read(&flipped);
    }
}
