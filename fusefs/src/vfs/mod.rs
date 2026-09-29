//! The filesystem core, independent of FUSE.
//!
//! The namespace is a tree of inodes built from the remote view (the merged
//! listing) plus a local overlay of operations that have not been committed
//! to the cache yet. One operation is pending per key; it is a `Put` of the
//! node currently at that path, or a `Delete` (a whiteout). See DESIGN.md.

mod commit;
mod state;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use crate::api::Api;
use crate::data::{CHUNK, DataFile, DataStore, RemoteData};
use crate::entry::{KeySpace, MAX_KEY_LEN, join, valid_component};
use crate::index::{self, Index};

use state::{Body, Content, Dir, File, Handle, PendingOp, State, Symlink};

pub type Ino = u64;
pub const ROOT: Ino = 1;

/// An errno value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Errno(pub i32);

impl std::fmt::Display for Errno {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", std::io::Error::from_raw_os_error(self.0))
    }
}

pub type Result<T> = std::result::Result<T, Errno>;

const ENOENT: Errno = Errno(libc::ENOENT);
const EEXIST: Errno = Errno(libc::EEXIST);
const ENOTDIR: Errno = Errno(libc::ENOTDIR);
const EISDIR: Errno = Errno(libc::EISDIR);
const ENOTEMPTY: Errno = Errno(libc::ENOTEMPTY);
const EINVAL: Errno = Errno(libc::EINVAL);
const EIO: Errno = Errno(libc::EIO);
const EROFS: Errno = Errno(libc::EROFS);
const EXDEV: Errno = Errno(libc::EXDEV);
const EBADF: Errno = Errno(libc::EBADF);
const ENAMETOOLONG: Errno = Errno(libc::ENAMETOOLONG);

fn io_err(e: std::io::Error) -> Errno {
    tracing::warn!("local I/O error: {e}");
    Errno(e.raw_os_error().unwrap_or(libc::EIO))
}

fn fetch_err(e: String) -> Errno {
    tracing::warn!("{e}");
    EIO
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    Dir,
    File,
    Symlink,
}

#[derive(Clone, Copy, Debug)]
pub struct Attr {
    pub ino: Ino,
    pub kind: FileKind,
    pub size: u64,
    pub mtime: SystemTime,
    pub perm: u16,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
}

#[derive(Clone, Copy, Debug)]
pub enum SetTime {
    Now,
    At(SystemTime),
}

impl SetTime {
    fn resolve(self) -> SystemTime {
        match self {
            SetTime::Now => SystemTime::now(),
            SetTime::At(t) => t,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct SetAttr {
    pub mode: Option<u32>,
    pub size: Option<u64>,
    pub mtime: Option<SetTime>,
}

#[derive(Clone, Debug)]
pub struct DirEntry {
    pub ino: Ino,
    pub kind: FileKind,
    pub name: String,
    pub attr: Attr,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenameMode {
    Replace,
    NoReplace,
}

#[derive(Clone, Debug)]
pub struct VfsConfig {
    pub keys: KeySpace,
    pub read_only: bool,
    /// How long a closed file waits before it is uploaded.
    pub settle: Duration,
    /// Minimum time between refreshes triggered by lookup misses.
    pub refresh: Option<Duration>,
    pub uid: u32,
    pub gid: u32,
    pub upload_concurrency: usize,
    /// Delete superseded entries of the run's own scope (needs `actions: write`).
    pub gc: bool,
    /// Reported as the filesystem size.
    pub quota: u64,
    /// Attempts per operation once unmounting has started.
    pub drain_attempts: u32,
}

impl VfsConfig {
    pub fn new(keys: KeySpace) -> VfsConfig {
        VfsConfig {
            keys,
            read_only: false,
            settle: Duration::from_secs(1),
            refresh: Some(Duration::from_secs(15)),
            uid: unsafe { libc::geteuid() },
            gid: unsafe { libc::getegid() },
            upload_concurrency: 8,
            gc: false,
            quota: 10 << 30,
            drain_attempts: 5,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Failure {
    pub key: String,
    pub error: String,
}

/// What happened during a mount, written at unmount.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Summary {
    pub uploaded_files: u64,
    pub uploaded_bytes: u64,
    pub whiteouts: u64,
    pub dir_markers: u64,
    pub download_requests: u64,
    pub downloaded_bytes: u64,
    pub gc_deleted: u64,
    pub failures: Vec<Failure>,
}

#[derive(Debug, Default)]
struct Stats {
    uploaded_files: AtomicU64,
    uploaded_bytes: AtomicU64,
    whiteouts: AtomicU64,
    dir_markers: AtomicU64,
    gc_deleted: AtomicU64,
}

struct Inner {
    cfg: VfsConfig,
    api: Api,
    store: Arc<DataStore>,
    st: Mutex<State>,
    /// Wakes the committer.
    wake: Notify,
    /// Signalled whenever a commit attempt finishes.
    done: Notify,
    refresh_lock: tokio::sync::Mutex<()>,
    stats: Stats,
    draining: AtomicBool,
    next_attempt: AtomicU64,
}

/// A cheaply cloneable handle to the filesystem.
#[derive(Clone)]
pub struct Vfs(Arc<Inner>);

impl Vfs {
    /// Lists the cache and builds the namespace. Must be called inside a tokio runtime.
    pub async fn load(
        cfg: VfsConfig,
        api: Api,
        store: Arc<DataStore>,
        scopes: Vec<String>,
    ) -> anyhow::Result<Vfs> {
        let mut index = Index::new(scopes);
        let items = index::list_all(&api.rest, cfg.keys.prefix(), index.scopes()).await?;
        let (mut ours, mut foreign) = (0usize, 0usize);
        for item in &items {
            match index.entry_from_item(item) {
                Some(e) => {
                    ours += 1;
                    index.insert(e);
                }
                None => foreign += 1,
            }
        }
        tracing::info!(
            "listed {} entries under {:?} in {:?} ({ours} ours, {foreign} ignored)",
            items.len(),
            cfg.keys.prefix(),
            index.scopes()
        );
        Ok(Vfs::with_index(cfg, api, store, index))
    }

    pub fn with_index(cfg: VfsConfig, api: Api, store: Arc<DataStore>, index: Index) -> Vfs {
        let mut st = State::new(index);
        for key in st.index.keys() {
            st.apply_remote(&cfg, &store, &key);
        }
        let vfs = Vfs(Arc::new(Inner {
            cfg,
            api,
            store,
            st: Mutex::new(st),
            wake: Notify::new(),
            done: Notify::new(),
            refresh_lock: tokio::sync::Mutex::new(()),
            stats: Stats::default(),
            draining: AtomicBool::new(false),
            next_attempt: AtomicU64::new(1),
        }));
        tokio::spawn(commit::run(vfs.0.clone()));
        vfs
    }

    pub fn config(&self) -> &VfsConfig {
        &self.0.cfg
    }

    fn check_writable(&self) -> Result<()> {
        if self.0.cfg.read_only {
            Err(EROFS)
        } else {
            Ok(())
        }
    }

    fn check_name(&self, st: &State, parent: Ino, name: &str, dir: bool) -> Result<String> {
        if !valid_component(name) {
            return Err(EINVAL);
        }
        let path = join(&st.path(parent), name);
        let key = if dir {
            self.0.cfg.keys.dir_key(&path)
        } else {
            self.0.cfg.keys.node_key(&path)
        };
        if key.len() > MAX_KEY_LEN {
            return Err(ENAMETOOLONG);
        }
        Ok(path)
    }

    fn due(&self) -> Option<Instant> {
        Some(Instant::now() + self.0.cfg.settle)
    }

    // ---- lookups ----------------------------------------------------------

    pub async fn lookup(&self, parent: Ino, name: &str) -> Result<Attr> {
        if let Some(a) = self.try_lookup(parent, name)? {
            return Ok(a);
        }
        if self.maybe_refresh().await {
            if let Some(a) = self.try_lookup(parent, name)? {
                return Ok(a);
            }
        }
        Err(ENOENT)
    }

    fn try_lookup(&self, parent: Ino, name: &str) -> Result<Option<Attr>> {
        let st = self.0.st.lock();
        st.dir(parent)?;
        Ok(st.child(parent, name).map(|ino| st.attr(&self.0.cfg, ino)))
    }

    /// Refreshes if the last refresh is older than the configured interval.
    async fn maybe_refresh(&self) -> bool {
        let Some(interval) = self.0.cfg.refresh else {
            return false;
        };
        if self.0.st.lock().last_refresh.elapsed() < interval {
            return false;
        }
        let _guard = self.0.refresh_lock.lock().await;
        if self.0.st.lock().last_refresh.elapsed() < interval {
            // Someone else refreshed while we waited.
            return true;
        }
        match self.refresh().await {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!("refresh failed: {e}");
                false
            }
        }
    }

    /// Picks up entries created since the last listing.
    pub async fn refresh(&self) -> anyhow::Result<()> {
        let (scopes, marks) = {
            let st = self.0.st.lock();
            (st.index.scopes().to_vec(), st.index.watermarks())
        };
        let prefix = self.0.cfg.keys.prefix().to_string();
        let lists = futures_util::future::try_join_all(
            scopes
                .iter()
                .zip(marks)
                .map(|(scope, mark)| index::list_since(&self.0.api.rest, &prefix, scope, mark)),
        )
        .await;
        let mut st = self.0.st.lock();
        st.last_refresh = Instant::now();
        let mut changed = Vec::new();
        for item in lists?.iter().flatten() {
            if let Some(e) = st.index.entry_from_item(item) {
                let key = e.key.clone();
                if st.index.insert(e) {
                    changed.push(key);
                }
            }
        }
        if !changed.is_empty() {
            tracing::debug!("refresh: {} keys changed", changed.len());
        }
        for key in changed {
            st.apply_remote(&self.0.cfg, &self.0.store, &key);
        }
        Ok(())
    }

    pub fn getattr(&self, ino: Ino) -> Result<Attr> {
        let st = self.0.st.lock();
        st.node(ino)?;
        Ok(st.attr(&self.0.cfg, ino))
    }

    // ---- directories ------------------------------------------------------

    /// Snapshots a directory for reading; the snapshot survives concurrent changes.
    pub fn opendir(&self, ino: Ino) -> Result<u64> {
        let mut st = self.0.st.lock();
        let entries = st.dir_snapshot(&self.0.cfg, ino)?;
        let fh = st.alloc_fh();
        st.dir_handles.insert(fh, entries);
        Ok(fh)
    }

    /// Entries from `offset` on; each entry's offset is its index + 1.
    pub fn readdir(&self, ino: Ino, fh: u64, offset: u64) -> Result<Vec<DirEntry>> {
        let mut st = self.0.st.lock();
        if offset == 0 {
            // A rewind: take a fresh snapshot.
            let entries = st.dir_snapshot(&self.0.cfg, ino)?;
            st.dir_handles.insert(fh, entries);
        }
        let entries = st.dir_handles.get(&fh).ok_or(EBADF)?;
        Ok(entries.iter().skip(offset as usize).cloned().collect())
    }

    pub fn releasedir(&self, fh: u64) {
        self.0.st.lock().dir_handles.remove(&fh);
    }

    pub fn mkdir(&self, parent: Ino, name: &str, mode: u32) -> Result<Attr> {
        self.check_writable()?;
        let mut st = self.0.st.lock();
        st.dir(parent)?;
        let path = self.check_name(&st, parent, name, true)?;
        if st.child(parent, name).is_some() {
            return Err(EEXIST);
        }
        let key = self.0.cfg.keys.dir_key(&path);
        let marker = st.index.visible(&key).cloned();
        if st
            .overlay
            .get(&key)
            .is_some_and(|p| matches!(p.op, PendingOp::Delete { .. }))
        {
            // An `rmdir` that has not been committed yet is undone.
            st.clear_op(&key);
        }
        let ino = st.alloc(
            parent,
            name,
            Body::Dir(Dir {
                children: BTreeMap::new(),
                marker,
                pinned: true,
                mode: (mode & 0o7777) as u16,
                mtime: SystemTime::now(),
            }),
        );
        Ok(st.attr(&self.0.cfg, ino))
    }

    pub fn rmdir(&self, parent: Ino, name: &str) -> Result<()> {
        self.check_writable()?;
        let mut st = self.0.st.lock();
        st.dir(parent)?;
        let ino = st.child(parent, name).ok_or(ENOENT)?;
        let dir = st.dir(ino)?;
        if !dir.children.is_empty() {
            return Err(ENOTEMPTY);
        }
        let key = self.0.cfg.keys.dir_key(&st.path(ino));
        st.detach(ino);
        st.pin(parent);
        st.retire_key(&key, None, self.due());
        drop(st);
        self.0.wake.notify_one();
        Ok(())
    }

    // ---- files ------------------------------------------------------------

    pub fn create(&self, parent: Ino, name: &str, mode: u32, flags: i32) -> Result<(Attr, u64)> {
        self.check_writable()?;
        let mut st = self.0.st.lock();
        st.dir(parent)?;
        let path = self.check_name(&st, parent, name, false)?;
        if let Some(existing) = st.child(parent, name) {
            if flags & libc::O_EXCL != 0 {
                return Err(EEXIST);
            }
            drop(st);
            // Racing creators: behave like open(2) on the existing file. If
            // that would mean downloading it, report the race instead.
            return self.open_sync_local(existing, flags);
        }
        let file = self.0.store.create().map_err(io_err)?;
        let ino = st.alloc(
            parent,
            name,
            Body::File(File {
                content: Content::Local(file.clone()),
                size: 0,
                mode: (mode & 0o7777) as u16,
                mtime: SystemTime::now(),
                writers: 1,
                writes_inflight: 0,
                generation: 1,
                dirty: true,
                committed: None,
            }),
        );
        let key = self.0.cfg.keys.node_key(&path);
        st.set_op(&key, PendingOp::Put(ino), None);
        let fh = st.add_handle(Handle::new(ino, Content::Local(file), true));
        Ok((st.attr(&self.0.cfg, ino), fh))
    }

    /// `open` for the cases that never need the network.
    fn open_sync_local(&self, ino: Ino, flags: i32) -> Result<(Attr, u64)> {
        let mut st = self.0.st.lock();
        let (content, writable) = st
            .open_local(ino, flags, &self.0.cfg, &self.0.store)?
            .ok_or(EEXIST)?;
        let fh = st.add_handle(Handle::new(ino, content, writable));
        Ok((st.attr(&self.0.cfg, ino), fh))
    }

    /// Opens a file. Returns the handle and whether the kernel may keep its page cache.
    pub async fn open(&self, ino: Ino, flags: i32) -> Result<(u64, bool)> {
        let writable = flags & libc::O_ACCMODE != libc::O_RDONLY;
        let trunc = flags & libc::O_TRUNC != 0;
        if writable || trunc {
            self.check_writable()?;
        }
        for _ in 0..3 {
            let rd = {
                let mut st = self.0.st.lock();
                if let Some((content, w)) = st.open_local(ino, flags, &self.0.cfg, &self.0.store)? {
                    let keep = st.keep_cache(ino, &content, w);
                    let fh = st.add_handle(Handle::new(ino, content, w));
                    return Ok((fh, keep));
                }
                // Copy-on-write of a remote file: fetch it all first.
                match &st.file(ino)?.content {
                    Content::Remote(rd) => rd.clone(),
                    Content::Local(_) => continue,
                }
            };
            let local = rd.materialize(&self.0.api).await.map_err(fetch_err)?;
            let mut st = self.0.st.lock();
            if st.make_local(ino, &rd, local, &self.0.cfg) {
                if let Some((content, w)) = st.open_local(ino, flags, &self.0.cfg, &self.0.store)? {
                    let fh = st.add_handle(Handle::new(ino, content, w));
                    return Ok((fh, false));
                }
            }
        }
        Err(EIO)
    }

    pub async fn read(&self, fh: u64, offset: u64, size: u32) -> Result<Vec<u8>> {
        let (content, ahead) = {
            let mut st = self.0.st.lock();
            let h = st.handles.get_mut(&fh).ok_or(EBADF)?;
            let ahead = h.readahead(offset, size as u64);
            (h.content.clone(), ahead)
        };
        match content {
            Content::Local(file) => {
                let mut buf = vec![0u8; size as usize];
                let n = file.read_at(&mut buf, offset).map_err(io_err)?;
                buf.truncate(n);
                Ok(buf)
            }
            Content::Remote(rd) => {
                if ahead > 0 {
                    rd.prefetch(&self.0.api, offset + size as u64, ahead);
                }
                rd.read(&self.0.api, offset, size as u64)
                    .await
                    .map_err(fetch_err)
            }
        }
    }

    pub fn write(&self, fh: u64, offset: u64, data: &[u8]) -> Result<u32> {
        let (file, ino) = {
            let mut st = self.0.st.lock();
            let h = st.handles.get(&fh).ok_or(EBADF)?;
            if !h.writable {
                return Err(EBADF);
            }
            let (ino, Content::Local(file)) = (h.ino, h.content.clone()) else {
                return Err(EBADF);
            };
            let f = st.file_mut(ino)?;
            f.writes_inflight += 1;
            f.generation += 1;
            f.dirty = true;
            f.mtime = SystemTime::now();
            f.size = f.size.max(offset + data.len() as u64);
            st.bump_epoch(ino);
            (file, ino)
        };
        let result = file.write_all_at(data, offset);
        let mut st = self.0.st.lock();
        if let Ok(f) = st.file_mut(ino) {
            f.writes_inflight -= 1;
        }
        result.map_err(io_err)?;
        Ok(data.len() as u32)
    }

    pub fn release(&self, fh: u64) -> Result<()> {
        let mut st = self.0.st.lock();
        let h = st.handles.remove(&fh).ok_or(EBADF)?;
        st.close_handle(&h, &self.0.cfg, &self.0.store, self.due());
        drop(st);
        self.0.wake.notify_one();
        Ok(())
    }

    /// Uploads the file now and waits: the durability point.
    pub async fn fsync(&self, fh: u64) -> Result<()> {
        let key = {
            let mut st = self.0.st.lock();
            let ino = st.handles.get(&fh).ok_or(EBADF)?.ino;
            let Ok(f) = st.file(ino) else { return Ok(()) };
            if !f.dirty || !st.node(ino)?.attached {
                return Ok(());
            }
            let key = self.0.cfg.keys.node_key(&st.path(ino));
            let p = st.overlay.get_mut(&key).ok_or(EIO)?;
            p.force = true;
            p.due = Some(Instant::now());
            key
        };
        self.0.wake.notify_one();
        loop {
            let notified = self.0.done.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let st = self.0.st.lock();
                match st.overlay.get(&key) {
                    None => return Ok(()),
                    Some(p) if p.failed => return Err(EIO),
                    Some(p) if !matches!(p.op, PendingOp::Put(_)) => return Ok(()),
                    _ => {}
                }
            }
            notified.await;
        }
    }

    pub async fn setattr(&self, ino: Ino, fh: Option<u64>, set: SetAttr) -> Result<Attr> {
        let _ = fh;
        let changes_file = set.size.is_some() || set.mode.is_some() || set.mtime.is_some();
        {
            let st = self.0.st.lock();
            let node = st.node(ino)?;
            let is_file = matches!(node.body, Body::File(_));
            if !(is_file && changes_file) {
                drop(st);
                return self.setattr_meta(ino, &set);
            }
        }
        self.check_writable()?;
        // Changing a remote file is copy-on-write.
        for _ in 0..3 {
            let rd = {
                let st = self.0.st.lock();
                match &st.file(ino)?.content {
                    Content::Local(_) => None,
                    Content::Remote(rd) => {
                        let f = st.file(ino)?;
                        let unchanged = set.size.is_none_or(|s| s == rd.size())
                            && set.mode.is_none_or(|m| (m & 0o7777) as u16 == f.mode)
                            && set
                                .mtime
                                .is_none_or(|t| matches!(t, SetTime::At(t) if t == f.mtime));
                        if unchanged {
                            return Ok(st.attr(&self.0.cfg, ino));
                        }
                        Some(rd.clone())
                    }
                }
            };
            if let Some(rd) = rd {
                let local = if set.size == Some(0) {
                    self.0.store.create().map_err(io_err)?
                } else {
                    rd.materialize(&self.0.api).await.map_err(fetch_err)?
                };
                let mut st = self.0.st.lock();
                if !st.make_local(ino, &rd, local, &self.0.cfg) {
                    continue;
                }
            }
            let mut st = self.0.st.lock();
            let due = self.due();
            let f = st.file_mut(ino)?;
            let Content::Local(file) = f.content.clone() else {
                continue;
            };
            if let Some(size) = set.size {
                file.set_len(size).map_err(io_err)?;
                f.size = size;
                f.mtime = SystemTime::now();
            }
            if let Some(mode) = set.mode {
                f.mode = (mode & 0o7777) as u16;
            }
            if let Some(t) = set.mtime {
                f.mtime = t.resolve();
            }
            f.generation += 1;
            f.dirty = true;
            let writers = f.writers;
            if set.size.is_some() {
                st.bump_epoch(ino);
            }
            let key = self.0.cfg.keys.node_key(&st.path(ino));
            if st.node(ino)?.attached {
                st.set_op(
                    &key,
                    PendingOp::Put(ino),
                    if writers == 0 { due } else { None },
                );
            }
            let attr = st.attr(&self.0.cfg, ino);
            drop(st);
            self.0.wake.notify_one();
            return Ok(attr);
        }
        Err(EIO)
    }

    /// Metadata changes on directories and symlinks live only in this mount.
    fn setattr_meta(&self, ino: Ino, set: &SetAttr) -> Result<Attr> {
        let mut st = self.0.st.lock();
        match &mut st.node_mut(ino)?.body {
            Body::Dir(d) => {
                if let Some(mode) = set.mode {
                    d.mode = (mode & 0o7777) as u16;
                }
                if let Some(t) = set.mtime {
                    d.mtime = t.resolve();
                }
            }
            Body::Symlink(s) => {
                if let Some(t) = set.mtime {
                    s.mtime = t.resolve();
                }
            }
            Body::File(_) => {}
        }
        Ok(st.attr(&self.0.cfg, ino))
    }

    pub fn unlink(&self, parent: Ino, name: &str) -> Result<()> {
        self.check_writable()?;
        let mut st = self.0.st.lock();
        st.dir(parent)?;
        let ino = st.child(parent, name).ok_or(ENOENT)?;
        if matches!(st.node(ino)?.body, Body::Dir(_)) {
            return Err(EISDIR);
        }
        let key = self.0.cfg.keys.node_key(&st.path(ino));
        st.detach(ino);
        st.pin(parent);
        st.retire_key(&key, None, self.due());
        drop(st);
        self.0.wake.notify_one();
        Ok(())
    }

    pub fn symlink(&self, parent: Ino, name: &str, target: &str) -> Result<Attr> {
        self.check_writable()?;
        let mut st = self.0.st.lock();
        st.dir(parent)?;
        let path = self.check_name(&st, parent, name, false)?;
        if st.child(parent, name).is_some() {
            return Err(EEXIST);
        }
        if target.is_empty() {
            return Err(EINVAL);
        }
        let ino = st.alloc(
            parent,
            name,
            Body::Symlink(Symlink {
                target: Some(target.to_string()),
                remote: None,
                mtime: SystemTime::now(),
                dirty: true,
                generation: 1,
            }),
        );
        let key = self.0.cfg.keys.node_key(&path);
        st.set_op(&key, PendingOp::Put(ino), self.due());
        drop(st);
        self.0.wake.notify_one();
        let st = self.0.st.lock();
        Ok(st.attr(&self.0.cfg, ino))
    }

    pub async fn readlink(&self, ino: Ino) -> Result<String> {
        let rd = {
            let st = self.0.st.lock();
            let Body::Symlink(s) = &st.node(ino)?.body else {
                return Err(EINVAL);
            };
            if let Some(t) = &s.target {
                return Ok(t.clone());
            }
            s.remote.clone().ok_or(EIO)?
        };
        let bytes = rd
            .read(&self.0.api, 0, rd.size())
            .await
            .map_err(fetch_err)?;
        let target = String::from_utf8(bytes).map_err(|_| EIO)?;
        let mut st = self.0.st.lock();
        if let Ok(Body::Symlink(s)) = st.node_mut(ino).map(|n| &mut n.body) {
            s.target = Some(target.clone());
        }
        Ok(target)
    }

    pub async fn rename(
        &self,
        parent: Ino,
        name: &str,
        newparent: Ino,
        newname: &str,
        mode: RenameMode,
    ) -> Result<()> {
        self.check_writable()?;
        for _ in 0..3 {
            // A remote source is only renamed if its content is already local
            // (it is copied) or it is a symlink (its target is fetched).
            let need = {
                let mut st = self.0.st.lock();
                match st.rename(
                    &self.0.cfg,
                    &self.0.store,
                    parent,
                    name,
                    newparent,
                    newname,
                    mode,
                    self.due(),
                )? {
                    None => {
                        drop(st);
                        self.0.wake.notify_one();
                        return Ok(());
                    }
                    Some(need) => need,
                }
            };
            match need {
                state::RenameNeeds::Copy(ino, rd) => {
                    if !rd.fully_present() {
                        return Err(EXDEV);
                    }
                    let local = rd.materialize(&self.0.api).await.map_err(fetch_err)?;
                    self.0.st.lock().make_local(ino, &rd, local, &self.0.cfg);
                }
                state::RenameNeeds::Target(ino) => {
                    self.readlink(ino).await?;
                }
            }
        }
        Err(EXDEV)
    }

    pub fn statfs(&self) -> (u64, u64, u64) {
        let st = self.0.st.lock();
        let used = st.index.visible_bytes();
        (
            self.0.cfg.quota,
            self.0.cfg.quota.saturating_sub(used),
            st.nodes.len() as u64,
        )
    }

    /// Uploads everything that is pending and returns what happened. Called
    /// once the filesystem is unmounted.
    pub async fn drain(&self) -> Summary {
        self.0.draining.store(true, Ordering::SeqCst);
        {
            let mut st = self.0.st.lock();
            st.prepare_drain(&self.0.cfg, &self.0.store);
        }
        self.0.wake.notify_one();
        loop {
            let notified = self.0.done.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.0.st.lock().quiescent() {
                break;
            }
            self.0.wake.notify_one();
            // Also re-check periodically, in case an attempt's due time passes.
            let _ = tokio::time::timeout(Duration::from_secs(5), notified).await;
        }
        self.summary()
    }

    pub fn summary(&self) -> Summary {
        let st = self.0.st.lock();
        let s = &self.0.stats;
        Summary {
            uploaded_files: s.uploaded_files.load(Ordering::Relaxed),
            uploaded_bytes: s.uploaded_bytes.load(Ordering::Relaxed),
            whiteouts: s.whiteouts.load(Ordering::Relaxed),
            dir_markers: s.dir_markers.load(Ordering::Relaxed),
            download_requests: self.0.store.stats.requests.load(Ordering::Relaxed),
            downloaded_bytes: self.0.store.stats.bytes.load(Ordering::Relaxed),
            gc_deleted: s.gc_deleted.load(Ordering::Relaxed),
            failures: st
                .overlay
                .iter()
                .filter(|(_, p)| p.failed || p.error.is_some())
                .map(|(key, p)| Failure {
                    key: key.clone(),
                    error: p.error.clone().unwrap_or_else(|| "not uploaded".into()),
                })
                .collect(),
        }
    }

    /// Number of operations not yet committed (for tests and status).
    pub fn pending(&self) -> usize {
        self.0.st.lock().overlay.len()
    }
}

/// Readahead window bounds.
const READAHEAD_MIN: u64 = 2 * CHUNK;
const READAHEAD_MAX: u64 = 64 * CHUNK;

impl Handle {
    fn new(ino: Ino, content: Content, writable: bool) -> Handle {
        Handle {
            ino,
            content,
            writable,
            next: 0,
            window: 0,
        }
    }

    /// Records a read and returns how far past it to prefetch.
    fn readahead(&mut self, offset: u64, len: u64) -> u64 {
        let sequential = offset == self.next && offset > 0
            || (offset == 0 && self.next == 0 && self.window == 0);
        self.window = if sequential {
            (self.window * 2).clamp(READAHEAD_MIN, READAHEAD_MAX)
        } else {
            0
        };
        self.next = offset + len;
        self.window
    }
}

/// A local file's content as a `DataFile`, for tests.
pub fn _assert_send_sync() {
    fn f<T: Send + Sync>() {}
    f::<Vfs>();
    f::<Arc<DataFile>>();
    f::<Arc<RemoteData>>();
}
