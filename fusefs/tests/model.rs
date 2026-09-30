//! The filesystem core against a model: hegel drives a mount with random
//! POSIX operations, and after every one the tree the mount shows must be
//! the tree an in-memory model of POSIX says it should. Remounting drains
//! the mount into the fake cache service and starts a new one, which must
//! show the same tree, down to the mtimes, whether from its layers or from
//! a snapshot of them. Along the way, a second mount that missed all that
//! takes over with a refresh, mounts show a directory of the volume
//! (`--root`), and the model moves to a feature branch, after which the
//! default branch must still show what it had.
//!
//! A long hunt: `HEGEL_TEST_CASES=5000 cargo test --release --test model`.
//! A failure found once replays from `.hegel/` until it is fixed.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hegel::TestCase;
use hegel::generators as gs;

use gha_cache_fusefs::api::Api;
use gha_cache_fusefs::data::DataStore;
use gha_cache_fusefs::entry::Volume;
use gha_cache_fusefs::fake::{FakeConfig, FakeServer};
use gha_cache_fusefs::vfs::{
    Attr, Errno, FileKind, FsyncMode, Ino, ROOT, RenameMode, SetAttr, SetTime, Vfs, VfsConfig,
};

const MAIN: &str = "refs/heads/main";
const FEATURE: &str = "refs/heads/feature";

/// Few names, so that operations collide.
const NAMES: [&str; 3] = ["a", "b", "c"];
const MODES: [u32; 5] = [0o644, 0o755, 0o600, 0o700, 0o4751];

type Id = u64;

#[derive(Clone, Debug)]
enum Body {
    Dir,
    File(Vec<u8>),
    Symlink(String),
}

#[derive(Clone, Debug)]
struct Node {
    body: Body,
    mode: u16,
    /// Known after `utimens`; any other change makes it unknown.
    mtime: Option<SystemTime>,
}

/// POSIX, by path, with inode identities so that open handles follow
/// renames and outlive unlinks.
#[derive(Clone, Debug, Default)]
struct Model {
    paths: BTreeMap<String, Id>,
    nodes: HashMap<Id, Node>,
    next: Id,
}

fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(p, _)| p)
}

fn within(path: &str, dir: &str) -> bool {
    path == dir || path.starts_with(&format!("{dir}/"))
}

fn errno(e: i32) -> Errno {
    Errno(e)
}

impl Model {
    fn get(&self, path: &str) -> Option<&Node> {
        self.paths.get(path).map(|id| &self.nodes[id])
    }

    /// What resolving the directory `path` component by component gives.
    fn resolve_dir(&self, path: &str) -> Result<(), Errno> {
        if path.is_empty() {
            return Ok(());
        }
        let mut p = String::new();
        for part in path.split('/') {
            if !p.is_empty() {
                p.push('/');
            }
            p.push_str(part);
            match self.get(&p) {
                None => return Err(errno(libc::ENOENT)),
                Some(Node {
                    body: Body::Dir, ..
                }) => {}
                Some(_) => return Err(errno(libc::ENOTDIR)),
            }
        }
        Ok(())
    }

    fn children(&self, dir: &str) -> impl Iterator<Item = &String> {
        self.paths
            .keys()
            .filter(move |p| !p.is_empty() && parent(p) == dir && *p != dir)
    }

    fn touch_dir(&mut self, dir: &str) {
        if let Some(id) = self.paths.get(dir) {
            self.nodes.get_mut(id).unwrap().mtime = None;
        }
    }

    fn insert(&mut self, path: &str, node: Node) -> Id {
        let id = self.next;
        self.next += 1;
        self.nodes.insert(id, node);
        self.paths.insert(path.to_string(), id);
        self.touch_dir(parent(path));
        id
    }

    fn remove(&mut self, path: &str) {
        self.paths.remove(path);
        self.touch_dir(parent(path));
    }

    fn mkdir(&mut self, path: &str, mode: u32) -> Result<(), Errno> {
        self.resolve_dir(parent(path))?;
        if self.paths.contains_key(path) {
            return Err(errno(libc::EEXIST));
        }
        self.insert(
            path,
            Node {
                body: Body::Dir,
                mode: (mode & 0o7777) as u16,
                mtime: None,
            },
        );
        Ok(())
    }

    fn rmdir(&mut self, path: &str) -> Result<(), Errno> {
        self.resolve_dir(parent(path))?;
        match self.get(path) {
            None => Err(errno(libc::ENOENT)),
            Some(Node {
                body: Body::Dir, ..
            }) => {
                if self.children(path).next().is_some() {
                    return Err(errno(libc::ENOTEMPTY));
                }
                self.remove(path);
                Ok(())
            }
            Some(_) => Err(errno(libc::ENOTDIR)),
        }
    }

    fn unlink(&mut self, path: &str) -> Result<(), Errno> {
        self.resolve_dir(parent(path))?;
        match self.get(path) {
            None => Err(errno(libc::ENOENT)),
            Some(Node {
                body: Body::Dir, ..
            }) => Err(errno(libc::EISDIR)),
            Some(_) => {
                self.remove(path);
                Ok(())
            }
        }
    }

    fn symlink(&mut self, path: &str, target: &str) -> Result<(), Errno> {
        self.resolve_dir(parent(path))?;
        if self.paths.contains_key(path) {
            return Err(errno(libc::EEXIST));
        }
        self.insert(
            path,
            Node {
                body: Body::Symlink(target.to_string()),
                mode: 0o777,
                mtime: None,
            },
        );
        Ok(())
    }

    /// `open` of an existing path: the file it names.
    fn open(&mut self, path: &str, trunc: bool) -> Result<Id, Errno> {
        self.resolve_dir(parent(path))?;
        let id = *self.paths.get(path).ok_or(errno(libc::ENOENT))?;
        let node = self.nodes.get_mut(&id).unwrap();
        match &mut node.body {
            Body::Dir => Err(errno(libc::EISDIR)),
            Body::Symlink(_) => Err(errno(libc::ELOOP)),
            Body::File(data) => {
                if trunc {
                    data.clear();
                    node.mtime = None;
                }
                Ok(id)
            }
        }
    }

    /// `open(O_CREAT | O_TRUNC)`, and `O_EXCL` if `excl`.
    fn create(&mut self, path: &str, mode: u32, excl: bool) -> Result<Id, Errno> {
        self.resolve_dir(parent(path))?;
        if self.paths.contains_key(path) {
            if excl {
                return Err(errno(libc::EEXIST));
            }
            return self.open(path, true);
        }
        Ok(self.insert(
            path,
            Node {
                body: Body::File(Vec::new()),
                mode: (mode & 0o7777) as u16,
                mtime: None,
            },
        ))
    }

    fn write(&mut self, id: Id, offset: usize, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let node = self.nodes.get_mut(&id).unwrap();
        let Body::File(data) = &mut node.body else {
            unreachable!("handles are of files")
        };
        if data.len() < offset + bytes.len() {
            data.resize(offset + bytes.len(), 0);
        }
        data[offset..offset + bytes.len()].copy_from_slice(bytes);
        node.mtime = None;
    }

    fn data(&self, id: Id) -> &[u8] {
        match &self.nodes[&id].body {
            Body::File(data) => data,
            _ => unreachable!("handles are of files"),
        }
    }

    fn setattr(&mut self, path: &str, set: &Set) -> Result<(), Errno> {
        self.resolve_dir(parent(path))?;
        let id = *self.paths.get(path).ok_or(errno(libc::ENOENT))?;
        let node = self.nodes.get_mut(&id).unwrap();
        match (&mut node.body, set) {
            (Body::File(data), Set::Size(n)) => {
                data.resize(*n as usize, 0);
                node.mtime = None;
            }
            (_, Set::Size(_)) => unreachable!("only files are truncated"),
            // The mode of a symlink is always 0777.
            (Body::Symlink(_), Set::Mode(_)) => {}
            (_, Set::Mode(m)) => node.mode = (*m & 0o7777) as u16,
            (_, Set::Mtime(t)) => node.mtime = Some(*t),
        }
        Ok(())
    }

    fn rename(&mut self, from: &str, to: &str, noreplace: bool) -> Result<(), Errno> {
        self.resolve_dir(parent(from))?;
        self.resolve_dir(parent(to))?;
        let src = self.get(from).ok_or(errno(libc::ENOENT))?;
        if from == to {
            return Ok(());
        }
        let dst = self.get(to);
        if dst.is_some() && noreplace {
            return Err(errno(libc::EEXIST));
        }
        if matches!(src.body, Body::Dir) {
            match dst {
                Some(Node {
                    body: Body::Dir, ..
                }) => {
                    if self.children(to).next().is_some() {
                        return Err(errno(libc::ENOTEMPTY));
                    }
                }
                Some(_) => return Err(errno(libc::ENOTDIR)),
                None => {}
            }
            if within(parent(to), from) {
                return Err(errno(libc::EINVAL));
            }
        } else if matches!(
            dst,
            Some(Node {
                body: Body::Dir,
                ..
            })
        ) {
            return Err(errno(libc::EISDIR));
        }
        self.remove(to);
        let moved: Vec<(String, Id)> = self
            .paths
            .iter()
            .filter(|(p, _)| within(p, from))
            .map(|(p, id)| (p.clone(), *id))
            .collect();
        for (p, _) in &moved {
            self.paths.remove(p);
        }
        for (p, id) in moved {
            self.paths.insert(format!("{to}{}", &p[from.len()..]), id);
        }
        self.touch_dir(parent(from));
        self.touch_dir(parent(to));
        Ok(())
    }

    /// The tree below `root` as a mount of it should show it.
    fn expected(&self, root: &str) -> BTreeMap<String, Seen> {
        self.paths
            .iter()
            .filter_map(|(path, id)| {
                let rel = if root.is_empty() {
                    path.as_str()
                } else {
                    path.strip_prefix(root)?.strip_prefix('/')?
                };
                Some((path, rel, id))
            })
            .map(|(path, rel, id)| {
                let node = &self.nodes[id];
                let subdirs = self
                    .children(path)
                    .filter(|c| matches!(self.get(c).unwrap().body, Body::Dir))
                    .count() as u32;
                let seen = match &node.body {
                    Body::Dir => Seen {
                        kind: FileKind::Dir,
                        perm: node.mode,
                        size: 4096,
                        nlink: 2 + subdirs,
                        content: None,
                        mtime: node.mtime,
                    },
                    Body::File(data) => Seen {
                        kind: FileKind::File,
                        perm: node.mode,
                        size: data.len() as u64,
                        nlink: 1,
                        content: Some(Content(data.clone())),
                        mtime: node.mtime,
                    },
                    Body::Symlink(target) => Seen {
                        kind: FileKind::Symlink,
                        perm: 0o777,
                        size: target.len() as u64,
                        nlink: 1,
                        content: Some(Content(target.as_bytes().to_vec())),
                        mtime: node.mtime,
                    },
                };
                (rel.to_string(), seen)
            })
            .collect()
    }

    fn dirs(&self) -> Vec<String> {
        self.paths
            .iter()
            .filter(|(_, id)| matches!(self.nodes[id].body, Body::Dir))
            .map(|(p, _)| p.clone())
            .collect()
    }
}

#[derive(Clone, Debug)]
enum Set {
    Size(u64),
    Mode(u32),
    Mtime(SystemTime),
}

/// File data, or a symlink target, printed briefly.
#[derive(Clone, PartialEq, Eq)]
struct Content(Vec<u8>);

impl std::fmt::Debug for Content {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let h = self
            .0
            .iter()
            .fold(0u64, |h, b| h.wrapping_mul(31).wrapping_add(*b as u64));
        if self.0.len() <= 16 {
            write!(f, "{:?}", String::from_utf8_lossy(&self.0))
        } else {
            write!(f, "<{} bytes, {:016x}>", self.0.len(), h)
        }
    }
}

/// What a path looks like.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Seen {
    kind: FileKind,
    perm: u16,
    size: u64,
    nlink: u32,
    content: Option<Content>,
    /// Compared only where the model knows it.
    mtime: Option<SystemTime>,
}

impl Seen {
    fn agrees(&self, want: &Seen) -> bool {
        let mut got = self.clone();
        if want.mtime.is_none() {
            got.mtime = None;
        }
        got == *want
    }
}

/// Deterministic bytes: `len` of them from `seed`.
fn pattern(seed: u8, len: usize) -> Vec<u8> {
    let mut x = (seed as u64 + 1).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

#[derive(Clone, Debug)]
struct JobConfig {
    git_ref: &'static str,
    /// The directory of the volume it shows.
    root: String,
    settle: Duration,
    fsync: FsyncMode,
    /// Snapshot at unmount once this many layers would be covered (0: never).
    snapshot_after: usize,
}

struct Job {
    vfs: Vfs,
    root: String,
    _dir: tempfile::TempDir,
}

struct Handle {
    fh: u64,
    id: Id,
    writable: bool,
}

struct Fs {
    /// Two mounts of the cache, one of them active: the other is as it was
    /// when it last handed over, and has missed everything since.
    jobs: [Option<Job>; 2],
    active: usize,
    /// The active mount's open files.
    handles: Vec<Handle>,
    model: Model,
    /// Where mounts start from: the default branch, until the model moves
    /// to a feature branch, keeping the default branch's tree here.
    git_ref: &'static str,
    main: Option<Model>,
    /// Per branch; building one loads the system's certificates, which
    /// takes a while.
    apis: HashMap<&'static str, Api>,
    server: FakeServer,
    rt: tokio::runtime::Runtime,
}

async fn mount(server: &FakeServer, api: &Api, cfg: &JobConfig) -> Job {
    let env = server.env(cfg.git_ref, &[]);
    let dir = tempfile::tempdir().unwrap();
    let store = DataStore::new(&dir.path().join("data"), 1 << 30, 8).unwrap();
    let mut vcfg = VfsConfig::new(Volume::new("default").unwrap())
        .with_root(&cfg.root)
        .unwrap();
    vcfg.settle = cfg.settle;
    vcfg.fsync = cfg.fsync;
    vcfg.snapshot_after = cfg.snapshot_after;
    vcfg.snapshot_margin = Duration::ZERO;
    vcfg.refresh = None;
    let vfs = Vfs::load(vcfg, api.clone(), store, env.scopes())
        .await
        .unwrap();
    Job {
        vfs,
        root: cfg.root.clone(),
        _dir: dir,
    }
}

async fn resolve(vfs: &Vfs, path: &str) -> Result<Ino, Errno> {
    let mut ino = ROOT;
    for part in path.split('/').filter(|p| !p.is_empty()) {
        ino = vfs.lookup(ino, part).await?.ino;
    }
    Ok(ino)
}

/// The parent directory's inode and the name. As the kernel's path walk,
/// a parent that is not a directory is `ENOTDIR`.
async fn locate<'a>(vfs: &Vfs, path: &'a str) -> Result<(Ino, &'a str), Errno> {
    let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
    let ino = resolve(vfs, dir).await?;
    if vfs.getattr(ino)?.kind != FileKind::Dir {
        return Err(errno(libc::ENOTDIR));
    }
    Ok((ino, name))
}

async fn read_all(vfs: &Vfs, fh: u64) -> Result<Vec<u8>, Errno> {
    let mut out = Vec::new();
    loop {
        let chunk = vfs.read(fh, out.len() as u64, 128 * 1024).await?;
        if chunk.is_empty() {
            return Ok(out);
        }
        out.extend(chunk);
    }
}

fn seen(attr: &Attr, content: Option<Vec<u8>>) -> Seen {
    Seen {
        kind: attr.kind,
        perm: attr.perm,
        size: attr.size,
        nlink: attr.nlink,
        content: content.map(Content),
        mtime: Some(attr.mtime),
    }
}

/// Everything the mount shows, by path.
async fn observe(vfs: &Vfs) -> BTreeMap<String, Seen> {
    let mut out = BTreeMap::new();
    let mut dirs = vec![(ROOT, String::new())];
    while let Some((dir, path)) = dirs.pop() {
        let fh = vfs.opendir(dir).unwrap();
        let entries = vfs.readdir(dir, fh, 0).unwrap();
        vfs.releasedir(fh);
        for e in entries {
            if e.name == "." || e.name == ".." {
                continue;
            }
            let child = if path.is_empty() {
                e.name.clone()
            } else {
                format!("{path}/{}", e.name)
            };
            let attr = vfs
                .lookup(dir, &e.name)
                .await
                .unwrap_or_else(|err| panic!("{child}: listed, but lookup says {err}"));
            assert_eq!(attr.ino, e.ino, "{child}: readdir and lookup disagree");
            assert_eq!(attr.kind, e.kind, "{child}: readdir and lookup disagree");
            let content = match attr.kind {
                FileKind::Dir => {
                    dirs.push((attr.ino, child.clone()));
                    None
                }
                FileKind::File => {
                    let (fh, _) = vfs
                        .open(attr.ino, libc::O_RDONLY)
                        .await
                        .unwrap_or_else(|err| panic!("{child}: open: {err}"));
                    let data = read_all(vfs, fh)
                        .await
                        .unwrap_or_else(|err| panic!("{child}: read: {err}"));
                    vfs.release(fh).unwrap();
                    Some(data)
                }
                FileKind::Symlink => Some(vfs.readlink(attr.ino).await.unwrap().into_bytes()),
            };
            out.insert(child, seen(&attr, content));
        }
    }
    out
}

fn forget_dir_mtimes(mut tree: BTreeMap<String, Seen>) -> BTreeMap<String, Seen> {
    for seen in tree.values_mut() {
        if seen.kind == FileKind::Dir {
            seen.mtime = None;
        }
    }
    tree
}

fn diff(got: &BTreeMap<String, Seen>, want: &BTreeMap<String, Seen>) -> Vec<String> {
    let mut out = Vec::new();
    for (path, w) in want {
        match got.get(path) {
            None => out.push(format!("missing {path}: want {w:?}")),
            Some(g) if !g.agrees(w) => {
                out.push(format!("wrong {path}:\n   got {g:?}\n  want {w:?}"))
            }
            Some(_) => {}
        }
    }
    for (path, g) in got {
        if !want.contains_key(path) {
            out.push(format!("extra {path}: {g:?}"));
        }
    }
    out
}

fn draw_path(tc: &TestCase) -> String {
    let depth = tc.draw(gs::integers::<usize>().min_value(1).max_value(3));
    (0..depth)
        .map(|_| tc.draw(gs::sampled_from(&NAMES[..])))
        .collect::<Vec<_>>()
        .join("/")
}

/// Below `root`: a path that exists, or a new name in a directory that
/// exists, mostly.
fn draw_existing(tc: &TestCase, model: &Model, root: &str) -> String {
    let rel = |p: &str| -> Option<String> {
        if root.is_empty() {
            Some(p.to_string())
        } else {
            Some(p.strip_prefix(root)?.strip_prefix('/')?.to_string())
        }
    };
    let paths: Vec<String> = model.paths.keys().filter_map(|p| rel(p)).collect();
    match tc.draw(gs::integers::<u8>().max_value(9)) {
        0 => draw_path(tc),
        1..=5 if !paths.is_empty() => tc.draw(gs::sampled_from(paths)),
        _ => {
            let mut dirs: Vec<String> = model.dirs().iter().filter_map(|p| rel(p)).collect();
            dirs.insert(0, String::new());
            let dir = tc.draw(gs::sampled_from(dirs));
            let name = tc.draw(gs::sampled_from(&NAMES[..]));
            if dir.is_empty() {
                name.to_string()
            } else {
                format!("{dir}/{name}")
            }
        }
    }
}

fn draw_data(tc: &TestCase) -> Vec<u8> {
    let seed = tc.draw(gs::integers::<u8>());
    let len = match tc.draw(gs::integers::<u8>().max_value(39)) {
        0..=3 => 0,
        4..=23 => tc.draw(gs::integers::<usize>().min_value(1).max_value(64)),
        24..=38 => tc.draw(gs::integers::<usize>().min_value(65).max_value(9000)),
        // Past LAYER_FILE_MAX: a blob.
        _ => (8 << 20) + tc.draw(gs::integers::<usize>().max_value(5000)),
    };
    pattern(seed, len)
}

fn draw_offset(tc: &TestCase) -> usize {
    tc.draw(gs::integers::<usize>().max_value(5000))
}

fn check(what: &str, got: Result<(), Errno>, want: Result<(), Errno>) {
    assert_eq!(got, want, "{what}: the mount and the model disagree");
}

impl Fs {
    fn vfs(&self) -> &Vfs {
        &self.jobs[self.active].as_ref().unwrap().vfs
    }

    fn root(&self) -> &str {
        &self.jobs[self.active].as_ref().unwrap().root
    }

    /// The path in the volume of a path of the active mount.
    fn full(&self, path: &str) -> String {
        match self.root() {
            "" => path.to_string(),
            root => format!("{root}/{path}"),
        }
    }

    fn draw(&self, tc: &TestCase) -> String {
        draw_existing(tc, &self.model, self.root())
    }

    fn api(&mut self, git_ref: &'static str) -> Api {
        let server = &self.server;
        self.apis
            .entry(git_ref)
            .or_insert_with(|| Api::new(&server.env(git_ref, &[])).unwrap())
            .clone()
    }

    /// Drains the active mount, checks it lost nothing, and mounts again.
    fn mount_again(&mut self, tc: &TestCase, cfg: JobConfig) {
        tc.note(&format!("mount {} again with {cfg:?}", self.active));
        self.handles.clear();
        let old = self.jobs[self.active].take();
        let api = self.api(cfg.git_ref);
        let job = self.rt.block_on(async {
            let mut before = None;
            if let Some(old) = old {
                let summary = old.vfs.drain().await;
                assert!(
                    summary.failures.is_empty(),
                    "failures: {:?}",
                    summary.failures
                );
                assert_eq!(old.vfs.pending(), 0, "drained, yet ops are pending");
                if old.root == cfg.root {
                    before = Some(observe(&old.vfs).await);
                }
            }
            let job = mount(&self.server, &api, &cfg).await;
            if let Some(before) = before {
                // Everything, file mtimes included, survives the round
                // trip. A directory's mtime is its last mark's (LAYERS.md
                // §4), which a change to its entries does not write: the
                // model knows it only after `utimens`.
                let after = observe(&job.vfs).await;
                let before = forget_dir_mtimes(before);
                let after = forget_dir_mtimes(after);
                let d = diff(&after, &before);
                assert!(d.is_empty(), "after a remount:\n{}", d.join("\n"));
            }
            job
        });
        self.jobs[self.active] = Some(job);
    }

    fn draw_config(&self, tc: &TestCase) -> JobConfig {
        let dirs = self.model.dirs();
        JobConfig {
            git_ref: self.git_ref,
            root: if dirs.is_empty() || tc.draw(gs::integers::<u8>().max_value(3)) > 0 {
                String::new()
            } else {
                tc.draw(gs::sampled_from(dirs))
            },
            settle: Duration::from_millis(tc.draw(gs::sampled_from(&[0u64, 5, 30, 3_600_000][..]))),
            fsync: if tc.draw(gs::booleans()) {
                FsyncMode::Local
            } else {
                FsyncMode::Commit
            },
            snapshot_after: tc.draw(gs::sampled_from(&[0usize, 1, 3][..])),
        }
    }
}

#[hegel::state_machine]
impl Fs {
    #[rule]
    fn mkdir(&mut self, tc: TestCase) {
        let path = self.draw(&tc);
        let mode = tc.draw(gs::sampled_from(&MODES[..]));
        tc.note(&format!("mkdir {path} {mode:o}"));
        let got = self.rt.block_on(async {
            let (dir, name) = locate(self.vfs(), &path).await?;
            self.vfs().mkdir(dir, name, mode).map(|_| ())
        });
        let path = self.full(&path);
        check("mkdir", got, self.model.mkdir(&path, mode));
    }

    #[rule]
    fn rmdir(&mut self, tc: TestCase) {
        let path = self.draw(&tc);
        tc.note(&format!("rmdir {path}"));
        let got = self.rt.block_on(async {
            let (dir, name) = locate(self.vfs(), &path).await?;
            self.vfs().rmdir(dir, name)
        });
        let path = self.full(&path);
        check("rmdir", got, self.model.rmdir(&path));
    }

    #[rule]
    fn unlink(&mut self, tc: TestCase) {
        let path = self.draw(&tc);
        tc.note(&format!("unlink {path}"));
        let got = self.rt.block_on(async {
            let (dir, name) = locate(self.vfs(), &path).await?;
            self.vfs().unlink(dir, name)
        });
        let path = self.full(&path);
        check("unlink", got, self.model.unlink(&path));
    }

    #[rule]
    fn symlink(&mut self, tc: TestCase) {
        let path = self.draw(&tc);
        let target = draw_path(&tc);
        tc.note(&format!("symlink {path} -> {target}"));
        let got = self.rt.block_on(async {
            let (dir, name) = locate(self.vfs(), &path).await?;
            self.vfs().symlink(dir, name, &target).map(|_| ())
        });
        let path = self.full(&path);
        check("symlink", got, self.model.symlink(&path, &target));
    }

    /// `echo > path`: create or truncate, write, close.
    #[rule(weight = 3.0)]
    fn write_file(&mut self, tc: TestCase) {
        let path = self.draw(&tc);
        let data = draw_data(&tc);
        let excl = tc.draw(gs::booleans());
        let mode = tc.draw(gs::sampled_from(&MODES[..]));
        tc.note(&format!(
            "write_file {path} {:?} mode {mode:o}{}",
            Content(data.clone()),
            if excl { " O_EXCL" } else { "" }
        ));
        let got = self.rt.block_on(async {
            let vfs = self.vfs();
            let (dir, name) = locate(vfs, &path).await?;
            let flags = libc::O_WRONLY
                | libc::O_CREAT
                | libc::O_TRUNC
                | if excl { libc::O_EXCL } else { 0 };
            let (_, fh) = vfs.create(dir, name, mode, flags)?;
            for (i, chunk) in data.chunks(128 * 1024).enumerate() {
                let n = vfs.write(fh, (i * 128 * 1024) as u64, chunk)?;
                assert_eq!(n as usize, chunk.len());
            }
            vfs.release(fh)
        });
        let path = self.full(&path);
        let want = self.model.create(&path, mode, excl).map(|id| {
            self.model.write(id, 0, &data);
        });
        check("write_file", got, want);
    }

    /// Opens a file and keeps the handle.
    #[rule(weight = 2.0)]
    fn open(&mut self, tc: TestCase) {
        let path = self.draw(&tc);
        let access = tc.draw(gs::sampled_from(
            &[libc::O_RDONLY, libc::O_WRONLY, libc::O_RDWR][..],
        ));
        let trunc = access != libc::O_RDONLY && tc.draw(gs::booleans());
        let flags = access | if trunc { libc::O_TRUNC } else { 0 };
        tc.note(&format!(
            "open {path} {}{}",
            match access {
                libc::O_RDONLY => "O_RDONLY",
                libc::O_WRONLY => "O_WRONLY",
                _ => "O_RDWR",
            },
            if trunc { "|O_TRUNC" } else { "" }
        ));
        let got = self.rt.block_on(async {
            let vfs = self.vfs();
            let ino = resolve(vfs, &path).await?;
            vfs.open(ino, flags).await.map(|(fh, _)| fh)
        });
        let path = self.full(&path);
        let want = self.model.open(&path, trunc);
        match (got, want) {
            (Ok(fh), Ok(id)) => {
                tc.note(&format!("  -> handle {}", self.handles.len()));
                self.handles.push(Handle {
                    fh,
                    id,
                    writable: access != libc::O_RDONLY,
                });
            }
            (got, want) => check("open", got.map(|_| ()), want.map(|_| ())),
        }
    }

    #[rule(weight = 2.0)]
    fn write(&mut self, tc: TestCase) {
        tc.assume(!self.handles.is_empty());
        let i = tc.draw(gs::integers::<usize>().max_value(self.handles.len() - 1));
        let offset = draw_offset(&tc);
        let data = draw_data(&tc);
        tc.note(&format!(
            "write handle {i} at {offset}: {:?}",
            Content(data.clone())
        ));
        let h = &self.handles[i];
        let got = self.vfs().write(h.fh, offset as u64, &data).map(|_| ());
        if h.writable {
            let id = h.id;
            check("write", got, Ok(()));
            self.model.write(id, offset, &data);
        } else {
            check("write", got, Err(errno(libc::EBADF)));
        }
    }

    #[rule]
    fn read(&mut self, tc: TestCase) {
        tc.assume(!self.handles.is_empty());
        let i = tc.draw(gs::integers::<usize>().max_value(self.handles.len() - 1));
        tc.note(&format!("read handle {i}"));
        let h = &self.handles[i];
        let got = self.rt.block_on(read_all(self.vfs(), h.fh)).unwrap();
        // POSIX: the file as it is now. (A refresh that brings a newer
        // version leaves a handle with the one it opened, DESIGN.md §6, but
        // handles here close before a refresh.)
        let want = self.model.data(h.id);
        assert!(
            got == want,
            "read through handle {i}: got {:?}, want {:?}",
            Content(got.clone()),
            Content(want.to_vec()),
        );
    }

    #[rule]
    fn fsync(&mut self, tc: TestCase) {
        tc.assume(!self.handles.is_empty());
        let i = tc.draw(gs::integers::<usize>().max_value(self.handles.len() - 1));
        tc.note(&format!("fsync handle {i}"));
        let fh = self.handles[i].fh;
        let got = self.rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(30), self.vfs().fsync(fh)).await
        });
        let got = got.expect("fsync hangs");
        if got.is_err() {
            tc.note(&format!("failures: {:?}", self.vfs().summary().failures));
        }
        check("fsync", got, Ok(()));
    }

    #[rule(weight = 2.0)]
    fn close(&mut self, tc: TestCase) {
        tc.assume(!self.handles.is_empty());
        let i = tc.draw(gs::integers::<usize>().max_value(self.handles.len() - 1));
        tc.note(&format!("close handle {i}"));
        let h = self.handles.remove(i);
        check("close", self.vfs().release(h.fh), Ok(()));
    }

    #[rule]
    fn truncate(&mut self, tc: TestCase) {
        let path = self.draw(&tc);
        tc.assume(matches!(
            self.model.get(&self.full(&path)),
            Some(Node {
                body: Body::File(_),
                ..
            })
        ));
        let size = draw_offset(&tc) as u64;
        tc.note(&format!("truncate {path} {size}"));
        self.setattr(&path, Set::Size(size));
    }

    #[rule]
    fn chmod(&mut self, tc: TestCase) {
        let path = self.draw(&tc);
        let mode = tc.draw(gs::sampled_from(&MODES[..]));
        tc.note(&format!("chmod {path} {mode:o}"));
        self.setattr(&path, Set::Mode(mode));
    }

    #[rule]
    fn utimens(&mut self, tc: TestCase) {
        let path = self.draw(&tc);
        let secs = tc.draw(gs::integers::<u64>().min_value(1).max_value(2_000_000_000));
        let nanos = tc.draw(gs::sampled_from(&[0u32, 1, 999_999_999, 123_456_789][..]));
        let t = UNIX_EPOCH + Duration::new(secs, nanos);
        tc.note(&format!("utimens {path} {secs}.{nanos:09}"));
        self.setattr(&path, Set::Mtime(t));
    }

    #[rule(weight = 3.0)]
    fn rename(&mut self, tc: TestCase) {
        let from = self.draw(&tc);
        let to = self.draw(&tc);
        let noreplace = tc.draw(gs::booleans());
        tc.note(&format!(
            "rename {from} {to}{}",
            if noreplace { " NOREPLACE" } else { "" }
        ));
        let got = self.rt.block_on(async {
            let vfs = self.vfs();
            let (dir, name) = locate(vfs, &from).await?;
            let (newdir, newname) = locate(vfs, &to).await?;
            let mode = if noreplace {
                RenameMode::NoReplace
            } else {
                RenameMode::Replace
            };
            vfs.rename(dir, name, newdir, newname, mode).await
        });
        let (from, to) = (self.full(&from), self.full(&to));
        check("rename", got, self.model.rename(&from, &to, noreplace));
    }

    /// Lets the committer catch up, part of the way or all of it.
    #[rule]
    fn pause(&mut self, tc: TestCase) {
        let ms = tc.draw(gs::sampled_from(&[1u64, 10, 50, 200][..]));
        tc.note(&format!("pause {ms}ms"));
        self.rt
            .block_on(async { tokio::time::sleep(Duration::from_millis(ms)).await });
    }

    #[rule]
    fn remount(&mut self, tc: TestCase) {
        let cfg = self.draw_config(&tc);
        self.mount_again(&tc, cfg);
    }

    /// The active mount ends, and the other one takes over: it picks up
    /// what it missed with a refresh, and must then show the model.
    #[rule]
    fn handoff(&mut self, tc: TestCase) {
        let cfg = self.draw_config(&tc);
        self.mount_again(&tc, cfg);
        self.active = 1 - self.active;
        tc.note(&format!("mount {} refreshes", self.active));
        self.rt.block_on(self.vfs().refresh()).unwrap();
        let root = self.root().to_string();
        if !root.is_empty() && !self.model.dirs().contains(&root) {
            // Its root went away: it shows nothing, and nothing the model
            // can say would be made there.
            let got = self.rt.block_on(observe(self.vfs()));
            assert!(
                got.is_empty(),
                "{root} is gone, yet the mount shows {got:?}"
            );
            let mut cfg = self.draw_config(&tc);
            cfg.root = String::new();
            self.mount_again(&tc, cfg);
        }
    }

    /// Moves to a feature branch: mounts from now on read the default
    /// branch's layers and write their own.
    #[rule]
    fn branch(&mut self, tc: TestCase) {
        tc.assume(self.git_ref == MAIN);
        tc.note("branch");
        self.git_ref = FEATURE;
        self.main = Some(self.model.clone());
        for _ in 0..2 {
            // Up to date first, so that the new mount must show the same.
            self.rt.block_on(self.vfs().refresh()).unwrap();
            let cfg = self.draw_config(&tc);
            self.mount_again(&tc, cfg);
            self.active = 1 - self.active;
        }
    }

    /// The default branch never sees the feature branch's changes.
    #[rule]
    fn check_main(&mut self, tc: TestCase) {
        let Some(main) = &self.main else {
            tc.reject();
        };
        tc.note("check_main");
        let cfg = JobConfig {
            git_ref: MAIN,
            root: String::new(),
            settle: Duration::from_secs(3600),
            fsync: FsyncMode::Local,
            snapshot_after: 0,
        };
        let want = main.expected("");
        let api = self.api(MAIN);
        self.rt.block_on(async {
            let job = mount(&self.server, &api, &cfg).await;
            let d = diff(&observe(&job.vfs).await, &want);
            assert!(d.is_empty(), "the default branch shows:\n{}", d.join("\n"));
            let summary = job.vfs.drain().await;
            assert_eq!(
                (summary.layers, summary.failures.len()),
                (0, 0),
                "{summary:?}"
            );
        });
    }

    #[invariant(always_run)]
    fn agrees_with_the_model(&mut self, tc: TestCase) {
        let got = self.rt.block_on(observe(self.vfs()));
        tc.event_value("paths", got.len() as f64);
        let d = diff(&got, &self.model.expected(self.root()));
        assert!(
            d.is_empty(),
            "the mount and the model disagree:\n{}",
            d.join("\n")
        );
    }
}

impl Fs {
    fn setattr(&mut self, path: &str, set: Set) {
        let got = self.rt.block_on(async {
            let vfs = self.vfs();
            let ino = resolve(vfs, path).await?;
            let s = match &set {
                Set::Size(n) => SetAttr {
                    size: Some(*n),
                    ..SetAttr::default()
                },
                Set::Mode(m) => SetAttr {
                    mode: Some(*m),
                    ..SetAttr::default()
                },
                Set::Mtime(t) => SetAttr {
                    mtime: Some(SetTime::At(*t)),
                    ..SetAttr::default()
                },
            };
            vfs.setattr(ino, None, s).await.map(|_| ())
        });
        let path = self.full(path);
        check("setattr", got, self.model.setattr(&path, &set));
    }
}

#[hegel::test]
fn the_mount_agrees_with_the_model(tc: TestCase) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let server = rt
        .block_on(FakeServer::start(FakeConfig::default()))
        .unwrap();
    let mut fs = Fs {
        jobs: [None, None],
        active: 1,
        handles: Vec::new(),
        model: Model::default(),
        git_ref: MAIN,
        main: None,
        apis: HashMap::new(),
        server,
        rt,
    };
    for _ in 0..2 {
        let cfg = fs.draw_config(&tc);
        fs.active = 1 - fs.active;
        fs.mount_again(&tc, cfg);
    }
    hegel::stateful::machine(fs).steps(40).run(tc);
}
