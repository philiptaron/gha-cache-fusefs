//! The filesystem core against the fake cache service: every test plays
//! several "jobs" that mount the same cache one after another.

use std::time::{Duration, UNIX_EPOCH};

use gha_cache_fusefs::api::Api;
use gha_cache_fusefs::data::DataStore;
use gha_cache_fusefs::entry::KeySpace;
use gha_cache_fusefs::fake::{FakeConfig, FakeServer};
use gha_cache_fusefs::vfs::{
    Attr, Errno, FileKind, Ino, ROOT, RenameMode, SetAttr, SetTime, Vfs, VfsConfig,
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
    let mut cfg = VfsConfig::new(KeySpace::new("fusefs/"));
    cfg.settle = Duration::from_millis(20);
    cfg.refresh = None;
    tweak(&mut cfg);
    let vfs = Vfs::load(cfg, api, store, env.scopes()).await.unwrap();
    Job { vfs, _dir: dir }
}

async fn job(server: &FakeServer, git_ref: &str) -> Job {
    job_with(server, git_ref, |_| {}).await
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

fn keys(server: &FakeServer) -> Vec<String> {
    let mut k: Vec<String> = server.entries().into_iter().map(|(k, _, _)| k).collect();
    k.sort();
    k
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn files_persist_across_jobs() {
    let server = server().await;
    let a = job(&server, MAIN).await;
    write_file(&a, ROOT, "hello.txt", b"hello, world\n");
    let d = a.mkdir(ROOT, "d", 0o755).unwrap();
    let big = noise(3 << 20, 1);
    write_file(&a, d.ino, "x.bin", &big);
    // Visible and readable in the writing job before anything is uploaded.
    assert_eq!(cat(&a, "hello.txt").await, b"hello, world\n");
    drained(&a).await;
    assert_eq!(keys(&server), ["fusefs/d/x.bin", "fusefs/hello.txt"]);

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
    drained(&a).await;
    assert_eq!(keys(&server), ["fusefs/out"]);
    let b = job(&server, MAIN).await;
    assert_eq!(cat(&b, "out").await, b"payload");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn renaming_committed_and_remote_files() {
    let server = server().await;
    let a = job(&server, MAIN).await;
    write_file(&a, ROOT, "f", b"content");
    drained(&a).await;

    // Still cached locally: the rename re-uploads under the new name.
    let b = job(&server, MAIN).await;
    assert_eq!(cat(&b, "f").await, b"content");
    b.rename(ROOT, "f", ROOT, "g", RenameMode::Replace)
        .await
        .unwrap();
    drained(&b).await;
    let c = job(&server, MAIN).await;
    assert_eq!(ls(&c, ROOT), ["g"]);
    assert_eq!(cat(&c, "g").await, b"content");

    // Not cached: moving it would mean downloading it, so the kernel is told
    // to copy instead (mv does this transparently).
    let d = job(&server, MAIN).await;
    lookup(&d, "g").await.unwrap();
    assert_eq!(
        d.rename(ROOT, "g", ROOT, "h", RenameMode::Replace)
            .await
            .unwrap_err(),
        Errno(libc::EXDEV)
    );
    write_file(&d, ROOT, "x", b"1");
    assert_eq!(
        d.rename(ROOT, "x", ROOT, "g", RenameMode::NoReplace)
            .await
            .unwrap_err(),
        Errno(libc::EEXIST)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn directories_persist_when_empty_and_vanish_when_removed() {
    let server = server().await;
    let a = job(&server, MAIN).await;
    a.mkdir(ROOT, "empty", 0o700).unwrap();
    let p = a.mkdir(ROOT, "p", 0o755).unwrap();
    let q = a.mkdir(p.ino, "q", 0o755).unwrap();
    write_file(&a, q.ino, "f", b"x");
    drained(&a).await;
    assert_eq!(keys(&server), ["fusefs/empty/", "fusefs/p/q/f"]);

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
    let a = job(&server, MAIN).await;
    let d = a.mkdir(ROOT, "d", 0o755).unwrap();
    write_file(&a, d.ino, "a", b"A");
    let sub = a.mkdir(d.ino, "sub", 0o755).unwrap();
    write_file(&a, sub.ino, "b", b"B");
    a.rename(ROOT, "d", ROOT, "e", RenameMode::Replace)
        .await
        .unwrap();
    drained(&a).await;
    assert_eq!(keys(&server), ["fusefs/e/a", "fusefs/e/sub/b"]);

    let b = job(&server, MAIN).await;
    assert_eq!(cat(&b, "e/sub/b").await, b"B");
    // Remote content cannot be moved in place.
    assert_eq!(
        b.rename(ROOT, "e", ROOT, "f", RenameMode::Replace)
            .await
            .unwrap_err(),
        Errno(libc::EXDEV)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn large_files_use_block_uploads_and_ranged_reads() {
    let server = server().await;
    let a = job(&server, MAIN).await;
    let data = noise(40 << 20, 7);
    write_file(&a, ROOT, "big", &data);
    drained(&a).await;
    assert_eq!(server.entries()[0].2, data.len() as u64);

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
async fn fsync_commits_before_close() {
    let server = server().await;
    let a = job_with(&server, MAIN, |c| c.settle = Duration::from_secs(3600)).await;
    let (_, fh) = a.create(ROOT, "log", 0o644, libc::O_WRONLY).unwrap();
    a.write(fh, 0, b"first").unwrap();
    a.fsync(fh).await.unwrap();
    assert_eq!(keys(&server), ["fusefs/log"]);
    let b = job(&server, MAIN).await;
    assert_eq!(cat(&b, "log").await, b"first");
    a.write(fh, 5, b" second").unwrap();
    a.release(fh).unwrap();
    drained(&a).await;
    let c = job(&server, MAIN).await;
    assert_eq!(cat(&c, "log").await, b"first second");
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
    let url = api.twirp.create("fusefs/foreign", &version).await.unwrap();
    api.blob
        .put_blob(&url, bytes::Bytes::from_static(b"tar"))
        .await
        .unwrap();
    api.twirp
        .finalize("fusefs/foreign", &version, 3)
        .await
        .unwrap();
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
    assert_eq!(cat(&b, "src/f00").await, noise(3000, 0));
    // Wait for the background fetches to settle.
    let mut last = server.blob_requests();
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let now = server.blob_requests();
        if now == last && now > 1 {
            break;
        }
        last = now;
    }
    let before = server.blob_requests();
    assert!(
        before >= 20,
        "siblings were not prefetched: {before} blob requests"
    );
    for i in 1..20u64 {
        assert_eq!(cat(&b, &format!("src/f{i:02}")).await, noise(3000, i));
    }
    assert_eq!(
        server.blob_requests(),
        before,
        "a prefetched file was fetched again"
    );
    // Large files are left alone until they are read.
    assert_eq!(cat(&b, "src/big").await, noise(3 << 20, 99));
    assert!(server.blob_requests() > before);
}
