//! Our EROFS images against erofs-utils. `fsck.erofs` must accept what we
//! write and extract exactly the tree we wrote, and we must read what
//! `mkfs.erofs` writes. Skipped when erofs-utils is not on PATH; the dev shell
//! and `nix flake check` have it.

use std::collections::BTreeMap;
use std::fs;
use std::io::Cursor;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gha_cache_fusefs::erofs::{self, Device, EntryKind, FileData, Image, Meta, Node, NodeKind};

fn tool(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

macro_rules! require {
    ($name:expr) => {
        match tool($name) {
            Some(path) => path,
            None => {
                eprintln!("skipping: {} is not on PATH", $name);
                return;
            }
        }
    };
}

fn run(cmd: &mut Command) -> String {
    let out = cmd.output().expect("the tool runs");
    let text =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{cmd:?} failed:\n{text}");
    text
}

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

fn meta(mode: u16, nsec: u32) -> Meta {
    Meta {
        mode,
        uid: 0,
        gid: 0,
        mtime: UNIX_EPOCH + Duration::new(1_790_000_000, nsec),
    }
}

/// What a tree should hold: a file's bytes, a symlink's target, or a directory.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Want {
    Dir(u32),
    File(u32, Vec<u8>),
    Symlink(String),
}

/// A tree with every kind of node our images hold, except whiteouts.
fn sample() -> BTreeMap<String, (Want, u32)> {
    let mut t = BTreeMap::new();
    t.insert("bin".to_string(), (Want::Dir(0o755), 1));
    t.insert(
        "bin/tool".into(),
        (Want::File(0o755, b"#!/bin/sh\necho hi\n".to_vec()), 2),
    );
    t.insert("data".into(), (Want::Dir(0o750), 3));
    for (i, n) in [0usize, 1, 1024, 1025, 4096, 4097, 300_000]
        .into_iter()
        .enumerate()
    {
        t.insert(
            format!("data/f{n}"),
            (Want::File(0o644, noise(n, i as u64)), 4 + i as u32),
        );
    }
    t.insert("many".into(), (Want::Dir(0o755), 20));
    for i in 0..500 {
        let name = format!("many/entry-with-a-longish-name-{i:04}");
        t.insert(name.clone(), (Want::File(0o600, name.into_bytes()), 21));
    }
    t.insert("link".into(), (Want::Symlink("data/f1".into()), 30));
    t.insert("far".into(), (Want::Symlink("x/".repeat(450) + "end"), 31));
    t.insert("empty".into(), (Want::Dir(0o700), 32));
    t
}

fn image_of(tree: &BTreeMap<String, (Want, u32)>) -> Image {
    let mut image = Image::new(meta(0o755, 0));
    for (path, (want, nsec)) in tree {
        let (kind, mode) = match want {
            Want::Dir(mode) => (NodeKind::Dir, *mode),
            Want::File(mode, data) => (
                NodeKind::File(FileData::Here {
                    len: data.len() as u64,
                    source: Box::new(Cursor::new(data.clone())),
                }),
                *mode,
            ),
            Want::Symlink(target) => (NodeKind::Symlink(target.clone()), 0o777),
        };
        let node = Node {
            meta: meta(mode as u16, *nsec),
            kind,
            xattrs: Vec::new(),
        };
        image.insert(path, node).unwrap();
    }
    image
}

/// A directory on disk, as a tree.
fn tree_on_disk(root: &Path) -> BTreeMap<String, (Want, SystemTime)> {
    let mut out = BTreeMap::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_str()
                .unwrap()
                .to_string();
            let md = fs::symlink_metadata(&path).unwrap();
            let mode = md.permissions().mode() & 0o7777;
            let want = if md.file_type().is_symlink() {
                Want::Symlink(fs::read_link(&path).unwrap().to_str().unwrap().into())
            } else if md.is_dir() {
                dirs.push(path.clone());
                Want::Dir(mode)
            } else {
                Want::File(mode, fs::read(&path).unwrap())
            };
            out.insert(rel, (want, md.modified().unwrap()));
        }
    }
    out
}

#[test]
fn fsck_extracts_what_we_wrote() {
    let fsck = require!("fsck.erofs");
    let dir = tempfile::tempdir().unwrap();
    let tree = sample();
    let image = dir.path().join("image.erofs");
    image_of(&tree)
        .write_to(&mut fs::File::create(&image).unwrap())
        .unwrap();
    let out = dir.path().join("out");
    run(Command::new(&fsck)
        .arg(format!("--extract={}", out.display()))
        .args(["--no-preserve-owner", "--preserve-perms"])
        .arg(&image));
    let got = tree_on_disk(&out);
    assert_eq!(got.len(), tree.len());
    for (path, (want, nsec)) in &tree {
        let (have, mtime) = &got[path];
        match want {
            // Extracted symlinks get the permissions the platform gives them.
            Want::Symlink(_) => assert_eq!(have, want, "{path}"),
            _ => {
                assert_eq!(have, want, "{path}");
                let expected = UNIX_EPOCH + Duration::new(1_790_000_000, *nsec);
                assert_eq!(*mtime, expected, "{path}: mtime");
            }
        }
    }
}

#[test]
fn fsck_accepts_whiteouts_and_reads_devices() {
    let fsck = require!("fsck.erofs");
    let dir = tempfile::tempdir().unwrap();
    let blob = noise((20 << 20) + 123, 7);
    let blob_path = dir.path().join("blob");
    fs::write(&blob_path, &blob).unwrap();

    let mut image = Image::new(meta(0o755, 0));
    let blocks = (blob.len() as u64).div_ceil(erofs::BLOCK);
    let dev = image
        .add_device(Device {
            tag: "b".repeat(64),
            blocks,
        })
        .unwrap();
    let big = NodeKind::File(FileData::Device {
        len: blob.len() as u64,
        device: dev,
        start: 0,
    });
    image
        .insert(
            "big",
            Node {
                meta: meta(0o644, 0),
                kind: big,
                xattrs: Vec::new(),
            },
        )
        .unwrap();
    let image_path = dir.path().join("image.erofs");
    image
        .write_to(&mut fs::File::create(&image_path).unwrap())
        .unwrap();
    let out = dir.path().join("out");
    run(Command::new(&fsck)
        .arg(format!("--device={}", blob_path.display()))
        .arg(format!("--extract={}", out.display()))
        .arg("--no-preserve-owner")
        .arg(&image_path));
    assert!(
        fs::read(out.join("big")).unwrap() == blob,
        "chunked data differs"
    );

    // Whiteouts are character devices, which only root may extract, and macOS
    // cannot make symlinks as long as `far`; fsck.erofs checks them instead.
    let mut image = image_of(&sample());
    let node = |kind, mode, xattrs| Node {
        meta: meta(mode, 0),
        kind,
        xattrs,
    };
    image
        .insert("data/gone", node(NodeKind::Whiteout, 0, Vec::new()))
        .unwrap();
    let far = "z/".repeat(2040) + "end";
    image
        .insert(
            "data/far",
            node(NodeKind::Symlink(far.clone()), 0o777, Vec::new()),
        )
        .unwrap();
    let keep = vec![("user.gha-fs.dir".to_string(), b"keep".to_vec())];
    image
        .insert("marked", node(NodeKind::Dir, 0o755, keep))
        .unwrap();
    let path = dir.path().join("whiteouts.erofs");
    image
        .write_to(&mut fs::File::create(&path).unwrap())
        .unwrap();
    run(Command::new(&fsck).arg("--extract").arg(&path));
    let listing = erofs::read(&fs::read(&path).unwrap()).unwrap();
    let find = |p: &str| listing.entries.iter().find(|e| e.path == p).unwrap();
    assert!(find("data/gone").is_whiteout());
    assert_eq!(find("data/far").kind, EntryKind::Symlink(far));
    assert_eq!(find("marked").xattr("user.gha-fs.dir"), Some(&b"keep"[..]));
}

#[test]
fn we_read_what_mkfs_writes() {
    let mkfs = require!("mkfs.erofs");
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    for d in ["a/b/c", "empty", "big"] {
        fs::create_dir_all(src.join(d)).unwrap();
    }
    for (i, n) in [0usize, 5, 3000, 4096, 10_000, 70_000]
        .into_iter()
        .enumerate()
    {
        fs::write(src.join(format!("a/f{n}")), noise(n, i as u64)).unwrap();
    }
    fs::write(src.join("a/b/c/deep"), b"deep").unwrap();
    for i in 0..400 {
        fs::write(src.join(format!("big/entry-{i:04}")), format!("{i}")).unwrap();
    }
    std::os::unix::fs::symlink("a/f5", src.join("link")).unwrap();
    std::os::unix::fs::symlink("y/".repeat(450) + "end", src.join("far")).unwrap();
    fs::set_permissions(src.join("a/f5"), fs::Permissions::from_mode(0o751)).unwrap();
    let want = tree_on_disk(&src);

    for flags in [&[][..], &["-Eforce-inode-extended"][..]] {
        let image = dir.path().join("mkfs.erofs");
        // Without xattrs: macOS adds some that are not ours to check. Nix
        // sets SOURCE_DATE_EPOCH, and mkfs.erofs would stamp it everywhere.
        run(Command::new(&mkfs)
            .env_remove("SOURCE_DATE_EPOCH")
            .args(flags)
            .arg("-x")
            .arg("-1")
            .arg(&image)
            .arg(&src));
        let bytes = fs::read(&image).unwrap();
        let listing = erofs::read(&bytes).unwrap();
        assert_eq!(listing.entries[0].path, "");
        let uid = fs::metadata(&src).unwrap().uid();
        let mut seen = 0;
        for e in &listing.entries[1..] {
            let (want, mtime) = &want[&e.path];
            let have = match &e.kind {
                EntryKind::Dir => Want::Dir(u32::from(e.meta.mode)),
                EntryKind::File => Want::File(
                    u32::from(e.meta.mode),
                    e.data.bytes(&bytes, e.size).unwrap(),
                ),
                EntryKind::Symlink(t) => Want::Symlink(t.clone()),
                other => panic!("{}: {other:?}", e.path),
            };
            assert_eq!(&have, want, "{} ({flags:?})", e.path);
            assert_eq!(e.meta.uid, uid);
            // mkfs.erofs keeps whole seconds.
            let secs = |t: SystemTime| t.duration_since(UNIX_EPOCH).unwrap().as_secs();
            assert_eq!(secs(e.meta.mtime), secs(*mtime), "{}: mtime", e.path);
            seen += 1;
        }
        assert_eq!(seen, want.len(), "{flags:?}");
    }
}
