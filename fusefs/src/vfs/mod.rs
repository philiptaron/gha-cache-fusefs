//! The filesystem core, independent of FUSE.
//!
//! The namespace is a tree of inodes built from the remote view (the layers,
//! stacked) plus a local overlay of operations that have not been committed
//! to the cache yet. One operation is pending per path: a `Put` of the node
//! currently there, or a `Remove` of what the view shows there. The
//! committer turns each batch of them into a layer. See DESIGN.md and
//! LAYERS.md.

mod commit;
mod snapshot;
mod state;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, watch};

use crate::api::{Api, Requests};
use crate::data::{CHUNK, DataFile, DataStore, RUN, RemoteData, RemoteFile};
use crate::entry::{self, Mark, Version, Volume, join, valid_component};
use crate::index::{self, Index, Item};

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

impl std::error::Error for Errno {}

pub type Result<T> = std::result::Result<T, Errno>;

const ENOENT: Errno = Errno(libc::ENOENT);
const EEXIST: Errno = Errno(libc::EEXIST);
const ENOTDIR: Errno = Errno(libc::ENOTDIR);
const EISDIR: Errno = Errno(libc::EISDIR);
const ENOTEMPTY: Errno = Errno(libc::ENOTEMPTY);
const EINVAL: Errno = Errno(libc::EINVAL);
const EIO: Errno = Errno(libc::EIO);
const EROFS: Errno = Errno(libc::EROFS);
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

/// What `fsync` waits for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsyncMode {
    /// Nothing: the file is on local disk once `write` returns, and is
    /// uploaded with the next batch, like any other.
    Local,
    /// The file's layer: it uploads a batch at once, and waits for it.
    Commit,
}

#[derive(Clone, Debug)]
pub struct VfsConfig {
    pub volume: Volume,
    /// The directory of the volume the mount shows ("" for all of it).
    root: String,
    pub read_only: bool,
    /// How long a closed file waits before it is uploaded.
    pub settle: Duration,
    pub fsync: FsyncMode,
    /// Write a snapshot at unmount if it would cover at least this many
    /// layers (0: never).
    pub snapshot_after: usize,
    /// How old a layer must be for a snapshot to cover it: the listing
    /// must already have shown it (LAYERS.md §8).
    pub snapshot_margin: Duration,
    /// Minimum time between refreshes triggered by lookup misses.
    pub refresh: Option<Duration>,
    pub uid: u32,
    pub gid: u32,
    pub upload_concurrency: usize,
    /// Reported as the filesystem size.
    pub quota: u64,
    /// Attempts per operation once unmounting has started.
    pub drain_attempts: u32,
}

impl VfsConfig {
    pub fn new(volume: Volume) -> VfsConfig {
        VfsConfig {
            volume,
            root: String::new(),
            read_only: false,
            settle: Duration::from_secs(1),
            fsync: FsyncMode::Local,
            snapshot_after: 16,
            snapshot_margin: Duration::from_secs(120),
            refresh: Some(Duration::from_secs(15)),
            uid: unsafe { libc::geteuid() },
            gid: unsafe { libc::getegid() },
            upload_concurrency: 8,
            quota: 10 << 30,
            drain_attempts: 5,
        }
    }

    /// Shows only the directory `root` of the volume, such as `a/b`.
    pub fn with_root(mut self, root: &str) -> std::result::Result<VfsConfig, String> {
        let root = root.trim_matches('/');
        if !root.is_empty() && !root.split('/').all(valid_component) {
            return Err(format!("{root:?} is not a path in the volume"));
        }
        self.root = root.to_string();
        Ok(self)
    }

    pub fn root(&self) -> &str {
        &self.root
    }

    /// The path in the volume of a path below the mount root.
    pub fn full(&self, path: &str) -> String {
        join(&self.root, path)
    }

    /// The path below the mount root of a path in the volume, if it is there.
    pub fn local<'a>(&self, full: &'a str) -> Option<&'a str> {
        if self.root.is_empty() {
            Some(full)
        } else if full == self.root {
            Some("")
        } else {
            full.strip_prefix(&self.root)?.strip_prefix('/')
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Failure {
    /// The path below the mount root. (Format 1 named failures by key.)
    pub key: String,
    pub error: String,
}

/// What happened during a mount, written at unmount.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Summary {
    /// Files and symlinks committed, and the bytes of the files.
    pub uploaded_files: u64,
    pub uploaded_bytes: u64,
    pub whiteouts: u64,
    /// Directory marks: `keep`, `attrs`, and `drop`.
    pub dir_markers: u64,
    /// Cache entries created: one layer per batch, one blob per large file.
    pub layers: u64,
    pub blobs: u64,
    /// Snapshots written at unmount.
    pub snapshots: u64,
    pub download_requests: u64,
    pub downloaded_bytes: u64,
    /// Blobs and covered layers kept from expiring because nothing read
    /// them (see `Vfs::touch_stale`).
    pub touched: u64,
    pub requests: Requests,
    /// Responses that asked us to slow down (429, or an exhausted REST quota).
    pub rate_limited: u64,
    /// How long those responses paused requests of their kind, in total.
    pub rate_limit_pause_ms: u64,
    pub failures: Vec<Failure>,
}

#[derive(Debug, Default)]
struct Stats {
    uploaded_files: AtomicU64,
    uploaded_bytes: AtomicU64,
    whiteouts: AtomicU64,
    dir_markers: AtomicU64,
    layers: AtomicU64,
    blobs: AtomicU64,
    snapshots: AtomicU64,
    touched: AtomicU64,
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
    /// The refresh under way, if any; it resolves to whether it worked.
    refreshing: Mutex<Option<watch::Receiver<Option<bool>>>>,
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
        let mut index = Index::new(cfg.volume.clone(), scopes);
        let prefix = cfg.volume.prefix();
        let items = index::list_all(&api.rest, prefix, index.scopes()).await?;
        let sorted = sort_items(&index, &items);
        let (n_layers, n_blobs) = (sorted.layers.len(), sorted.blobs.len());
        for (entry, sha) in sorted.blobs {
            index.insert_blob(entry, sha);
        }
        let snapshots = index::read_snapshots(&api, sorted.snapshots).await?;
        let n_snapshots = snapshots.len();
        for snapshot in snapshots {
            index.insert_layer(snapshot);
        }
        let layers = take_covered(&mut index, sorted.layers);
        let n_read = layers.len();
        for layer in index::read_layers(&api, layers).await? {
            index.insert_layer(layer);
        }
        index.restack();
        tracing::info!(
            "listed {} entries under {prefix:?} in {:?} ({n_layers} layers, {n_read} of them read, {n_snapshots} snapshots read, {n_blobs} blobs, {} ignored)",
            items.len(),
            index.scopes(),
            sorted.foreign
        );
        let vfs = Vfs::with_index(cfg, api, store, index);
        vfs.touch_stale();
        Ok(vfs)
    }

    /// The service expires entries a week after their last download. A
    /// mount reads the metadata of every layer, which keeps layers alive,
    /// but a blob is only downloaded when its file is read. So a mount
    /// resolves a download URL, which counts as use, for the blobs of visible
    /// files that have not been used for a while (LAYERS.md §7).
    fn touch_stale(&self) {
        let stale = {
            let st = self.0.st.lock();
            let cutoff = SystemTime::now()
                .checked_sub(TOUCH_AFTER)
                .unwrap_or(SystemTime::UNIX_EPOCH);
            st.index.stale_entries(cutoff, TOUCH_MAX)
        };
        if stale.is_empty() {
            return;
        }
        tracing::debug!("touching {} blobs and layers nothing read", stale.len());
        let vfs = self.clone();
        tokio::spawn(async move {
            use futures_util::StreamExt;
            futures_util::stream::iter(stale)
                .for_each_concurrent(TOUCH_CONCURRENCY, |e| {
                    let vfs = vfs.clone();
                    async move {
                        match vfs.0.api.twirp.download_url(&e.key, &e.version).await {
                            Ok(Some(_)) => {
                                vfs.0.stats.touched.fetch_add(1, Ordering::Relaxed);
                            }
                            Ok(None) => tracing::debug!("{}: gone before it was touched", e.key),
                            Err(err) => tracing::debug!("touching {}: {err}", e.key),
                        }
                    }
                })
                .await;
        });
    }

    pub fn with_index(cfg: VfsConfig, api: Api, store: Arc<DataStore>, index: Index) -> Vfs {
        let mut st = State::new(index);
        let paths: Vec<String> = st
            .index
            .paths()
            .filter_map(|p| cfg.local(p))
            .map(str::to_string)
            .collect();
        for path in paths {
            st.apply_remote(&cfg, &store, &path);
        }
        let vfs = Vfs(Arc::new(Inner {
            cfg,
            api,
            store,
            st: Mutex::new(st),
            wake: Notify::new(),
            done: Notify::new(),
            refreshing: Mutex::new(None),
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

    fn check_name(&self, st: &State, parent: Ino, name: &str) -> Result<String> {
        if name.len() > entry::NAME_MAX {
            return Err(ENAMETOOLONG);
        }
        if !valid_component(name) {
            return Err(EINVAL);
        }
        Ok(join(&st.path(parent), name))
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

    /// Refreshes if the last refresh is older than the configured interval,
    /// and returns whether a refresh finished. One refresh runs at a time, in
    /// the background: a lookup waits for it at most `REFRESH_WAIT`, and a
    /// slow listing only delays what it finds.
    async fn maybe_refresh(&self) -> bool {
        let Some(interval) = self.0.cfg.refresh else {
            return false;
        };
        let mut done = {
            let mut refreshing = self.0.refreshing.lock();
            match &*refreshing {
                Some(rx) => rx.clone(),
                None => {
                    if self.0.st.lock().last_refresh.elapsed() < interval {
                        return false;
                    }
                    let (tx, rx) = watch::channel(None);
                    *refreshing = Some(rx.clone());
                    let vfs = self.clone();
                    tokio::spawn(async move {
                        let ok = match vfs.refresh().await {
                            Ok(()) => true,
                            Err(e) => {
                                tracing::warn!("refresh failed: {e:#}");
                                false
                            }
                        };
                        *vfs.0.refreshing.lock() = None;
                        let _ = tx.send(Some(ok));
                    });
                    rx
                }
            }
        };
        let finished = tokio::time::timeout(REFRESH_WAIT, done.wait_for(Option::is_some)).await;
        matches!(finished, Ok(Ok(ok)) if *ok == Some(true))
    }

    /// Picks up entries created since the last listing.
    pub async fn refresh(&self) -> anyhow::Result<()> {
        let (scopes, marks) = {
            let st = self.0.st.lock();
            (st.index.scopes().to_vec(), st.index.watermarks())
        };
        let prefix = self.0.cfg.volume.prefix();
        let lists = futures_util::future::try_join_all(
            scopes
                .iter()
                .zip(marks)
                .map(|(scope, mark)| index::list_since(&self.0.api.rest, prefix, scope, mark)),
        )
        .await;
        // Snapshots change nothing the mount shows, so a refresh ignores
        // them; a layer the mount's snapshot covers is not read.
        let result = async {
            let items: Vec<_> = lists?.into_iter().flatten().collect();
            let sorted = sort_items(&self.0.st.lock().index, &items);
            let layers = take_covered(&mut self.0.st.lock().index, sorted.layers);
            let layers = index::read_layers(&self.0.api, layers).await?;
            anyhow::Ok((layers, sorted.blobs))
        }
        .await;
        let mut st = self.0.st.lock();
        st.last_refresh = Instant::now();
        let (layers, blobs) = result?;
        if layers.is_empty() && blobs.is_empty() {
            return Ok(());
        }
        for (entry, sha) in blobs {
            st.index.insert_blob(entry, sha);
        }
        for layer in layers {
            st.index.insert_layer(layer);
        }
        let changed = st.index.restack();
        tracing::debug!("refresh: {} paths changed", changed.len());
        self.apply_changes(&mut st, &changed);
        Ok(())
    }

    /// Makes the tree agree with the view at paths of the volume that changed.
    fn apply_changes(&self, st: &mut State, changed: &[String]) {
        for full in changed {
            if let Some(path) = self.0.cfg.local(full) {
                st.apply_remote(&self.0.cfg, &self.0.store, path);
            }
        }
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
        let path = self.check_name(&st, parent, name)?;
        if st.child(parent, name).is_some() {
            return Err(EEXIST);
        }
        let kept = st.index.kept(&self.0.cfg.full(&path));
        let ino = st.alloc(
            parent,
            name,
            Body::Dir(Dir {
                children: BTreeMap::new(),
                kept,
                pinned: true,
                mark: Some(Mark::Keep),
                generation: 0,
                mode: (mode & 0o7777) as u16,
                mtime: SystemTime::now(),
            }),
        );
        // This replaces whatever was pending here, such as an `rmdir`.
        st.set_op(&path, PendingOp::Put(ino), self.due());
        let attr = st.attr(&self.0.cfg, ino);
        drop(st);
        self.0.wake.notify_one();
        Ok(attr)
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
        let path = st.path(ino);
        st.detach(ino);
        st.pin(parent);
        st.retire(&self.0.cfg, &path, None, self.due());
        drop(st);
        self.0.wake.notify_one();
        Ok(())
    }

    // ---- files ------------------------------------------------------------

    pub fn create(&self, parent: Ino, name: &str, mode: u32, flags: i32) -> Result<(Attr, u64)> {
        self.check_writable()?;
        let mut st = self.0.st.lock();
        st.dir(parent)?;
        let path = self.check_name(&st, parent, name)?;
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
        st.set_op(&path, PendingOp::Put(ino), None);
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
            let rf = {
                let mut st = self.0.st.lock();
                if let Some((content, w)) = st.open_local(ino, flags, &self.0.cfg, &self.0.store)? {
                    let keep = st.keep_cache(ino, &content, w);
                    let fh = st.add_handle(Handle::new(ino, content, w));
                    return Ok((fh, keep));
                }
                // Copy-on-write of a remote file: fetch it all first.
                match &st.file(ino)?.content {
                    Content::Remote(rf) => rf.clone(),
                    Content::Local(_) => continue,
                }
            };
            let local = rf
                .materialize(&self.0.api, &self.0.store)
                .await
                .map_err(fetch_err)?;
            let mut st = self.0.st.lock();
            if st.make_local(ino, &rf, local) {
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
            let first_read = !std::mem::replace(&mut h.has_read, true);
            let (ino, content) = (h.ino, h.content.clone());
            if first_read && matches!(content, Content::Remote(_)) {
                self.prefetch_siblings(&mut st, ino);
            }
            (content, ahead)
        };
        match content {
            Content::Local(file) => {
                let mut buf = vec![0u8; size as usize];
                let n = file.read_at(&mut buf, offset).map_err(io_err)?;
                buf.truncate(n);
                Ok(buf)
            }
            Content::Remote(rf) => {
                if ahead > 0 {
                    // What this read needs goes first, in a request of its
                    // own. Then the window is topped up to a whole run:
                    // sliding it one read at a time would fetch a chunk per
                    // request.
                    let from = offset + size as u64;
                    let mut to = from + ahead;
                    if to < rf.size() && to / RUN * RUN > from {
                        to = to / RUN * RUN;
                    }
                    rf.prefetch(&self.0.api, offset, size as u64);
                    rf.prefetch(&self.0.api, from, to - from);
                }
                rf.read(&self.0.api, offset, size as u64)
                    .await
                    .map_err(fetch_err)
            }
        }
    }

    /// Reading one small remote file suggests the rest of its directory is
    /// next (`cp -r`, `diff -r`, `tar c`): fetch the small siblings in the
    /// background, so that reading them costs local I/O instead of two
    /// ~250 ms round trips each. A layer holds its files in path order, so
    /// a directory's small files are usually one range of it, fetched in a
    /// few large requests. Once per directory.
    fn prefetch_siblings(&self, st: &mut State, ino: Ino) {
        let Some(parent) = st.nodes.get(&ino).map(|n| n.parent) else {
            return;
        };
        if !st.prefetched_dirs.insert(parent) {
            return;
        }
        let siblings = st.small_remote_files(parent, PREFETCH_MAX_SIZE, PREFETCH_MAX_FILES);
        if siblings.len() < 2 {
            return;
        }
        // Per entry, the range the files cover.
        struct Span {
            rd: Arc<RemoteData>,
            start: u64,
            end: u64,
            files: Vec<Arc<RemoteFile>>,
        }
        let mut spans: BTreeMap<i64, Span> = BTreeMap::new();
        for rf in siblings {
            let Some((rd, offset)) = rf.range_of() else {
                continue;
            };
            let end = offset + rf.size();
            let span = spans.entry(rd.entry.id).or_insert_with(|| Span {
                rd: rd.clone(),
                start: offset,
                end,
                files: Vec::new(),
            });
            span.start = span.start.min(offset);
            span.end = span.end.max(end);
            span.files.push(rf.clone());
        }
        let api = self.0.api.clone();
        for Span {
            rd,
            start,
            end,
            files,
        } in spans.into_values()
        {
            if end - start <= PREFETCH_MAX_SPAN {
                rd.prefetch(&api, start, end - start);
                continue;
            }
            let api = api.clone();
            tokio::spawn(async move {
                use futures_util::StreamExt;
                futures_util::stream::iter(files)
                    .for_each_concurrent(PREFETCH_CONCURRENCY, |rf| {
                        let api = api.clone();
                        async move {
                            if let Err(e) = rf.ensure(&api, 0, rf.size()).await {
                                tracing::debug!("prefetch: {e}");
                            }
                        }
                    })
                    .await;
            });
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
            if result.is_err() && f.writes_inflight == 0 {
                // The size grew in advance; take it from the file instead,
                // so that it never claims bytes the file does not have.
                if let (Content::Local(cur), Ok(len)) = (&f.content, file.size()) {
                    if Arc::ptr_eq(cur, &file) {
                        f.size = len;
                        f.generation += 1;
                    }
                }
            }
        }
        result.map_err(io_err)?;
        Ok(data.len() as u32)
    }

    pub fn release(&self, fh: u64) -> Result<()> {
        let mut st = self.0.st.lock();
        let h = st.handles.remove(&fh).ok_or(EBADF)?;
        st.close_handle(&h, self.due());
        drop(st);
        self.0.wake.notify_one();
        Ok(())
    }

    /// With `FsyncMode::Commit`, uploads the file now and waits.
    pub async fn fsync(&self, fh: u64) -> Result<()> {
        let path = {
            let mut st = self.0.st.lock();
            let ino = st.handles.get(&fh).ok_or(EBADF)?.ino;
            if self.0.cfg.fsync == FsyncMode::Local {
                return Ok(());
            }
            let Ok(f) = st.file(ino) else { return Ok(()) };
            if !f.dirty || !st.node(ino)?.attached {
                return Ok(());
            }
            let path = st.path(ino);
            st.overlay.get_mut(&path).ok_or(EIO)?.force = true;
            st.schedule(&path, Some(Instant::now()));
            path
        };
        self.0.wake.notify_one();
        loop {
            let notified = self.0.done.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let st = self.0.st.lock();
                match st.overlay.get(&path) {
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
        // Changing a remote file's attributes needs no data: the new layer
        // refers to the data it has. Changing its size is copy-on-write.
        if set.size.is_none() {
            let mut st = self.0.st.lock();
            if let Content::Remote(_) = st.file(ino)?.content {
                let f = st.file(ino)?;
                let unchanged = set.mode.is_none_or(|m| (m & 0o7777) as u16 == f.mode)
                    && set
                        .mtime
                        .is_none_or(|t| matches!(t, SetTime::At(t) if t == f.mtime));
                if unchanged {
                    return Ok(st.attr(&self.0.cfg, ino));
                }
                let due = self.due();
                let f = st.file_mut(ino)?;
                if let Some(mode) = set.mode {
                    f.mode = (mode & 0o7777) as u16;
                }
                if let Some(t) = set.mtime {
                    f.mtime = t.resolve();
                }
                f.generation += 1;
                f.dirty = true;
                let path = st.path(ino);
                if st.node(ino)?.attached {
                    st.set_op(&path, PendingOp::Put(ino), due);
                }
                let attr = st.attr(&self.0.cfg, ino);
                drop(st);
                self.0.wake.notify_one();
                return Ok(attr);
            }
        }
        for _ in 0..3 {
            let rf = {
                let st = self.0.st.lock();
                match &st.file(ino)?.content {
                    Content::Local(_) => None,
                    Content::Remote(rf) => {
                        let f = st.file(ino)?;
                        let unchanged = set.size.is_none_or(|s| s == rf.size())
                            && set.mode.is_none_or(|m| (m & 0o7777) as u16 == f.mode)
                            && set
                                .mtime
                                .is_none_or(|t| matches!(t, SetTime::At(t) if t == f.mtime));
                        if unchanged {
                            return Ok(st.attr(&self.0.cfg, ino));
                        }
                        Some(rf.clone())
                    }
                }
            };
            if let Some(rf) = rf {
                let local = if set.size == Some(0) {
                    self.0.store.create().map_err(io_err)?
                } else {
                    rf.materialize(&self.0.api, &self.0.store)
                        .await
                        .map_err(fetch_err)?
                };
                let mut st = self.0.st.lock();
                if !st.make_local(ino, &rf, local) {
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
            let path = st.path(ino);
            if st.node(ino)?.attached {
                st.set_op(
                    &path,
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

    /// `chmod` and `utimens` of directories (an `attrs` mark) and symlinks.
    fn setattr_meta(&self, ino: Ino, set: &SetAttr) -> Result<Attr> {
        let mut st = self.0.st.lock();
        st.node(ino)?;
        if set.mode.is_none() && set.mtime.is_none() {
            return Ok(st.attr(&self.0.cfg, ino));
        }
        self.check_writable()?;
        let path = st.path(ino);
        let attached = st.node(ino)?.attached;
        match &mut st.node_mut(ino)?.body {
            Body::Dir(d) => {
                if let Some(mode) = set.mode {
                    d.mode = (mode & 0o7777) as u16;
                }
                if let Some(t) = set.mtime {
                    d.mtime = t.resolve();
                }
                d.mark.get_or_insert(Mark::Attrs);
                d.generation += 1;
            }
            Body::Symlink(s) => {
                if let Some(t) = set.mtime {
                    s.mtime = t.resolve();
                }
                s.dirty = true;
                s.generation += 1;
            }
            Body::File(_) => return Ok(st.attr(&self.0.cfg, ino)),
        }
        if attached {
            st.set_op(&path, PendingOp::Put(ino), self.due());
        }
        let attr = st.attr(&self.0.cfg, ino);
        drop(st);
        self.0.wake.notify_one();
        Ok(attr)
    }

    pub fn unlink(&self, parent: Ino, name: &str) -> Result<()> {
        self.check_writable()?;
        let mut st = self.0.st.lock();
        st.dir(parent)?;
        let ino = st.child(parent, name).ok_or(ENOENT)?;
        if matches!(st.node(ino)?.body, Body::Dir(_)) {
            return Err(EISDIR);
        }
        let path = st.path(ino);
        st.detach(ino);
        st.pin(parent);
        st.retire(&self.0.cfg, &path, None, self.due());
        drop(st);
        self.0.wake.notify_one();
        Ok(())
    }

    pub fn symlink(&self, parent: Ino, name: &str, target: &str) -> Result<Attr> {
        self.check_writable()?;
        let mut st = self.0.st.lock();
        st.dir(parent)?;
        let path = self.check_name(&st, parent, name)?;
        if st.child(parent, name).is_some() {
            return Err(EEXIST);
        }
        // EROFS keeps a symlink's target in one block.
        if target.is_empty() || target.len() > 4096 {
            return Err(EINVAL);
        }
        let ino = st.alloc(
            parent,
            name,
            Body::Symlink(Symlink {
                target: target.to_string(),
                id: None,
                mtime: SystemTime::now(),
                dirty: true,
                generation: 1,
            }),
        );
        st.set_op(&path, PendingOp::Put(ino), self.due());
        let attr = st.attr(&self.0.cfg, ino);
        drop(st);
        self.0.wake.notify_one();
        Ok(attr)
    }

    /// Targets arrive with the layers' metadata, so this never waits.
    pub async fn readlink(&self, ino: Ino) -> Result<String> {
        let st = self.0.st.lock();
        match &st.node(ino)?.body {
            Body::Symlink(s) => Ok(s.target.clone()),
            _ => Err(EINVAL),
        }
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
        let due = self.due();
        self.0
            .st
            .lock()
            .rename(&self.0.cfg, parent, name, newparent, newname, mode, due)?;
        self.0.wake.notify_one();
        Ok(())
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
            st.prepare_drain();
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
            // Each check walks every pending op, and uploads finish every few
            // milliseconds; checking after each one would be quadratic.
            tokio::time::sleep(DRAIN_CHECK).await;
        }
        // A snapshot is an optimization: failing to write one fails nothing.
        if let Err(e) = self.snapshot().await {
            tracing::warn!("writing a snapshot: {e:#}");
        }
        self.summary()
    }

    pub fn summary(&self) -> Summary {
        let st = self.0.st.lock();
        let s = &self.0.stats;
        let api = &self.0.api.stats;
        Summary {
            uploaded_files: s.uploaded_files.load(Ordering::Relaxed),
            uploaded_bytes: s.uploaded_bytes.load(Ordering::Relaxed),
            whiteouts: s.whiteouts.load(Ordering::Relaxed),
            dir_markers: s.dir_markers.load(Ordering::Relaxed),
            layers: s.layers.load(Ordering::Relaxed),
            snapshots: s.snapshots.load(Ordering::Relaxed),
            blobs: s.blobs.load(Ordering::Relaxed),
            download_requests: self.0.store.stats.requests.load(Ordering::Relaxed),
            downloaded_bytes: self.0.store.stats.bytes.load(Ordering::Relaxed),
            touched: s.touched.load(Ordering::Relaxed),
            requests: api.requests(),
            rate_limited: api.rate_limited.load(Ordering::Relaxed),
            rate_limit_pause_ms: api.paused_ms.load(Ordering::Relaxed),
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

/// Sibling prefetch: files up to this size, this many per directory, this
/// many at once (modest, so speculative reads do not provoke rate limits).
const PREFETCH_MAX_SIZE: u64 = CHUNK;
const PREFETCH_MAX_FILES: usize = 256;
const PREFETCH_CONCURRENCY: usize = 6;
/// A directory's small files in one layer are fetched as one range when it
/// is at most this long.
const PREFETCH_MAX_SPAN: u64 = 32 * CHUNK;

/// How long a lookup of a missing name waits for the refresh it starts.
const REFRESH_WAIT: Duration = Duration::from_secs(2);

/// How often an unmount checks whether everything is done.
const DRAIN_CHECK: Duration = Duration::from_millis(20);

/// Touching blobs nothing read: those last used this long ago, at most this
/// many per mount (the stalest first), this many at once.
const TOUCH_AFTER: Duration = Duration::from_secs(3 * 24 * 3600);
const TOUCH_MAX: usize = 1000;
const TOUCH_CONCURRENCY: usize = 4;

/// Listed items that are new: layers, whose metadata must be read, and
/// blobs; and how many are not ours.
#[allow(clippy::type_complexity)]
/// New entries of a listing, by kind.
#[derive(Default)]
struct Sorted {
    layers: Vec<(index::Listed, u32)>,
    blobs: Vec<(index::Listed, String)>,
    snapshots: Vec<(index::Listed, u32, SystemTime)>,
    /// Entries this filesystem did not write.
    foreign: usize,
}

fn sort_items(index: &Index, items: &[crate::api::CacheItem]) -> Sorted {
    let mut out = Sorted::default();
    let mut new = std::collections::HashSet::new();
    for item in items {
        match index.classify(item) {
            Some(_) if !new.insert(item.id) => {}
            Some(Item::Layer { entry, meta_blocks }) => out.layers.push((entry, meta_blocks)),
            Some(Item::Blob { entry, sha }) => out.blobs.push((entry, sha)),
            Some(Item::Snapshot {
                entry,
                meta_blocks,
                covers,
            }) => out.snapshots.push((entry, meta_blocks, covers)),
            None if Version::decode(&item.version).is_none() => out.foreign += 1,
            None => {}
        }
    }
    out
}

/// Records the layers a snapshot covers without reading them, and returns
/// the rest, whose metadata must be read.
fn take_covered(index: &mut Index, layers: Vec<(index::Listed, u32)>) -> Vec<(index::Listed, u32)> {
    let (covered, rest): (Vec<_>, Vec<_>) =
        layers.into_iter().partition(|(e, _)| index.is_covered(e));
    for (entry, meta_blocks) in covered {
        index.insert_layer(index::Layer::covered(entry, meta_blocks));
    }
    rest
}

impl Handle {
    fn new(ino: Ino, content: Content, writable: bool) -> Handle {
        Handle {
            ino,
            content,
            writable,
            next: 0,
            window: 0,
            has_read: false,
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
    f::<Arc<RemoteFile>>();
}
