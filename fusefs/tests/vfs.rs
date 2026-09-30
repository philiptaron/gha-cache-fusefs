//! The filesystem core against the fake cache service: every test plays
//! several "jobs" that mount the same cache one after another.

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gha_cache_fusefs::api::Api;
use gha_cache_fusefs::data::DataStore;
use gha_cache_fusefs::entry::{Kind, Version, Volume};
use gha_cache_fusefs::erofs;
use gha_cache_fusefs::fake::{FakeConfig, FakeServer, RateLimit};
use gha_cache_fusefs::index;
use gha_cache_fusefs::vfs::{
    Attr, Errno, FileKind, FsyncMode, Ino, ROOT, RenameMode, SetAttr, SetTime, Vfs, VfsConfig,
};

const MAIN: &str = "refs/heads/main";
const FEATURE: &str = "refs/heads/feature";

struct Job {
    vfs: Vfs,
    _dir: tempfile::TempDir,
}

impl std::ops::Deref for Job {
    type Target = Vfs;
    fn deref(&self) -> &Vfs {
        &self.vfs
    }
}

async fn job_with(server: &FakeServer, git_ref: &str, tweak: impl FnOnce(&mut VfsConfig)) -> Job {
    let env = server.env(git_ref, &[]);
    let api = Api::new(&env).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let store = DataStore::new(&dir.path().join("data"), 1 << 30, 8).unwrap();
    let mut cfg = VfsConfig::new(Volume::new("default").unwrap());
    cfg.settle = Duration::from_millis(20);
    cfg.refresh = None;
    tweak(&mut cfg);
    let vfs = Vfs::load(cfg, api, store, env.scopes()).await.unwrap();
    Job { vfs, _dir: dir }
}

async fn job(server: &FakeServer, git_ref: &str) -> Job {
    job_with(server, git_ref, |_| {}).await
}

/// A job that commits only when it unmounts, so in one layer.
async fn patient_job(server: &FakeServer, git_ref: &str) -> Job {
    job_with(server, git_ref, |c| c.settle = Duration::from_secs(3600)).await
}

async fn server() -> FakeServer {
    FakeServer::start(FakeConfig::default()).await.unwrap()
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

fn write_file(vfs: &Vfs, parent: Ino, name: &str, data: &[u8]) -> Ino {
    let (attr, fh) = vfs
        .create(parent, name, 0o644, libc::O_WRONLY | libc::O_CREAT)
        .unwrap();
    let mut off = 0;
    for chunk in data.chunks(128 * 1024) {
        assert_eq!(vfs.write(fh, off, chunk).unwrap() as usize, chunk.len());
        off += chunk.len() as u64;
    }
    vfs.release(fh).unwrap();
    attr.ino
}

async fn read_file(vfs: &Vfs, ino: Ino) -> Vec<u8> {
    let (fh, _) = vfs.open(ino, libc::O_RDONLY).await.unwrap();
    let mut out = Vec::new();
    loop {
        let chunk = vfs.read(fh, out.len() as u64, 128 * 1024).await.unwrap();
        if chunk.is_empty() {
            break;
        }
        out.extend(chunk);
    }
    vfs.release(fh).unwrap();
    out
}

async fn lookup(vfs: &Vfs, path: &str) -> Result<Attr, Errno> {
    let mut attr = vfs.getattr(ROOT)?;
    for part in path.split('/').filter(|p| !p.is_empty()) {
        attr = vfs.lookup(attr.ino, part).await?;
    }
    Ok(attr)
}

async fn cat(vfs: &Vfs, path: &str) -> Vec<u8> {
    let attr = lookup(vfs, path)
        .await
        .unwrap_or_else(|e| panic!("{path}: {e}"));
    read_file(vfs, attr.ino).await
}

fn ls(vfs: &Vfs, dir: Ino) -> Vec<String> {
    let fh = vfs.opendir(dir).unwrap();
    let names = vfs
        .readdir(dir, fh, 0)
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .filter(|n| n != "." && n != "..")
        .collect();
    vfs.releasedir(fh);
    names
}

async fn ls_path(vfs: &Vfs, path: &str) -> Vec<String> {
    ls(vfs, lookup(vfs, path).await.unwrap().ino)
}

async fn drained(vfs: &Vfs) {
    let summary = vfs.drain().await;
    assert!(
        summary.failures.is_empty(),
        "failures: {:?}",
        summary.failures
    );
}

/// The keys of the default volume's layers or blobs, with their sizes.
fn entries(server: &FakeServer, kind: &str) -> Vec<(String, u64)> {
    let prefix = format!("gha-fs/default/{kind}/");
    let mut k: Vec<(String, u64)> = server
        .entries()
        .into_iter()
        .filter(|(k, _, _)| k.starts_with(&prefix))
        .map(|(k, _, size)| (k, size))
        .collect();
    k.sort();
    k
}

fn layers(server: &FakeServer) -> usize {
    entries(server, "layer").len()
}

fn blobs(server: &FakeServer) -> usize {
    entries(server, "blob").len()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn files_persist_across_jobs() {
    let server = server().await;
    let a = patient_job(&server, MAIN).await;
    write_file(&a, ROOT, "hello.txt", b"hello, world\n");
    let d = a.mkdir(ROOT, "d", 0o755).unwrap();
    let big = noise(3 << 20, 1);
    write_file(&a, d.ino, "x.bin", &big);
    // Visible and readable in the writing job before anything is uploaded.
    assert_eq!(cat(&a, "hello.txt").await, b"hello, world\n");
    drained(&a).await;
    assert_eq!((layers(&server), blobs(&server)), (1, 0));

    let b = job(&server, MAIN).await;
    assert_eq!(ls(&b, ROOT), ["d", "hello.txt"]);
    let attr = lookup(&b, "d/x.bin").await.unwrap();
    assert_eq!(attr.size, big.len() as u64);
    assert_eq!(attr.kind, FileKind::File);
    assert_eq!(attr.perm, 0o644);
    assert_eq!(cat(&b, "hello.txt").await, b"hello, world\n");
    assert_eq!(cat(&b, "d/x.bin").await, big);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn empty_files_symlinks_and_metadata() {
    let server = server().await;
    let a = job(&server, MAIN).await;
    let empty = write_file(&a, ROOT, "empty", b"");
    let script = write_file(&a, ROOT, "run.sh", b"#!/bin/sh\necho hi\n");
    let mtime = UNIX_EPOCH + Duration::new(1_700_000_000, 123_456_789);
    a.setattr(
        script,
        None,
        SetAttr {
            mode: Some(0o755),
            mtime: Some(SetTime::At(mtime)),
            ..SetAttr::default()
        },
    )
    .await
    .unwrap();
    a.symlink(ROOT, "link", "run.sh").unwrap();
    assert_eq!(a.getattr(empty).unwrap().size, 0);
    drained(&a).await;

    let b = job(&server, MAIN).await;
    let e = lookup(&b, "empty").await.unwrap();
    assert_eq!(e.size, 0);
    assert!(read_file(&b, e.ino).await.is_empty());
    let s = lookup(&b, "run.sh").await.unwrap();
    assert_eq!(s.perm, 0o755);
    assert_eq!(s.mtime, mtime);
    let l = lookup(&b, "link").await.unwrap();
    assert_eq!(l.kind, FileKind::Symlink);
    assert_eq!(b.readlink(l.ino).await.unwrap(), "run.sh");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_last_writer_wins() {
    let server = server().await;
    let a = job(&server, MAIN).await;
    write_file(&a, ROOT, "f", b"one");
    drained(&a).await;

    let b = job(&server, MAIN).await;
    let f = lookup(&b, "f").await.unwrap();
    let (fh, _) = b.open(f.ino, libc::O_WRONLY | libc::O_TRUNC).await.unwrap();
    b.write(fh, 0, b"two!").unwrap();
    b.release(fh).unwrap();
    assert_eq!(cat(&b, "f").await, b"two!");
    drained(&b).await;

    let c = job(&server, MAIN).await;
    assert_eq!(cat(&c, "f").await, b"two!");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn open_for_write_without_truncate_copies_first() {
    let server = server().await;
    let a = job(&server, MAIN).await;
    write_file(&a, ROOT, "f", b"abcdef");
    drained(&a).await;

    let b = job(&server, MAIN).await;
    let f = lookup(&b, "f").await.unwrap();
    let (fh, _) = b.open(f.ino, libc::O_RDWR).await.unwrap();
    b.write(fh, 2, b"XY").unwrap();
    b.release(fh).unwrap();
    drained(&b).await;

    let c = job(&server, MAIN).await;
    assert_eq!(cat(&c, "f").await, b"abXYef");
    let f = lookup(&c, "f").await.unwrap();
    c.setattr(
        f.ino,
        None,
        SetAttr {
            size: Some(3),
            ..SetAttr::default()
        },
    )
    .await
    .unwrap();
    drained(&c).await;
    let d = job(&server, MAIN).await;
    assert_eq!(cat(&d, "f").await, b"abX");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn branches_overlay_the_default_branch() {
    let server = server().await;
    let main = job(&server, MAIN).await;
    write_file(&main, ROOT, "shared", b"from main");
    write_file(&main, ROOT, "doomed", b"from main");
    drained(&main).await;

    let feature = job(&server, FEATURE).await;
    assert_eq!(cat(&feature, "shared").await, b"from main");
    let shared = lookup(&feature, "shared").await.unwrap();
    let (fh, _) = feature
        .open(shared.ino, libc::O_WRONLY | libc::O_TRUNC)
        .await
        .unwrap();
    feature.write(fh, 0, b"from feature").unwrap();
    feature.release(fh).unwrap();
    feature.unlink(ROOT, "doomed").unwrap();
    drained(&feature).await;

    let feature2 = job(&server, FEATURE).await;
    assert_eq!(cat(&feature2, "shared").await, b"from feature");
    assert_eq!(
        lookup(&feature2, "doomed").await.unwrap_err(),
        Errno(libc::ENOENT)
    );
    assert_eq!(ls(&feature2, ROOT), ["shared"]);

    // The default branch never sees the feature branch's changes.
    let main2 = job(&server, MAIN).await;
    assert_eq!(cat(&main2, "shared").await, b"from main");
    assert_eq!(cat(&main2, "doomed").await, b"from main");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn write_then_rename_uploads_only_the_final_name() {
    let server = server().await;
    let a = job_with(&server, MAIN, |c| c.settle = Duration::from_secs(5)).await;
    write_file(&a, ROOT, "out.tmp.1234", b"payload");
    a.rename(ROOT, "out.tmp.1234", ROOT, "out", RenameMode::Replace)
        .await
        .unwrap();
    assert_eq!(cat(&a, "out").await, b"payload");
    let summary = a.drain().await;
    assert_eq!((summary.uploaded_files, summary.whiteouts), (1, 0));
    assert_eq!(layers(&server), 1);
    let b = job(&server, MAIN).await;
    assert_eq!(cat(&b, "out").await, b"payload");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn renaming_committed_and_remote_files() {
    let server = server().await;
    let a = job(&server, MAIN).await;
    // Too large to be inline in the layer's metadata.
    let content = noise(5000, 9);
    write_file(&a, ROOT, "f", &content);
    write_file(&a, ROOT, "tiny", b"inline");
    drained(&a).await;

    // Still cached locally: the rename re-uploads under the new name.
    let b = job(&server, MAIN).await;
    assert_eq!(cat(&b, "f").await, content);
    b.rename(ROOT, "f", ROOT, "g", RenameMode::Replace)
        .await
        .unwrap();
    // Small files arrive with the metadata, so they are always at hand.
    b.rename(ROOT, "tiny", ROOT, "small", RenameMode::Replace)
        .await
        .unwrap();
    drained(&b).await;
    let c = job(&server, MAIN).await;
    assert_eq!(ls(&c, ROOT), ["g", "small"]);
    assert_eq!(cat(&c, "g").await, content);
    assert_eq!(cat(&c, "small").await, b"inline");

    // Not cached: the new layer refers to the data where it is.
    let d = job(&server, MAIN).await;
    lookup(&d, "g").await.unwrap();
    d.rename(ROOT, "g", ROOT, "h", RenameMode::Replace)
        .await
        .unwrap();
    d.rename(ROOT, "h", ROOT, "g", RenameMode::Replace)
        .await
        .unwrap();
    write_file(&d, ROOT, "x", b"1");
    assert_eq!(
        d.rename(ROOT, "x", ROOT, "g", RenameMode::NoReplace)
            .await
            .unwrap_err(),
        Errno(libc::EEXIST)
    );

    // A symlink's target comes with the layer, so renaming one never needs
    // the network.
    d.symlink(ROOT, "link", "g").unwrap();
    drained(&d).await;
    let e = job(&server, MAIN).await;
    lookup(&e, "link").await.unwrap();
    e.rename(ROOT, "link", ROOT, "moved", RenameMode::Replace)
        .await
        .unwrap();
    drained(&e).await;
    let f = job(&server, MAIN).await;
    let l = lookup(&f, "moved").await.unwrap();
    assert_eq!(f.readlink(l.ino).await.unwrap(), "g");
    assert!(lookup(&f, "link").await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn directories_persist_when_empty_and_vanish_when_removed() {
    let server = server().await;
    let a = job(&server, MAIN).await;
    a.mkdir(ROOT, "empty", 0o700).unwrap();
    let p = a.mkdir(ROOT, "p", 0o755).unwrap();
    let q = a.mkdir(p.ino, "q", 0o755).unwrap();
    write_file(&a, q.ino, "f", b"x");
    let summary = a.drain().await;
    assert_eq!(summary.dir_markers, 3, "one keep per mkdir");

    let b = job(&server, MAIN).await;
    assert_eq!(ls(&b, ROOT), ["empty", "p"]);
    assert_eq!(lookup(&b, "empty").await.unwrap().perm, 0o700);
    // Emptying a directory keeps it; removing it removes it.
    let q = lookup(&b, "p/q").await.unwrap();
    b.unlink(q.ino, "f").unwrap();
    b.rmdir(ROOT, "empty").unwrap();
    assert_eq!(b.rmdir(ROOT, "p").unwrap_err(), Errno(libc::ENOTEMPTY));
    drained(&b).await;

    let c = job(&server, MAIN).await;
    assert_eq!(ls(&c, ROOT), ["p"]);
    assert!(ls_path(&c, "p/q").await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn renaming_a_local_directory() {
    let server = server().await;
    let a = patient_job(&server, MAIN).await;
    let d = a.mkdir(ROOT, "d", 0o755).unwrap();
    write_file(&a, d.ino, "a", b"A");
    let sub = a.mkdir(d.ino, "sub", 0o755).unwrap();
    write_file(&a, sub.ino, "b", b"B");
    a.rename(ROOT, "d", ROOT, "e", RenameMode::Replace)
        .await
        .unwrap();
    drained(&a).await;

    let b = job(&server, MAIN).await;
    assert_eq!(ls(&b, ROOT), ["e"]);
    assert_eq!(ls_path(&b, "e").await, ["a", "sub"]);
    assert_eq!(cat(&b, "e/sub/b").await, b"B");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn renaming_a_committed_directory() {
    let server = server().await;
    let a = job(&server, MAIN).await;
    let d = a.mkdir(ROOT, "d", 0o750).unwrap();
    let big = noise(5000, 3);
    write_file(&a, d.ino, "big", &big);
    write_file(&a, d.ino, "tiny", b"inline");
    a.symlink(d.ino, "link", "big").unwrap();
    let sub = a.mkdir(d.ino, "sub", 0o755).unwrap();
    a.mkdir(sub.ino, "empty", 0o700).unwrap();
    write_file(&a, sub.ino, "f", b"F");
    drained(&a).await;

    // Files that are cached, inline, or symlinks move with their directory,
    // and so do empty directories.
    let b = job(&server, MAIN).await;
    assert_eq!(cat(&b, "d/big").await, big);
    let before = b.summary().requests.blob;
    b.rename(ROOT, "d", ROOT, "e", RenameMode::Replace)
        .await
        .unwrap();
    assert_eq!(ls(&b, ROOT), ["e"]);
    let summary = b.drain().await;
    assert!(summary.failures.is_empty(), "{:?}", summary.failures);
    assert_eq!(summary.layers, 1, "{summary:?}");
    assert_eq!(
        summary.requests.blob - before,
        1,
        "no download: {summary:?}"
    );

    let c = job(&server, MAIN).await;
    assert_eq!(ls(&c, ROOT), ["e"]);
    assert_eq!(ls_path(&c, "e").await, ["big", "link", "sub", "tiny"]);
    assert_eq!(cat(&c, "e/big").await, big);
    assert_eq!(cat(&c, "e/tiny").await, b"inline");
    assert_eq!(cat(&c, "e/sub/f").await, b"F");
    let link = lookup(&c, "e/link").await.unwrap();
    assert_eq!(c.readlink(link.ino).await.unwrap(), "big");
    assert!(ls_path(&c, "e/sub/empty").await.is_empty());
    assert_eq!(lookup(&c, "e").await.unwrap().perm, 0o750);
    assert_eq!(lookup(&c, "e/sub/empty").await.unwrap().perm, 0o700);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn moving_remote_files_downloads_nothing() {
    let server = server().await;
    let a = job(&server, MAIN).await;
    let d = a.mkdir(ROOT, "d", 0o755).unwrap();
    let (small, large) = (noise(5000, 5), noise(9 << 20, 6));
    write_file(&a, d.ino, "small", &small);
    write_file(&a, d.ino, "large", &large);
    write_file(&a, d.ino, "tiny", b"inline");
    write_file(&a, ROOT, "alone", &small);
    drained(&a).await;

    // Nothing is cached here: the new layer refers to the data in the old
    // layer and the blob.
    let b = job(&server, MAIN).await;
    let mounted = b.summary().requests;
    b.rename(ROOT, "d", ROOT, "e", RenameMode::Replace)
        .await
        .unwrap();
    b.rename(ROOT, "alone", ROOT, "moved", RenameMode::Replace)
        .await
        .unwrap();
    let summary = b.drain().await;
    assert!(summary.failures.is_empty(), "{:?}", summary.failures);
    assert_eq!((summary.layers, summary.blobs), (1, 0));
    assert_eq!(summary.download_requests, 0, "{summary:?}");
    // Creating, uploading, and finalizing the layer.
    let r = summary.requests;
    assert_eq!(
        (
            r.cache_service - mounted.cache_service,
            r.blob - mounted.blob
        ),
        (2, 1),
        "{summary:?}"
    );
    let smallest = entries(&server, "layer")
        .iter()
        .map(|(_, size)| *size)
        .min();
    assert!(smallest.unwrap() <= 16 << 10, "metadata only");

    let c = job(&server, MAIN).await;
    assert_eq!(ls(&c, ROOT), ["e", "moved"]);
    assert_eq!(cat(&c, "e/small").await, small);
    assert_eq!(cat(&c, "e/large").await, large);
    assert_eq!(cat(&c, "e/tiny").await, b"inline");
    assert_eq!(cat(&c, "moved").await, small);

    // Moving it again refers to the same data, not to the layer that moved
    // it first.
    c.rename(ROOT, "e", ROOT, "f", RenameMode::Replace)
        .await
        .unwrap();
    drained(&c).await;
    let dd = job(&server, MAIN).await;
    assert_eq!(cat(&dd, "f/small").await, small);
    assert_eq!(cat(&dd, "f/large").await, large);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_moved_file_whose_layer_is_gone_is_gone() {
    let server = server().await;
    let a = job(&server, MAIN).await;
    write_file(&a, ROOT, "f", &noise(5000, 7));
    drained(&a).await;
    let first = entries(&server, "layer").remove(0).0;
    let b = job(&server, MAIN).await;
    b.rename(ROOT, "f", ROOT, "g", RenameMode::Replace)
        .await
        .unwrap();
    write_file(&b, ROOT, "other", b"still here");
    drained(&b).await;
    server.remove(&first, MAIN);
    let c = job(&server, MAIN).await;
    assert_eq!(ls(&c, ROOT), ["other"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn chmod_of_a_remote_file_downloads_nothing() {
    let server = server().await;
    let a = job(&server, MAIN).await;
    write_file(&a, ROOT, "run.sh", &noise(5000, 8));
    drained(&a).await;
    let b = job(&server, MAIN).await;
    let f = lookup(&b, "run.sh").await.unwrap();
    let when = UNIX_EPOCH + Duration::from_secs(1_600_000_000);
    b.setattr(
        f.ino,
        None,
        SetAttr {
            mode: Some(0o755),
            mtime: Some(SetTime::At(when)),
            ..SetAttr::default()
        },
    )
    .await
    .unwrap();
    let summary = b.drain().await;
    assert_eq!(summary.download_requests, 0, "{summary:?}");
    let c = job(&server, MAIN).await;
    let f = lookup(&c, "run.sh").await.unwrap();
    assert_eq!((f.perm, f.mtime), (0o755, when));
    assert_eq!(cat(&c, "run.sh").await, noise(5000, 8));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn renaming_a_directory_this_job_already_saved() {
    let server = server().await;
    let a = job(&server, MAIN).await;
    let out = a.mkdir(ROOT, "out.tmp", 0o755).unwrap();
    write_file(&a, out.ino, "result", &noise(5000, 4));
    a.mkdir(ROOT, "empty", 0o755).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while a.pending() > 0 {
        assert!(Instant::now() < deadline, "never committed");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // The layer it sealed is its cache, so nothing needs downloading.
    a.rename(ROOT, "out.tmp", ROOT, "out", RenameMode::Replace)
        .await
        .unwrap();
    a.rename(ROOT, "empty", ROOT, "still-empty", RenameMode::Replace)
        .await
        .unwrap();
    drained(&a).await;
    let b = job(&server, MAIN).await;
    assert_eq!(ls(&b, ROOT), ["out", "still-empty"]);
    assert_eq!(cat(&b, "out/result").await, noise(5000, 4));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn large_files_use_block_uploads_and_ranged_reads() {
    let server = server().await;
    let a = job(&server, MAIN).await;
    let data = noise(40 << 20, 7);
    write_file(&a, ROOT, "big", &data);
    drained(&a).await;
    let blob = &entries(&server, "blob")[0];
    assert_eq!(blob.1, data.len() as u64);

    let b = job(&server, MAIN).await;
    let f = lookup(&b, "big").await.unwrap();
    let (fh, _) = b.open(f.ino, libc::O_RDONLY).await.unwrap();
    for (offset, len) in [
        (0u64, 10u32),
        (13 << 20, 4096),
        ((40 << 20) - 5, 100),
        (1 << 20, 3 << 20),
    ] {
        let got = b.read(fh, offset, len).await.unwrap();
        let end = (offset as usize + len as usize).min(data.len());
        assert_eq!(got, &data[offset as usize..end], "read at {offset}");
    }
    b.release(fh).unwrap();
    assert_eq!(read_file(&b, f.ino).await, data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transient_service_failures_are_retried() {
    let server = server().await;
    server.set_config(|c| c.fail_blob_every = Some(3));
    let a = job(&server, MAIN).await;
    for i in 0..10 {
        write_file(&a, ROOT, &format!("f{i}"), &noise(100_000, i));
    }
    write_file(&a, ROOT, "big", &noise(20 << 20, 99));
    drained(&a).await;
    let b = job(&server, MAIN).await;
    for i in 0..10 {
        assert_eq!(cat(&b, &format!("f{i}")).await, noise(100_000, i));
    }
    assert_eq!(cat(&b, "big").await, noise(20 << 20, 99));
    assert!(server.blob_requests() > 20);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rate_limited_creations_wait_and_are_reported() {
    let server = FakeServer::start(FakeConfig {
        create_limit: Some(RateLimit {
            calls: 1,
            window: Duration::from_secs(2),
        }),
        ..FakeConfig::default()
    })
    .await
    .unwrap();
    let a = job_with(&server, MAIN, |c| c.fsync = FsyncMode::Commit).await;
    // Each fsync commits a layer of its own.
    for i in 0..2 {
        let (_, fh) = a
            .create(ROOT, &format!("f{i}"), 0o644, libc::O_WRONLY)
            .unwrap();
        a.write(fh, 0, b"x").unwrap();
        a.fsync(fh).await.unwrap();
        a.release(fh).unwrap();
    }
    let summary = a.drain().await;
    assert!(summary.failures.is_empty(), "{:?}", summary.failures);
    assert_eq!((summary.uploaded_files, summary.layers), (2, 2));
    assert!(summary.rate_limited >= 1);
    assert!(summary.rate_limit_pause_ms >= 1000, "{summary:?}");
    // Create and finalize for each layer, plus the refused creations.
    assert_eq!(summary.requests.cache_service, 4 + summary.rate_limited);
    assert_eq!(summary.requests.blob, 2);
    assert_eq!(summary.requests.rest, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rate_limit_on_creating_entries_does_not_hold_up_reads() {
    let server = FakeServer::start(FakeConfig {
        create_limit: Some(RateLimit {
            calls: 2,
            window: Duration::from_secs(5),
        }),
        ..FakeConfig::default()
    })
    .await
    .unwrap();
    let a = job(&server, MAIN).await;
    // Too large to arrive inline with the layer's metadata.
    write_file(&a, ROOT, "target", &noise(5000, 1));
    drained(&a).await;

    let b = job(&server, MAIN).await;
    let target = lookup(&b, "target").await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    for i in 0.. {
        if b.summary().rate_limited > 0 {
            break;
        }
        assert!(Instant::now() < deadline, "never rate limited");
        write_file(&b, ROOT, &format!("f{i}"), b"x");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // Uploads now wait for the window to close; downloads do not.
    let t = Instant::now();
    assert_eq!(read_file(&b, target.ino).await, noise(5000, 1));
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_exhausted_rest_budget_fails_fast() {
    let server = server().await;
    let a = job_with(&server, MAIN, |c| c.refresh = Some(Duration::ZERO)).await;
    let reset = SystemTime::now() + Duration::from_secs(3600);
    server.set_config(|c| c.rest_exhausted_until = Some(reset));

    // Lookups of missing names refresh; they must not wait an hour for it.
    let t = Instant::now();
    for _ in 0..3 {
        assert_eq!(
            lookup(&a, "missing").await.unwrap_err(),
            Errno(libc::ENOENT)
        );
    }
    // Nor does a mount, which fails and says why.
    let env = server.env(MAIN, &[]);
    let dir = tempfile::tempdir().unwrap();
    let store = DataStore::new(&dir.path().join("data"), 1 << 30, 8).unwrap();
    let cfg = VfsConfig::new(Volume::new("default").unwrap());
    let loaded = Vfs::load(cfg, Api::new(&env).unwrap(), store, env.scopes()).await;
    let err = format!("{:#}", loaded.err().expect("the listing is refused"));
    assert!(err.contains("rate limited; try again in"), "{err}");
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
    // Uploads go on.
    write_file(&a, ROOT, "f", b"x");
    drained(&a).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sequential_reads_fetch_whole_runs() {
    let server = server().await;
    let a = job(&server, MAIN).await;
    let data = noise(64 << 20, 5);
    write_file(&a, ROOT, "f", &data);
    drained(&a).await;
    let b = job(&server, MAIN).await;
    let before = server.blob_requests();
    assert_eq!(cat(&b, "f").await, data);
    // 8 MiB ranges, and a few smaller ones while the window grows.
    let gets = server.blob_requests() - before;
    assert!(gets <= 14, "{gets} range requests");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn entries_deleted_during_a_listing_do_not_hide_others() {
    let server = FakeServer::start(FakeConfig {
        latency: Duration::from_millis(500),
        ..FakeConfig::default()
    })
    .await
    .unwrap();
    let volume = Volume::new("default").unwrap();
    let mut keys = Vec::new();
    for i in 0..300 {
        let (image, version) = index::layer_image(&[(format!("f{i:03}"), b"x".to_vec())]).unwrap();
        let key = volume.layer_key(version.nonce);
        server.insert(&key, &version.encode(), MAIN, bytes::Bytes::from(image));
        keys.push(key);
    }
    let env = server.env(MAIN, &[]);
    let api = Api::new(&env).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let store = DataStore::new(&dir.path().join("data"), 1 << 30, 8).unwrap();
    let mut cfg = VfsConfig::new(volume);
    cfg.refresh = None;
    // The first of three pages arrives after 0.5 s, the others after 1 s;
    // in between, the ten oldest layers are evicted. Once the pages are in,
    // the service speeds up, so that reading 290 layers takes no time.
    let evict = async {
        tokio::time::sleep(Duration::from_millis(750)).await;
        for key in &keys[..10] {
            server.remove(key, MAIN);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        server.set_config(|c| c.latency = Duration::ZERO);
    };
    let (vfs, ()) = tokio::join!(Vfs::load(cfg, api, store, env.scopes()), evict);
    let names = ls(&vfs.unwrap(), ROOT);
    assert_eq!(names.len(), 290);
    assert_eq!(names[0], "f010");
    assert!(names.contains(&"f105".to_string()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blobs_nothing_reads_are_kept_alive() {
    let server = server().await;
    let main = job(&server, MAIN).await;
    write_file(&main, ROOT, "big", &noise(10 << 20, 1));
    write_file(&main, ROOT, "small", b"in the layer");
    drained(&main).await;
    // Four days pass, as far as the service can tell.
    server.age(Duration::from_secs(4 * 24 * 3600));
    let stale = SystemTime::now() - Duration::from_secs(3 * 24 * 3600);

    let again = job(&server, FEATURE).await;
    let deadline = Instant::now() + Duration::from_secs(5);
    while again.summary().touched < 1 {
        assert!(Instant::now() < deadline, "{:?}", again.summary());
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // Mounting read every layer, and touched the blob nothing read.
    let all = [entries(&server, "layer"), entries(&server, "blob")].concat();
    assert_eq!(all.len(), 2);
    for (key, _) in &all {
        assert!(server.last_used(key, MAIN).unwrap() > stale, "{key}");
    }
    assert_eq!(again.summary().touched, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fsync_commits_before_close() {
    let server = server().await;
    let a = job_with(&server, MAIN, |c| {
        c.settle = Duration::from_secs(3600);
        c.fsync = FsyncMode::Commit;
    })
    .await;
    let (_, fh) = a.create(ROOT, "log", 0o644, libc::O_WRONLY).unwrap();
    a.write(fh, 0, b"first").unwrap();
    a.fsync(fh).await.unwrap();
    assert_eq!(layers(&server), 1);
    let b = job(&server, MAIN).await;
    assert_eq!(cat(&b, "log").await, b"first");
    a.write(fh, 5, b" second").unwrap();
    a.release(fh).unwrap();
    drained(&a).await;
    let c = job(&server, MAIN).await;
    assert_eq!(cat(&c, "log").await, b"first second");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fsync_of_a_file_opened_while_it_commits() {
    let server = FakeServer::start(FakeConfig {
        latency: Duration::from_millis(100),
        ..FakeConfig::default()
    })
    .await
    .unwrap();
    let a = job_with(&server, MAIN, |c| {
        c.settle = Duration::ZERO;
        c.fsync = FsyncMode::Commit;
    })
    .await;
    let f = write_file(&a, ROOT, "log", b"");
    // Its layer is on its way while the file is opened again.
    tokio::time::sleep(Duration::from_millis(20)).await;
    let (fh, _) = a.open(f, libc::O_WRONLY).await.unwrap();
    a.fsync(fh).await.unwrap();
    assert_eq!(layers(&server), 1);
    a.write(fh, 0, b"synced").unwrap();
    a.fsync(fh).await.unwrap();
    assert_eq!(layers(&server), 2);
    let b = job(&server, MAIN).await;
    assert_eq!(cat(&b, "log").await, b"synced");
    a.release(fh).unwrap();
    drained(&a).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fsync_is_local_by_default() {
    let server = server().await;
    let a = patient_job(&server, MAIN).await;
    let before = a.summary().requests;
    for i in 0..3 {
        let (_, fh) = a
            .create(ROOT, &format!("f{i}"), 0o644, libc::O_WRONLY)
            .unwrap();
        a.write(fh, 0, b"x").unwrap();
        a.fsync(fh).await.unwrap();
        a.release(fh).unwrap();
    }
    assert_eq!(a.summary().requests, before);
    assert_eq!(layers(&server), 0);
    // They all upload at unmount, in one layer.
    let summary = a.drain().await;
    assert_eq!((summary.uploaded_files, summary.layers), (3, 1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lookup_misses_pick_up_other_jobs_writes() {
    let server = server().await;
    let reader = job_with(&server, MAIN, |c| c.refresh = Some(Duration::ZERO)).await;
    assert_eq!(
        lookup(&reader, "later/done").await.unwrap_err(),
        Errno(libc::ENOENT)
    );
    let writer = job(&server, MAIN).await;
    let d = writer.mkdir(ROOT, "later", 0o755).unwrap();
    write_file(&writer, d.ino, "done", b"yes");
    drained(&writer).await;
    assert_eq!(cat(&reader, "later/done").await, b"yes");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unlinked_files_stay_readable_while_open() {
    let server = server().await;
    let a = job(&server, MAIN).await;
    write_file(&a, ROOT, "f", b"still here");
    drained(&a).await;
    let b = job(&server, MAIN).await;
    let f = lookup(&b, "f").await.unwrap();
    let (fh, _) = b.open(f.ino, libc::O_RDONLY).await.unwrap();
    b.unlink(ROOT, "f").unwrap();
    assert_eq!(b.read(fh, 0, 100).await.unwrap(), b"still here");
    b.release(fh).unwrap();
    drained(&b).await;
    let c = job(&server, MAIN).await;
    assert!(ls(&c, ROOT).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn readdir_snapshots_survive_removal() {
    let server = server().await;
    let a = job(&server, MAIN).await;
    for i in 0..50 {
        write_file(&a, ROOT, &format!("f{i:02}"), b"x");
    }
    let fh = a.opendir(ROOT).unwrap();
    let first = a.readdir(ROOT, fh, 0).unwrap();
    assert_eq!(first.len(), 52);
    // Remove entries while "iterating", as rm -r does.
    for e in &first[2..20] {
        a.unlink(ROOT, &e.name).unwrap();
    }
    let rest = a.readdir(ROOT, fh, 20).unwrap();
    assert_eq!(rest.len(), 32);
    assert_eq!(rest[0].name, "f18");
    a.releasedir(fh);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn read_only_mounts_reject_writes() {
    let server = server().await;
    let a = job_with(&server, MAIN, |c| c.read_only = true).await;
    assert_eq!(
        a.create(ROOT, "f", 0o644, libc::O_WRONLY).unwrap_err(),
        Errno(libc::EROFS)
    );
    assert_eq!(a.mkdir(ROOT, "d", 0o755).unwrap_err(), Errno(libc::EROFS));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn names_round_trip() {
    let server = server().await;
    let a = job(&server, MAIN).await;
    let names = [
        "with space",
        "unicodé 日本 🎉",
        "comma,here",
        "percent%41",
        "hash#q?x=1&y",
        "UPPER",
        "upper",
    ];
    for n in names {
        write_file(&a, ROOT, n, n.as_bytes());
    }
    drained(&a).await;
    let b = job(&server, MAIN).await;
    let mut want: Vec<String> = names.iter().map(|s| s.to_string()).collect();
    want.sort();
    assert_eq!(ls(&b, ROOT), want);
    for n in names {
        assert_eq!(cat(&b, n).await, n.as_bytes());
    }
    let long = "x".repeat(510);
    assert_eq!(
        a.create(ROOT, &long, 0o644, libc::O_WRONLY).unwrap_err(),
        Errno(libc::ENAMETOOLONG)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_small_files() {
    let server = server().await;
    let a = job(&server, MAIN).await;
    let d = a.mkdir(ROOT, "many", 0o755).unwrap();
    for i in 0..300u64 {
        write_file(&a, d.ino, &format!("{i}"), &noise(1000 + i as usize, i));
    }
    let summary = a.drain().await;
    assert!(summary.failures.is_empty());
    assert_eq!(summary.uploaded_files, 300);
    // A few layers, each a creation and a finalization.
    assert!(summary.layers < 10, "{summary:?}");
    assert_eq!(summary.requests.cache_service, 2 * summary.layers);
    let b = job(&server, MAIN).await;
    assert_eq!(ls_path(&b, "many").await.len(), 300);
    assert_eq!(cat(&b, "many/123").await, noise(1123, 123));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn entries_from_other_tools_are_ignored() {
    let server = server().await;
    // What actions/cache would store: a sha256 version over paths.
    let env = server.env(MAIN, &[]);
    let api = Api::new(&env).unwrap();
    let version = "a".repeat(64);
    let key = Volume::new("default").unwrap().layer_key(1);
    let url = api.twirp.create(&key, &version).await.unwrap();
    api.blob
        .put_blob(&url, bytes::Bytes::from_static(b"tar"))
        .await
        .unwrap();
    api.twirp.finalize(&key, &version, 3).await.unwrap();
    let a = job(&server, MAIN).await;
    assert!(ls(&a, ROOT).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_whole_tree() {
    // Many operations interleaved, then compared against an independent view.
    let server = server().await;
    let a = job(&server, MAIN).await;
    let src = a.mkdir(ROOT, "src", 0o755).unwrap();
    let bin = a.mkdir(ROOT, "bin", 0o755).unwrap();
    for i in 0..20u64 {
        write_file(&a, src.ino, &format!("m{i}.rs"), &noise(5000, i));
    }
    write_file(&a, bin.ino, "tool", &noise(2 << 20, 42));
    a.symlink(ROOT, "latest", "bin/tool").unwrap();
    a.unlink(src.ino, "m3.rs").unwrap();
    a.rename(src.ino, "m4.rs", ROOT, "moved.rs", RenameMode::Replace)
        .await
        .unwrap();
    drained(&a).await;

    let b = job(&server, MAIN).await;
    assert_eq!(ls(&b, ROOT), ["bin", "latest", "moved.rs", "src"]);
    assert_eq!(ls_path(&b, "src").await.len(), 18);
    assert_eq!(cat(&b, "moved.rs").await, noise(5000, 4));
    assert_eq!(cat(&b, "bin/tool").await, noise(2 << 20, 42));
    let l = lookup(&b, "latest").await.unwrap();
    assert_eq!(b.readlink(l.ino).await.unwrap(), "bin/tool");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reading_one_small_file_prefetches_its_siblings() {
    let server = server().await;
    let a = job(&server, MAIN).await;
    let d = a.mkdir(ROOT, "src", 0o755).unwrap();
    for i in 0..20u64 {
        write_file(&a, d.ino, &format!("f{i:02}"), &noise(3000, i));
    }
    write_file(&a, d.ino, "big", &noise(3 << 20, 99));
    drained(&a).await;

    let b = job(&server, MAIN).await;
    let before = server.blob_requests();
    assert_eq!(cat(&b, "src/f00").await, noise(3000, 0));
    // The small files are one range of the layer, fetched at once.
    let mut last = server.blob_requests();
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let now = server.blob_requests();
        if now == last {
            break;
        }
        last = now;
    }
    for i in 1..20u64 {
        assert_eq!(cat(&b, &format!("src/f{i:02}")).await, noise(3000, i));
    }
    let gets = server.blob_requests() - before;
    assert!(gets <= 3, "{gets} requests for 20 small files");
    // Large files are left alone until they are read.
    let before = server.blob_requests();
    assert_eq!(cat(&b, "src/big").await, noise(3 << 20, 99));
    assert!(server.blob_requests() > before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_thousand_files_are_one_creation() {
    let server = server().await;
    let a = patient_job(&server, MAIN).await;
    let top = a.mkdir(ROOT, "tree", 0o755).unwrap();
    let mut dirs = Vec::new();
    for d in 0..10 {
        dirs.push(a.mkdir(top.ino, &format!("d{d}"), 0o755).unwrap().ino);
    }
    for i in 0..1000u64 {
        write_file(
            &a,
            dirs[i as usize % 10],
            &format!("f{i:04}"),
            &noise(4096, i),
        );
    }
    let summary = a.drain().await;
    assert!(summary.failures.is_empty(), "{:?}", summary.failures);
    assert_eq!((summary.layers, summary.blobs), (1, 0));
    assert_eq!(summary.requests.cache_service, 2);
    assert_eq!((summary.uploaded_files, summary.dir_markers), (1000, 11));

    // Removing them all is one creation too.
    let b = patient_job(&server, MAIN).await;
    assert_eq!(cat(&b, "tree/d7/f0587").await, noise(4096, 587));
    for d in 0..10 {
        let dir = lookup(&b, &format!("tree/d{d}")).await.unwrap().ino;
        for name in ls(&b, dir) {
            b.unlink(dir, &name).unwrap();
        }
        b.rmdir(top_ino(&b).await, &format!("d{d}")).unwrap();
    }
    b.rmdir(ROOT, "tree").unwrap();
    let summary = b.drain().await;
    assert_eq!(
        (summary.layers, summary.whiteouts, summary.dir_markers),
        (1, 1000, 11)
    );
    let c = job(&server, MAIN).await;
    assert!(ls(&c, ROOT).is_empty());
}

async fn top_ino(vfs: &Vfs) -> Ino {
    lookup(vfs, "tree").await.unwrap().ino
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deletion_hides_only_what_the_deleting_job_saw() {
    let server = server().await;
    let a = job(&server, MAIN).await;
    let d = a.mkdir(ROOT, "d", 0o755).unwrap();
    write_file(&a, d.ino, "old", b"seen by both");
    drained(&a).await;

    // Two jobs start from the same cache. One adds a file to d; the other,
    // which never sees it, removes d with everything it knows of.
    let adder = job(&server, MAIN).await;
    let remover = job(&server, MAIN).await;
    let d = lookup(&adder, "d").await.unwrap();
    write_file(&adder, d.ino, "new", b"added concurrently");
    drained(&adder).await;
    let d = lookup(&remover, "d").await.unwrap();
    remover.unlink(d.ino, "old").unwrap();
    remover.rmdir(ROOT, "d").unwrap();
    drained(&remover).await;

    let c = job(&server, MAIN).await;
    assert_eq!(ls_path(&c, "d").await, ["new"]);
    assert_eq!(cat(&c, "d/new").await, b"added concurrently");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn directory_attributes_persist() {
    let server = server().await;
    let a = job(&server, MAIN).await;
    let d = a.mkdir(ROOT, "d", 0o755).unwrap();
    write_file(&a, d.ino, "f", b"x");
    drained(&a).await;

    let b = job(&server, MAIN).await;
    let d = lookup(&b, "d").await.unwrap();
    let mtime = UNIX_EPOCH + Duration::new(1_600_000_000, 5);
    b.setattr(
        d.ino,
        None,
        SetAttr {
            mode: Some(0o700),
            mtime: Some(SetTime::At(mtime)),
            ..SetAttr::default()
        },
    )
    .await
    .unwrap();
    drained(&b).await;

    let c = job(&server, MAIN).await;
    let d = lookup(&c, "d").await.unwrap();
    assert_eq!((d.perm, d.mtime), (0o700, mtime));
    // A later layer that only passes through keeps them.
    write_file(&c, d.ino, "g", b"y");
    drained(&c).await;
    let e = job(&server, MAIN).await;
    assert_eq!(lookup(&e, "d").await.unwrap().perm, 0o700);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn volumes_are_separate() {
    let server = server().await;
    let in_volume =
        |name: &'static str| move |c: &mut VfsConfig| c.volume = Volume::new(name).unwrap();
    let a = job_with(&server, MAIN, in_volume("one")).await;
    write_file(&a, ROOT, "f", b"in one");
    drained(&a).await;
    let b = job_with(&server, MAIN, in_volume("two")).await;
    assert!(ls(&b, ROOT).is_empty());
    write_file(&b, ROOT, "f", b"in two");
    drained(&b).await;
    let c = job_with(&server, MAIN, in_volume("one")).await;
    assert_eq!(cat(&c, "f").await, b"in one");
    assert!(
        server
            .entries()
            .iter()
            .all(|(k, _, _)| k.starts_with("gha-fs/one/") || k.starts_with("gha-fs/two/"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mounting_a_directory_of_the_volume() {
    let server = server().await;
    let a = job(&server, MAIN).await;
    let sub = a.mkdir(ROOT, "sub", 0o755).unwrap();
    let dir = a.mkdir(sub.ino, "dir", 0o755).unwrap();
    write_file(&a, dir.ino, "f", b"deep");
    write_file(&a, ROOT, "outside", b"not shown");
    drained(&a).await;

    let at_sub = |c: &mut VfsConfig| *c = c.clone().with_root("sub").unwrap();
    let b = job_with(&server, MAIN, at_sub).await;
    assert_eq!(ls(&b, ROOT), ["dir"]);
    assert_eq!(cat(&b, "dir/f").await, b"deep");
    write_file(&b, ROOT, "g", b"written below sub");
    b.unlink(lookup(&b, "dir").await.unwrap().ino, "f").unwrap();
    drained(&b).await;

    let c = job(&server, MAIN).await;
    assert_eq!(ls(&c, ROOT), ["outside", "sub"]);
    assert_eq!(ls_path(&c, "sub").await, ["dir", "g"]);
    assert_eq!(cat(&c, "sub/g").await, b"written below sub");
    assert!(ls_path(&c, "sub/dir").await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn identical_large_files_share_a_blob() {
    let server = server().await;
    let a = patient_job(&server, MAIN).await;
    let data = noise(10 << 20, 3);
    write_file(&a, ROOT, "one", &data);
    write_file(&a, ROOT, "two", &data);
    let summary = a.drain().await;
    assert_eq!((summary.layers, summary.blobs), (1, 1));
    // A later job writing the same content uploads nothing but a layer.
    let b = job(&server, FEATURE).await;
    write_file(&b, ROOT, "three", &data);
    let summary = b.drain().await;
    assert_eq!((summary.layers, summary.blobs), (1, 0));
    let c = job(&server, FEATURE).await;
    for name in ["one", "two", "three"] {
        assert_eq!(cat(&c, name).await, data, "{name}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_whose_blob_is_gone_is_gone() {
    let server = server().await;
    let a = job(&server, MAIN).await;
    write_file(&a, ROOT, "big", &noise(10 << 20, 4));
    write_file(&a, ROOT, "small", b"still here");
    drained(&a).await;
    let (blob, _) = entries(&server, "blob").remove(0);
    server.remove(&blob, MAIN);
    let b = job(&server, MAIN).await;
    assert_eq!(ls(&b, ROOT), ["small"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn changes_made_while_sealing_wait_for_the_next_layer() {
    let server = server().await;
    let a = job_with(&server, MAIN, |c| c.fsync = FsyncMode::Commit).await;
    // A file that keeps changing is committed as it was, and then as it is.
    let (_, fh) = a.create(ROOT, "log", 0o644, libc::O_WRONLY).unwrap();
    for i in 0..50u64 {
        a.write(fh, i * 4096, &noise(4096, i)).unwrap();
        if i % 10 == 0 {
            let _ = a.fsync(fh).await;
        }
    }
    a.release(fh).unwrap();
    drained(&a).await;
    let b = job(&server, MAIN).await;
    let want: Vec<u8> = (0..50u64).flat_map(|i| noise(4096, i)).collect();
    assert_eq!(cat(&b, "log").await, want);
}

fn tool(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

/// Every layer the filesystem writes is an EROFS image that erofs-utils
/// accepts, blobs and all. Skipped without `fsck.erofs` on PATH.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn layers_pass_fsck() {
    let Some(fsck) = tool("fsck.erofs") else {
        eprintln!("skipping: fsck.erofs is not on PATH");
        return;
    };
    let server = server().await;
    let a = patient_job(&server, MAIN).await;
    let d = a.mkdir(ROOT, "d", 0o700).unwrap();
    write_file(&a, d.ino, "inline", b"small");
    write_file(&a, d.ino, "blocks", &noise(100_000, 1));
    write_file(&a, d.ino, "blob", &noise(9 << 20, 2));
    write_file(&a, ROOT, "doomed", b"x");
    a.symlink(d.ino, "link", "inline").unwrap();
    drained(&a).await;
    let b = patient_job(&server, MAIN).await;
    b.unlink(ROOT, "doomed").unwrap();
    let d = lookup(&b, "d").await.unwrap();
    b.setattr(
        d.ino,
        None,
        SetAttr {
            mode: Some(0o755),
            ..SetAttr::default()
        },
    )
    .await
    .unwrap();
    // Moved files refer to the first layer, and to the blob.
    for name in ["blocks", "blob"] {
        b.rename(
            d.ino,
            name,
            d.ino,
            &format!("moved-{name}"),
            RenameMode::Replace,
        )
        .await
        .unwrap();
    }
    drained(&b).await;

    // Each device is a file of its own, named by its tag.
    let dir = tempfile::tempdir().unwrap();
    let device = |tag: &str| dir.path().join(tag.replace('/', "-"));
    let (blob, _) = entries(&server, "blob").remove(0);
    let sha = blob.rsplit('/').next().unwrap();
    std::fs::write(device(sha), server.data(&blob, MAIN).unwrap()).unwrap();
    let layers = entries(&server, "layer");
    assert_eq!(layers.len(), 2);
    for (key, _) in &layers {
        let tag = format!("layer/{}", key.rsplit('/').next().unwrap());
        std::fs::write(device(&tag), server.data(key, MAIN).unwrap()).unwrap();
    }
    let mut layer_refs = 0;
    for (key, _) in &layers {
        let image = server.data(key, MAIN).unwrap();
        let listing = erofs::read(&image).unwrap();
        let mut cmd = Command::new(&fsck);
        for (tag, _) in &listing.devices {
            assert!(tag == sha || tag.starts_with("layer/"), "{tag}");
            layer_refs += usize::from(tag.starts_with("layer/"));
            cmd.arg(format!("--device={}", device(tag).display()));
        }
        let path = device(&format!("layer/{}", key.rsplit('/').next().unwrap()));
        let out = cmd.arg(&path).output().unwrap();
        assert!(
            out.status.success(),
            "fsck.erofs rejected {key}:\n{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert_eq!(layer_refs, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn renaming_a_directory_takes_everything_pending_in_it() {
    let server = server().await;
    let a = patient_job(&server, MAIN).await;
    let d = a.mkdir(ROOT, "pkg", 0o755).unwrap();
    write_file(&a, d.ino, "a", b"inside");
    // Sorts between "pkg" and "pkg/".
    write_file(&a, ROOT, "pkg.tar", b"beside");
    a.rename(ROOT, "pkg", ROOT, "moved", RenameMode::Replace)
        .await
        .unwrap();
    drained(&a).await;
    let b = job(&server, MAIN).await;
    assert_eq!(ls(&b, ROOT), ["moved", "pkg.tar"]);
    assert_eq!(cat(&b, "moved/a").await, b"inside");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_renamed_twice_keeps_its_old_name_until_the_new_one_lands() {
    let server = server().await;
    let a = job(&server, MAIN).await;
    write_file(&a, ROOT, "x", &noise(5000, 1));
    drained(&a).await;

    let b = job(&server, MAIN).await;
    let x = lookup(&b, "x").await.unwrap();
    let (fh, _) = b.open(x.ino, libc::O_RDWR).await.unwrap();
    b.write(fh, 0, b"changed").unwrap();
    b.rename(ROOT, "x", ROOT, "y", RenameMode::Replace)
        .await
        .unwrap();
    b.rename(ROOT, "y", ROOT, "z", RenameMode::Replace)
        .await
        .unwrap();
    // z is still open, so it is not committed; neither may x's removal be.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let c = job(&server, MAIN).await;
    assert_eq!(ls(&c, ROOT), ["x"]);
    b.release(fh).unwrap();
    drained(&b).await;
    let d = job(&server, MAIN).await;
    assert_eq!(ls(&d, ROOT), ["z"]);
    assert_eq!(&cat(&d, "z").await[..7], b"changed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_chmod_while_a_mark_uploads_is_not_lost() {
    let server = server().await;
    let a = job(&server, MAIN).await;
    let d = a.mkdir(ROOT, "d", 0o755).unwrap();
    write_file(&a, d.ino, "f", b"x");
    drained(&a).await;

    server.set_config(|c| c.latency = Duration::from_millis(300));
    let b = job(&server, MAIN).await;
    let d = lookup(&b, "d").await.unwrap();
    let chmod = |mode| SetAttr {
        mode: Some(mode),
        ..SetAttr::default()
    };
    b.setattr(d.ino, None, chmod(0o700)).await.unwrap();
    // The first mark is on its way up when the second chmod lands.
    tokio::time::sleep(Duration::from_millis(150)).await;
    b.setattr(d.ino, None, chmod(0o711)).await.unwrap();
    drained(&b).await;
    assert_eq!(b.getattr(d.ino).unwrap().perm, 0o711);
    server.set_config(|c| c.latency = Duration::ZERO);
    let c = job(&server, MAIN).await;
    assert_eq!(lookup(&c, "d").await.unwrap().perm, 0o711);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn removing_a_replacement_keeps_the_original_removed() {
    let server = server().await;
    let a = job(&server, MAIN).await;
    write_file(&a, ROOT, "was-file", b"x");
    a.mkdir(ROOT, "was-dir", 0o755).unwrap();
    drained(&a).await;

    let b = job(&server, MAIN).await;
    lookup(&b, "was-file").await.unwrap();
    b.unlink(ROOT, "was-file").unwrap();
    b.mkdir(ROOT, "was-file", 0o755).unwrap();
    b.rmdir(ROOT, "was-file").unwrap();
    lookup(&b, "was-dir").await.unwrap();
    b.rmdir(ROOT, "was-dir").unwrap();
    write_file(&b, ROOT, "was-dir", b"y");
    b.unlink(ROOT, "was-dir").unwrap();
    drained(&b).await;
    let c = job(&server, MAIN).await;
    assert!(ls(&c, ROOT).is_empty(), "{:?}", ls(&c, ROOT));
}

/// A job that writes a snapshot at unmount once it would cover `n` layers,
/// however new they are.
async fn snapshotting_job(server: &FakeServer, git_ref: &str, n: usize) -> Job {
    job_with(server, git_ref, |c| {
        c.snapshot_after = n;
        c.snapshot_margin = Duration::ZERO;
    })
    .await
}

/// The snapshots in a scope, as their keys and what they cover.
fn snapshots(server: &FakeServer, scope: &str) -> Vec<(String, SystemTime)> {
    let mut out: Vec<(String, SystemTime)> = entries(server, "layer")
        .into_iter()
        .filter_map(|(key, _)| {
            let version = server.version(&key, scope)?;
            match Version::decode(&version)?.kind {
                Kind::Snapshot { covers, .. } => Some((key, covers)),
                _ => None,
            }
        })
        .collect();
    out.sort_by_key(|(_, covers)| *covers);
    out
}

fn digest(data: &[u8]) -> u64 {
    data.iter()
        .fold(0u64, |h, b| h.wrapping_mul(31).wrapping_add(*b as u64))
}

/// Everything below the root, one line per path: its kind, permissions,
/// and a digest of its content.
async fn dump(vfs: &Vfs) -> Vec<String> {
    let mut out = Vec::new();
    let mut dirs = vec![String::new()];
    while let Some(dir) = dirs.pop() {
        for name in ls_path(vfs, &dir).await {
            let path = if dir.is_empty() {
                name
            } else {
                format!("{dir}/{name}")
            };
            let attr = lookup(vfs, &path).await.unwrap();
            let line = match attr.kind {
                FileKind::Dir => {
                    dirs.push(path.clone());
                    format!("{path}/ {:o}", attr.perm)
                }
                FileKind::Symlink => {
                    format!("{path} -> {}", vfs.readlink(attr.ino).await.unwrap())
                }
                FileKind::File => {
                    let data = read_file(vfs, attr.ino).await;
                    format!("{path} {:o} {} {:x}", attr.perm, data.len(), digest(&data))
                }
            };
            out.push(line);
        }
    }
    out.sort();
    out
}

/// Six layers of every kind of change, on MAIN.
async fn six_layers(server: &FakeServer) {
    let a = job(server, MAIN).await;
    let d = a.mkdir(ROOT, "d", 0o750).unwrap();
    write_file(&a, d.ino, "small", &noise(5000, 1));
    write_file(&a, d.ino, "tiny", b"inline");
    write_file(&a, d.ino, "big", &noise(9 << 20, 2));
    a.symlink(d.ino, "link", "small").unwrap();
    a.mkdir(ROOT, "empty", 0o700).unwrap();
    write_file(&a, ROOT, "doomed", b"x");
    drained(&a).await;
    let b = job(server, MAIN).await;
    b.unlink(ROOT, "doomed").unwrap();
    b.rmdir(ROOT, "empty").unwrap();
    let d = lookup(&b, "d").await.unwrap();
    b.rename(d.ino, "tiny", d.ino, "tiny2", RenameMode::Replace)
        .await
        .unwrap();
    b.setattr(
        d.ino,
        None,
        SetAttr {
            mode: Some(0o755),
            ..SetAttr::default()
        },
    )
    .await
    .unwrap();
    drained(&b).await;
    for i in 0..4 {
        let j = job(server, MAIN).await;
        write_file(&j, ROOT, &format!("f{i}"), &noise(2000, 10 + i));
        if i == 1 {
            j.mkdir(ROOT, "kept", 0o711).unwrap();
        }
        drained(&j).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn snapshots_stand_in_for_the_layers_they_cover() {
    let server = server().await;
    six_layers(&server).await;
    let before = dump(&job(&server, MAIN).await.vfs).await;

    let s = snapshotting_job(&server, MAIN, 4).await;
    let summary = s.drain().await;
    assert_eq!((summary.layers, summary.snapshots), (0, 1), "{summary:?}");
    assert_eq!(snapshots(&server, MAIN).len(), 1);

    // A mount now reads the snapshot's metadata, and nothing else.
    let fresh = job(&server, MAIN).await;
    assert_eq!(fresh.summary().requests.blob, 1, "{:?}", fresh.summary());
    assert_eq!(dump(&fresh).await, before);

    // Later layers stack above it, and the next snapshot covers them too.
    let j = job(&server, MAIN).await;
    write_file(&j, ROOT, "after", b"new");
    j.unlink(ROOT, "f0").unwrap();
    drained(&j).await;
    let mut want: Vec<String> = before
        .iter()
        .filter(|l| !l.starts_with("f0 "))
        .cloned()
        .collect();
    want.push(format!("after 644 3 {:x}", digest(b"new")));
    want.sort();
    assert_eq!(dump(&job(&server, MAIN).await.vfs).await, want);
    let s = snapshotting_job(&server, MAIN, 1).await;
    assert_eq!(s.drain().await.snapshots, 1);
    let snaps = snapshots(&server, MAIN);
    assert_eq!(snaps.len(), 2);
    assert!(snaps[0].1 < snaps[1].1);
    let fresh = job(&server, MAIN).await;
    assert_eq!(fresh.summary().requests.blob, 1, "only the newest snapshot");
    assert_eq!(dump(&fresh).await, want);

    // The covered layers' metadata is not needed any more, only the data
    // they hold: without them, only files whose data is elsewhere remain.
    for (key, _) in entries(&server, "layer") {
        if !snaps.iter().any(|(k, _)| *k == key) {
            server.remove(&key, MAIN);
        }
    }
    let bare = job(&server, MAIN).await;
    assert_eq!(ls(&bare, ROOT), ["after", "d", "kept"]);
    assert_eq!(ls_path(&bare, "d").await, ["big", "link", "tiny2"]);
    assert_eq!(cat(&bare, "d/tiny2").await, b"inline");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_branch_snapshot_keeps_what_hides_the_default_branch() {
    let server = server().await;
    let main = job(&server, MAIN).await;
    write_file(&main, ROOT, "a", b"A");
    write_file(&main, ROOT, "b", b"B");
    let d = main.mkdir(ROOT, "d", 0o755).unwrap();
    write_file(&main, d.ino, "f", b"F");
    drained(&main).await;

    let f = job(&server, FEATURE).await;
    f.unlink(ROOT, "a").unwrap();
    let d = lookup(&f, "d").await.unwrap();
    f.setattr(
        d.ino,
        None,
        SetAttr {
            mode: Some(0o700),
            ..SetAttr::default()
        },
    )
    .await
    .unwrap();
    drained(&f).await;
    // Given attributes, and then removed: attrs-drop in the snapshot.
    let f = job(&server, FEATURE).await;
    let d = lookup(&f, "d").await.unwrap();
    f.unlink(d.ino, "f").unwrap();
    f.rmdir(ROOT, "d").unwrap();
    drained(&f).await;
    for i in 0..2 {
        let f = job(&server, FEATURE).await;
        write_file(&f, ROOT, &format!("x{i}"), b"x");
        drained(&f).await;
    }
    let s = snapshotting_job(&server, FEATURE, 4).await;
    assert_eq!(s.drain().await.snapshots, 1);

    let f = job(&server, FEATURE).await;
    assert_eq!(
        f.summary().requests.blob,
        2,
        "main's layer and the snapshot"
    );
    assert_eq!(ls(&f, ROOT), ["b", "x0", "x1"]);
    let m = job(&server, MAIN).await;
    assert_eq!(ls(&m, ROOT), ["a", "b", "d"]);
    // The drop still says d exists only while something is in it.
    let d = lookup(&m, "d").await.unwrap();
    write_file(&m, d.ino, "g", b"G");
    drained(&m).await;
    let f = job(&server, FEATURE).await;
    assert_eq!(ls(&f, ROOT), ["b", "d", "x0", "x1"]);
    assert_eq!(ls_path(&f, "d").await, ["g"]);
    assert_eq!(lookup(&f, "d").await.unwrap().perm, 0o700);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn snapshots_pass_fsck() {
    let Some(fsck) = tool("fsck.erofs") else {
        eprintln!("skipping: fsck.erofs is not on PATH");
        return;
    };
    let server = server().await;
    six_layers(&server).await;
    let s = snapshotting_job(&server, MAIN, 4).await;
    assert_eq!(s.drain().await.snapshots, 1);
    // Each device is a file of its own, named by its tag.
    let dir = tempfile::tempdir().unwrap();
    let device = |tag: &str| dir.path().join(tag.replace('/', "-"));
    let volume = Volume::new("default").unwrap();
    for (key, _) in [entries(&server, "layer"), entries(&server, "blob")].concat() {
        let tag = volume.device_tag(&key).unwrap();
        std::fs::write(device(&tag), server.data(&key, MAIN).unwrap()).unwrap();
    }
    let (key, _) = snapshots(&server, MAIN).remove(0);
    let image = server.data(&key, MAIN).unwrap();
    let listing = erofs::read(&image).unwrap();
    assert!(listing.devices.len() >= 2, "a blob and layers");
    let mut cmd = Command::new(&fsck);
    for (tag, _) in &listing.devices {
        cmd.arg(format!("--device={}", device(tag).display()));
    }
    let out = cmd
        .arg(device(&volume.device_tag(&key).unwrap()))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "fsck.erofs rejected the snapshot:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_default_branch_snapshot_leaves_out_what_hides_nothing() {
    let server = server().await;
    let a = job(&server, MAIN).await;
    write_file(&a, ROOT, "x", b"x");
    a.mkdir(ROOT, "z", 0o755).unwrap();
    drained(&a).await;
    let b = job(&server, MAIN).await;
    b.unlink(ROOT, "x").unwrap();
    b.rmdir(ROOT, "z").unwrap();
    drained(&b).await;
    let c = job(&server, MAIN).await;
    write_file(&c, ROOT, "y", b"y");
    drained(&c).await;
    let s = snapshotting_job(&server, MAIN, 3).await;
    assert_eq!(s.drain().await.snapshots, 1);
    let (key, _) = snapshots(&server, MAIN).remove(0);
    let listing = erofs::read(&server.data(&key, MAIN).unwrap()).unwrap();
    let paths: Vec<&str> = listing.entries.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(paths, ["", "y"]);
    assert!(listing.devices.is_empty(), "y is inline");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_snapshot_stops_before_a_layer_it_cannot_read() {
    let server = server().await;
    for i in 0..2 {
        let j = job(&server, MAIN).await;
        write_file(&j, ROOT, &format!("f{i}"), b"x");
        drained(&j).await;
    }
    // A layer from some later version of the tool.
    let volume = Volume::new("default").unwrap();
    let version = Version::layer(1);
    server.insert(
        &volume.layer_key(version.nonce),
        &version.encode(),
        MAIN,
        bytes::Bytes::from(vec![0xa5u8; 4096]),
    );
    tokio::time::sleep(Duration::from_millis(5)).await;
    for i in 2..4 {
        let j = job(&server, MAIN).await;
        write_file(&j, ROOT, &format!("f{i}"), b"x");
        drained(&j).await;
    }
    let s = snapshotting_job(&server, MAIN, 2).await;
    assert_eq!(s.drain().await.snapshots, 1);
    let (key, _) = snapshots(&server, MAIN).remove(0);
    let listing = erofs::read(&server.data(&key, MAIN).unwrap()).unwrap();
    let paths: Vec<&str> = listing.entries.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(paths, ["", "f0", "f1"]);
    let fresh = job(&server, MAIN).await;
    assert_eq!(ls(&fresh, ROOT), ["f0", "f1", "f2", "f3"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn covered_layers_live_while_visible_files_need_them() {
    let server = server().await;
    let a = job(&server, MAIN).await;
    write_file(&a, ROOT, "kept", &noise(5000, 1));
    drained(&a).await;
    let b = job(&server, MAIN).await;
    write_file(&b, ROOT, "replaced", &noise(5000, 2));
    drained(&b).await;
    let c = job(&server, MAIN).await;
    c.unlink(ROOT, "replaced").unwrap();
    drained(&c).await;
    let layer_keys: Vec<String> = entries(&server, "layer")
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    let s = snapshotting_job(&server, MAIN, 3).await;
    assert_eq!(s.drain().await.snapshots, 1);

    server.age(Duration::from_secs(4 * 24 * 3600));
    let stale = SystemTime::now() - Duration::from_secs(3 * 24 * 3600);
    let again = job(&server, MAIN).await;
    let deadline = Instant::now() + Duration::from_secs(5);
    while again.summary().touched < 1 {
        assert!(Instant::now() < deadline, "{:?}", again.summary());
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(again.summary().touched, 1);
    // The layer that holds "kept" was touched; the others were not.
    for key in &layer_keys {
        let used = server.last_used(key, MAIN).unwrap() > stale;
        let holds_kept = erofs::read(&server.data(key, MAIN).unwrap())
            .unwrap()
            .entries
            .iter()
            .any(|e| e.path == "kept");
        assert_eq!(used, holds_kept, "{key}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_empty_write_past_the_end_changes_nothing() {
    let server = server().await;
    let a = patient_job(&server, MAIN).await;
    let (attr, fh) = a.create(ROOT, "f", 0o644, libc::O_WRONLY).unwrap();
    a.write(fh, 0, b"abc").unwrap();
    assert_eq!(a.write(fh, 100, b"").unwrap(), 0);
    a.release(fh).unwrap();
    assert_eq!(a.getattr(attr.ino).unwrap().size, 3);
    drained(&a).await;
    assert_eq!(cat(&job(&server, MAIN).await.vfs, "f").await, b"abc");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_renamed_directory_stays_gone_while_a_file_in_it_is_open() {
    let server = server().await;
    let a = job(&server, MAIN).await;
    let b = a.mkdir(ROOT, "b", 0o755).unwrap();
    write_file(&a, b.ino, "c", b"open");
    drained(&a).await;

    let j = job(&server, MAIN).await;
    let c = lookup(&j, "b/c").await.unwrap();
    let (fh, _) = j.open(c.ino, libc::O_RDWR).await.unwrap();
    j.rename(ROOT, "b", ROOT, "moved", RenameMode::Replace)
        .await
        .unwrap();
    // Everything commits but the open file's new name, and the whiteout of
    // its old name, which waits for it.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(ls(&j, ROOT), ["moved"]);
    j.release(fh).unwrap();
    drained(&j).await;
    assert_eq!(ls(&job(&server, MAIN).await.vfs, ROOT), ["moved"]);
}
