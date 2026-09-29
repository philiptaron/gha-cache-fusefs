//! The mutable state behind the filesystem's lock: inodes, handles, the remote
//! index, and the overlay of pending operations.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use super::{
    Attr, DirEntry, EEXIST, EINVAL, EISDIR, ENAMETOOLONG, ENOENT, ENOTDIR, ENOTEMPTY, EXDEV, Errno,
    FileKind, Ino, ROOT, RenameMode, Result, VfsConfig, io_err,
};
use crate::data::{DataFile, DataStore, RemoteData};
use crate::entry::{KeyPath, Kind, MAX_KEY_LEN, join, split, valid_component};
use crate::index::{Index, RemoteEntry};

#[derive(Clone, Debug)]
pub(super) enum Content {
    /// Authoritative local data (pending, or not yet evictable).
    Local(Arc<DataFile>),
    /// A remote entry and its local cache.
    Remote(Arc<RemoteData>),
}

#[derive(Debug)]
pub(super) struct Dir {
    pub children: BTreeMap<String, Ino>,
    /// The visible remote marker entry, if any.
    pub marker: Option<RemoteEntry>,
    /// Exists even without children or a marker (created or emptied locally).
    pub pinned: bool,
    pub mode: u16,
    pub mtime: SystemTime,
}

#[derive(Debug)]
pub(super) struct File {
    pub content: Content,
    /// Size of local content (remote content knows its own size).
    pub size: u64,
    pub mode: u16,
    pub mtime: SystemTime,
    pub writers: u32,
    pub writes_inflight: u32,
    /// Bumped by every change; a commit is valid only if it did not move.
    pub generation: u64,
    pub dirty: bool,
    /// The entry that holds this local content, once committed.
    pub committed: Option<RemoteEntry>,
}

#[derive(Debug)]
pub(super) struct Symlink {
    pub target: Option<String>,
    pub remote: Option<Arc<RemoteData>>,
    pub mtime: SystemTime,
    pub dirty: bool,
    pub generation: u64,
}

#[derive(Debug)]
pub(super) enum Body {
    Dir(Dir),
    File(File),
    Symlink(Symlink),
}

#[derive(Debug)]
pub(super) struct Node {
    pub parent: Ino,
    pub name: String,
    pub attached: bool,
    pub open: u32,
    /// Bumped when content changes, to decide `FOPEN_KEEP_CACHE`.
    pub epoch: u64,
    pub last_open_epoch: Option<u64>,
    pub body: Body,
}

#[derive(Debug)]
pub(super) struct Handle {
    pub ino: Ino,
    pub content: Content,
    pub writable: bool,
    /// Where a sequential reader would read next.
    pub next: u64,
    pub window: u64,
    /// Whether this handle has read anything yet.
    pub has_read: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum PendingOp {
    /// Upload the node currently at this key.
    Put(Ino),
    /// Hide this key with a whiteout, once `after` has no pending `Put`.
    Delete { after: Option<String> },
}

#[derive(Debug)]
pub(super) struct Pending {
    pub op: PendingOp,
    pub op_id: u64,
    /// When the op becomes eligible; `None` while the file is open for
    /// writing. Set it with `State::schedule`, which also queues the op.
    pub due: Option<Instant>,
    /// Eligible even while the file is open for writing (fsync, unmount).
    pub force: bool,
    pub inflight: Option<u64>,
    pub attempts: u32,
    pub error: Option<String>,
    pub failed: bool,
}

pub(super) enum RenameNeeds {
    /// The source's remote content must be made local first.
    Copy(Ino, Arc<RemoteData>),
    /// The source symlink's target must be fetched first.
    Target(Ino),
}

pub(super) struct State {
    pub nodes: HashMap<Ino, Node>,
    next_ino: Ino,
    pub handles: HashMap<u64, Handle>,
    pub dir_handles: HashMap<u64, Vec<DirEntry>>,
    next_fh: u64,
    next_op: u64,
    pub index: Index,
    pub overlay: BTreeMap<String, Pending>,
    /// Overlay ops by due time, earliest first, so that the committer looks
    /// only at what is due. An entry lapses when its op is rescheduled,
    /// starts, fails, or goes away; `queued` tells.
    pub queue: BinaryHeap<Reverse<(Instant, String)>>,
    pub last_refresh: Instant,
    /// Directories whose small files were already prefetched.
    pub prefetched_dirs: HashSet<Ino>,
}

impl State {
    pub fn new(index: Index) -> State {
        let mut nodes = HashMap::new();
        nodes.insert(
            ROOT,
            Node {
                parent: ROOT,
                name: String::new(),
                attached: true,
                open: 0,
                epoch: 0,
                last_open_epoch: None,
                body: Body::Dir(Dir {
                    children: BTreeMap::new(),
                    marker: None,
                    pinned: true,
                    mode: 0o755,
                    mtime: SystemTime::now(),
                }),
            },
        );
        State {
            nodes,
            next_ino: ROOT + 1,
            handles: HashMap::new(),
            dir_handles: HashMap::new(),
            next_fh: 1,
            next_op: 1,
            index,
            overlay: BTreeMap::new(),
            queue: BinaryHeap::new(),
            last_refresh: Instant::now(),
            prefetched_dirs: HashSet::new(),
        }
    }

    /// Remote files in `dir` small enough to fetch speculatively.
    pub fn small_remote_files(
        &self,
        dir: Ino,
        max_size: u64,
        limit: usize,
    ) -> Vec<Arc<RemoteData>> {
        let Ok(d) = self.dir(dir) else {
            return Vec::new();
        };
        d.children
            .values()
            .filter_map(|c| match self.nodes.get(c).map(|n| &n.body) {
                Some(Body::File(File {
                    content: Content::Remote(rd),
                    ..
                })) if (1..=max_size).contains(&rd.size()) => Some(rd.clone()),
                _ => None,
            })
            .take(limit)
            .collect()
    }

    // ---- accessors --------------------------------------------------------

    pub fn node(&self, ino: Ino) -> Result<&Node> {
        self.nodes.get(&ino).ok_or(ENOENT)
    }

    pub fn node_mut(&mut self, ino: Ino) -> Result<&mut Node> {
        self.nodes.get_mut(&ino).ok_or(ENOENT)
    }

    pub fn dir(&self, ino: Ino) -> Result<&Dir> {
        match &self.node(ino)?.body {
            Body::Dir(d) => Ok(d),
            _ => Err(ENOTDIR),
        }
    }

    fn dir_mut(&mut self, ino: Ino) -> Result<&mut Dir> {
        match &mut self.node_mut(ino)?.body {
            Body::Dir(d) => Ok(d),
            _ => Err(ENOTDIR),
        }
    }

    pub fn file(&self, ino: Ino) -> Result<&File> {
        match &self.node(ino)?.body {
            Body::File(f) => Ok(f),
            Body::Dir(_) => Err(EISDIR),
            Body::Symlink(_) => Err(Errno(libc::ELOOP)),
        }
    }

    pub fn file_mut(&mut self, ino: Ino) -> Result<&mut File> {
        match &mut self.node_mut(ino)?.body {
            Body::File(f) => Ok(f),
            Body::Dir(_) => Err(EISDIR),
            Body::Symlink(_) => Err(Errno(libc::ELOOP)),
        }
    }

    pub fn child(&self, parent: Ino, name: &str) -> Option<Ino> {
        self.dir(parent).ok()?.children.get(name).copied()
    }

    /// The path of a node relative to the mount root ("" for the root).
    pub fn path(&self, mut ino: Ino) -> String {
        let mut parts = Vec::new();
        while ino != ROOT {
            let Some(node) = self.nodes.get(&ino) else {
                break;
            };
            parts.push(node.name.as_str());
            ino = node.parent;
        }
        parts.reverse();
        parts.join("/")
    }

    fn resolve(&self, path: &str) -> Option<Ino> {
        let mut ino = ROOT;
        if path.is_empty() {
            return Some(ino);
        }
        for part in path.split('/') {
            ino = self.child(ino, part)?;
        }
        Some(ino)
    }

    fn is_ancestor(&self, ancestor: Ino, mut ino: Ino) -> bool {
        loop {
            if ino == ancestor {
                return true;
            }
            if ino == ROOT {
                return false;
            }
            match self.nodes.get(&ino) {
                Some(n) => ino = n.parent,
                None => return false,
            }
        }
    }

    pub fn attr(&self, cfg: &VfsConfig, ino: Ino) -> Attr {
        let Some(node) = self.nodes.get(&ino) else {
            return Attr {
                ino,
                kind: FileKind::File,
                size: 0,
                mtime: SystemTime::UNIX_EPOCH,
                perm: 0,
                nlink: 0,
                uid: cfg.uid,
                gid: cfg.gid,
            };
        };
        let (kind, size, mtime, perm, nlink) = match &node.body {
            Body::Dir(d) => {
                let subdirs = d
                    .children
                    .values()
                    .filter(|c| matches!(self.nodes.get(c).map(|n| &n.body), Some(Body::Dir(_))))
                    .count();
                (FileKind::Dir, 4096, d.mtime, d.mode, 2 + subdirs as u32)
            }
            Body::File(f) => {
                let size = match &f.content {
                    Content::Local(_) => f.size,
                    Content::Remote(rd) => rd.size(),
                };
                (FileKind::File, size, f.mtime, f.mode, 1)
            }
            Body::Symlink(s) => {
                let size = match (&s.target, &s.remote) {
                    (Some(t), _) => t.len() as u64,
                    (None, Some(rd)) => rd.size(),
                    (None, None) => 0,
                };
                (FileKind::Symlink, size, s.mtime, 0o777, 1)
            }
        };
        Attr {
            ino,
            kind,
            size,
            mtime,
            perm,
            nlink: if node.attached { nlink } else { 0 },
            uid: cfg.uid,
            gid: cfg.gid,
        }
    }

    pub fn dir_snapshot(&self, cfg: &VfsConfig, ino: Ino) -> Result<Vec<DirEntry>> {
        let dir = self.dir(ino)?;
        let parent = self.node(ino)?.parent;
        let mut out = Vec::with_capacity(dir.children.len() + 2);
        out.push(DirEntry {
            ino,
            kind: FileKind::Dir,
            name: ".".into(),
            attr: self.attr(cfg, ino),
        });
        out.push(DirEntry {
            ino: parent,
            kind: FileKind::Dir,
            name: "..".into(),
            attr: self.attr(cfg, parent),
        });
        for (name, &child) in &dir.children {
            let attr = self.attr(cfg, child);
            out.push(DirEntry {
                ino: child,
                kind: attr.kind,
                name: name.clone(),
                attr,
            });
        }
        Ok(out)
    }

    // ---- structure --------------------------------------------------------

    pub fn alloc_fh(&mut self) -> u64 {
        let fh = self.next_fh;
        self.next_fh += 1;
        fh
    }

    pub fn alloc(&mut self, parent: Ino, name: &str, body: Body) -> Ino {
        let ino = self.next_ino;
        self.next_ino += 1;
        self.nodes.insert(
            ino,
            Node {
                parent,
                name: name.to_string(),
                attached: true,
                open: 0,
                epoch: 0,
                last_open_epoch: None,
                body,
            },
        );
        if let Ok(d) = self.dir_mut(parent) {
            d.children.insert(name.to_string(), ino);
            d.mtime = SystemTime::now();
        }
        ino
    }

    /// Unlinks a node (and, for directories, everything below it). Open
    /// nodes stay alive until their last handle is released.
    pub fn detach(&mut self, ino: Ino) {
        let Some(node) = self.nodes.get_mut(&ino) else {
            return;
        };
        node.attached = false;
        let (parent, name) = (node.parent, node.name.clone());
        let children: Vec<Ino> = match &node.body {
            Body::Dir(d) => d.children.values().copied().collect(),
            _ => Vec::new(),
        };
        if let Ok(d) = self.dir_mut(parent) {
            if d.children.get(&name) == Some(&ino) {
                d.children.remove(&name);
                d.mtime = SystemTime::now();
            }
        }
        for c in children {
            self.detach(c);
        }
        if self.nodes.get(&ino).is_some_and(|n| n.open == 0) {
            self.nodes.remove(&ino);
        }
    }

    fn move_node(&mut self, ino: Ino, newparent: Ino, newname: &str) {
        let Some(node) = self.nodes.get_mut(&ino) else {
            return;
        };
        let (parent, name) = (
            node.parent,
            std::mem::replace(&mut node.name, newname.to_string()),
        );
        node.parent = newparent;
        if let Ok(d) = self.dir_mut(parent) {
            d.children.remove(&name);
            d.mtime = SystemTime::now();
        }
        if let Ok(d) = self.dir_mut(newparent) {
            d.children.insert(newname.to_string(), ino);
            d.mtime = SystemTime::now();
        }
    }

    /// POSIX directories outlive their last entry.
    pub fn pin(&mut self, ino: Ino) {
        if let Ok(d) = self.dir_mut(ino) {
            d.pinned = true;
        }
    }

    pub fn bump_epoch(&mut self, ino: Ino) {
        if let Some(n) = self.nodes.get_mut(&ino) {
            n.epoch += 1;
        }
    }

    /// Removes directories that only existed because of remote children.
    fn prune(&mut self, mut ino: Ino) {
        while ino != ROOT {
            let Some(node) = self.nodes.get(&ino) else {
                return;
            };
            let parent = node.parent;
            match &node.body {
                Body::Dir(d) if !d.pinned && d.marker.is_none() && d.children.is_empty() => {
                    self.detach(ino);
                    ino = parent;
                }
                _ => return,
            }
        }
    }

    // ---- the overlay ------------------------------------------------------

    pub fn set_op(&mut self, key: &str, op: PendingOp, due: Option<Instant>) {
        let op_id = self.next_op;
        self.next_op += 1;
        let p = self
            .overlay
            .entry(key.to_string())
            .or_insert_with(|| Pending {
                op: op.clone(),
                op_id,
                due: None,
                force: false,
                inflight: None,
                attempts: 0,
                error: None,
                failed: false,
            });
        if p.op != op {
            p.op = op;
            p.op_id = op_id;
        }
        p.force = false;
        p.attempts = 0;
        p.error = None;
        p.failed = false;
        self.schedule(key, due);
    }

    /// Sets when the op at `key` becomes eligible, and queues it for then.
    pub fn schedule(&mut self, key: &str, due: Option<Instant>) {
        let Some(p) = self.overlay.get_mut(key) else {
            return;
        };
        p.due = due;
        if let Some(t) = due {
            self.queue.push(Reverse((t, key.to_string())));
        }
    }

    /// Whether a queue entry still stands for an op waiting to be attempted.
    pub fn queued(&self, key: &str, due: Instant) -> bool {
        self.overlay
            .get(key)
            .is_some_and(|p| p.due == Some(due) && p.inflight.is_none() && !p.failed)
    }

    /// When the next queued op becomes eligible. Drops lapsed entries.
    pub fn next_due(&mut self) -> Option<Instant> {
        while let Some(Reverse((due, key))) = self.queue.peek() {
            if self.queued(key, *due) {
                return Some(*due);
            }
            self.queue.pop();
        }
        None
    }

    /// Drops an op that turned out to be unnecessary.
    pub fn clear_op(&mut self, key: &str) {
        if self.overlay.get(key).is_some_and(|p| p.inflight.is_none()) {
            self.overlay.remove(key);
        }
    }

    /// The node at `key` went away locally: hide the remote entry, if there
    /// is (or may soon be) one.
    pub fn retire_key(&mut self, key: &str, after: Option<String>, due: Option<Instant>) {
        let inflight = self.overlay.get(key).is_some_and(|p| p.inflight.is_some());
        if inflight || self.index.visible(key).is_some() {
            self.set_op(key, PendingOp::Delete { after }, due);
        } else {
            self.overlay.remove(key);
        }
    }

    // ---- merging the remote view -----------------------------------------

    /// Makes the tree agree with the index for `key`, unless a local
    /// operation on the key is pending.
    pub fn apply_remote(&mut self, cfg: &VfsConfig, store: &Arc<DataStore>, key: &str) {
        if self.overlay.contains_key(key) {
            return;
        }
        let Some(kp) = cfg.keys.parse(key) else {
            return;
        };
        let winner = self.index.visible(key).cloned();
        match kp {
            KeyPath::DirMarker(path) => match winner {
                Some(e) if e.meta.kind == Kind::Dir => {
                    if let Some(ino) = self.ensure_dir(store, &path) {
                        if let Ok(d) = self.dir_mut(ino) {
                            d.mode = e.meta.mode;
                            d.mtime = e.meta.mtime;
                            d.marker = Some(e);
                        }
                    }
                }
                _ => {
                    if let Some(ino) = self.resolve(&path) {
                        if let Ok(d) = self.dir_mut(ino) {
                            d.marker = None;
                        }
                        self.prune(ino);
                    }
                }
            },
            KeyPath::Node(path) => {
                let (parent_path, name) = split(&path);
                match winner {
                    Some(e) if matches!(e.meta.kind, Kind::File | Kind::Symlink) => {
                        let Some(parent) = self.ensure_dir(store, parent_path) else {
                            return;
                        };
                        match self.child(parent, name) {
                            None => {
                                self.alloc(parent, name, body_from_entry(store, &e));
                            }
                            Some(ino) => {
                                let node = self.nodes.get_mut(&ino).expect("child exists");
                                if matches!(node.body, Body::Dir(_)) || is_dirty(&node.body) {
                                    return; // directories win; local changes win
                                }
                                if current_entry_id(&node.body) != Some(e.id) {
                                    node.body = body_from_entry(store, &e);
                                    node.epoch += 1;
                                }
                            }
                        }
                    }
                    _ => {
                        let Some(ino) = self.resolve(&path) else {
                            return;
                        };
                        let node = self.nodes.get(&ino).expect("resolved");
                        if matches!(node.body, Body::Dir(_)) || is_dirty(&node.body) {
                            return;
                        }
                        let parent = node.parent;
                        self.detach(ino);
                        self.prune(parent);
                    }
                }
            }
        }
    }

    /// Finds or creates (as implicit) the directory at `path`. A clean remote
    /// file in the way is replaced: directories win.
    fn ensure_dir(&mut self, _store: &Arc<DataStore>, path: &str) -> Option<Ino> {
        let mut ino = ROOT;
        if path.is_empty() {
            return Some(ino);
        }
        for part in path.split('/') {
            ino = match self.child(ino, part) {
                Some(c) => match &self.nodes[&c].body {
                    Body::Dir(_) => c,
                    b if is_dirty(b) => return None,
                    _ => {
                        tracing::debug!("{path}: a directory hides a file of the same name");
                        self.detach(c);
                        self.alloc_implicit_dir(ino, part)
                    }
                },
                None => self.alloc_implicit_dir(ino, part),
            };
        }
        Some(ino)
    }

    fn alloc_implicit_dir(&mut self, parent: Ino, name: &str) -> Ino {
        self.alloc(
            parent,
            name,
            Body::Dir(Dir {
                children: BTreeMap::new(),
                marker: None,
                pinned: false,
                mode: 0o755,
                mtime: SystemTime::now(),
            }),
        )
    }

    // ---- files ------------------------------------------------------------

    pub fn add_handle(&mut self, h: Handle) -> u64 {
        if let Content::Remote(rd) = &h.content {
            rd.pin();
        }
        if let Some(n) = self.nodes.get_mut(&h.ino) {
            n.open += 1;
        }
        let fh = self.alloc_fh();
        self.handles.insert(fh, h);
        fh
    }

    pub fn keep_cache(&mut self, ino: Ino, content: &Content, writable: bool) -> bool {
        let Some(n) = self.nodes.get_mut(&ino) else {
            return false;
        };
        let keep = !writable
            && matches!(content, Content::Remote(_))
            && n.last_open_epoch == Some(n.epoch);
        n.last_open_epoch = Some(n.epoch);
        keep
    }

    /// Opens without the network if possible: `Ok(None)` means the remote
    /// content must be materialized first (copy-on-write).
    pub fn open_local(
        &mut self,
        ino: Ino,
        flags: i32,
        cfg: &VfsConfig,
        store: &Arc<DataStore>,
    ) -> Result<Option<(Content, bool)>> {
        let writable = flags & libc::O_ACCMODE != libc::O_RDONLY;
        let trunc = flags & libc::O_TRUNC != 0;
        let key = cfg.keys.node_key(&self.path(ino));
        let attached = self.node(ino)?.attached;
        let due = Some(Instant::now() + cfg.settle);
        let f = self.file_mut(ino)?;
        let content = match f.content.clone() {
            Content::Local(file) => {
                if trunc {
                    file.set_len(0).map_err(io_err)?;
                    f.size = 0;
                    f.generation += 1;
                    f.dirty = true;
                    f.mtime = SystemTime::now();
                }
                Content::Local(file)
            }
            Content::Remote(rd) if !writable && !trunc => {
                return Ok(Some((Content::Remote(rd), false)));
            }
            Content::Remote(rd) => {
                if !trunc && rd.size() > 0 {
                    return Ok(None);
                }
                // Truncated, or empty anyway: nothing to download.
                let file = store.create().map_err(io_err)?;
                f.content = Content::Local(file.clone());
                f.size = 0;
                f.generation += 1;
                if trunc {
                    f.committed = None;
                    f.dirty = true;
                    f.mtime = SystemTime::now();
                } else {
                    f.committed = Some(rd.entry.clone());
                }
                Content::Local(file)
            }
        };
        if writable {
            f.writers += 1;
        }
        let writers = f.writers;
        if trunc {
            self.bump_epoch(ino);
        }
        if attached && (writable || trunc) {
            self.set_op(
                &key,
                PendingOp::Put(ino),
                if writers > 0 { None } else { due },
            );
        }
        Ok(Some((content, writable)))
    }

    /// Switches a node from `rd` to a local copy of it. False if the node no
    /// longer holds `rd`.
    pub fn make_local(
        &mut self,
        ino: Ino,
        rd: &Arc<RemoteData>,
        local: Arc<DataFile>,
        _cfg: &VfsConfig,
    ) -> bool {
        let Ok(f) = self.file_mut(ino) else {
            return false;
        };
        match &f.content {
            Content::Remote(cur) if Arc::ptr_eq(cur, rd) => {
                f.size = rd.size();
                f.committed = Some(rd.entry.clone());
                f.content = Content::Local(local);
                true
            }
            _ => false,
        }
    }

    pub fn close_handle(
        &mut self,
        h: &Handle,
        cfg: &VfsConfig,
        store: &Arc<DataStore>,
        due: Option<Instant>,
    ) {
        if let Content::Remote(rd) = &h.content {
            rd.unpin();
        }
        let path = self.path(h.ino);
        let Some(node) = self.nodes.get_mut(&h.ino) else {
            return;
        };
        node.open = node.open.saturating_sub(1);
        let attached = node.attached;
        let open = node.open;
        if let (true, Body::File(f)) = (h.writable, &mut node.body) {
            f.writers = f.writers.saturating_sub(1);
            if f.writers == 0 {
                let key = cfg.keys.node_key(&path);
                if f.dirty && attached {
                    self.set_op(&key, PendingOp::Put(h.ino), due);
                } else {
                    make_evictable(f, store);
                    if attached
                        && self
                            .overlay
                            .get(&key)
                            .is_some_and(|p| p.op == PendingOp::Put(h.ino))
                    {
                        self.clear_op(&key);
                    }
                }
            }
        }
        if !attached && open == 0 {
            self.nodes.remove(&h.ino);
        }
    }

    // ---- rename -----------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    pub fn rename(
        &mut self,
        cfg: &VfsConfig,
        store: &Arc<DataStore>,
        parent: Ino,
        name: &str,
        newparent: Ino,
        newname: &str,
        mode: RenameMode,
        due: Option<Instant>,
    ) -> Result<Option<RenameNeeds>> {
        let _ = store;
        self.dir(parent)?;
        self.dir(newparent)?;
        if !valid_component(newname) {
            return Err(EINVAL);
        }
        let src = self.child(parent, name).ok_or(ENOENT)?;
        if parent == newparent && name == newname {
            return Ok(None);
        }
        let dst = self.child(newparent, newname);
        if dst.is_some() && mode == RenameMode::NoReplace {
            return Err(EEXIST);
        }
        let new_path = join(&self.path(newparent), newname);
        let src_is_dir = matches!(self.node(src)?.body, Body::Dir(_));
        let new_key = if src_is_dir {
            cfg.keys.dir_key(&new_path)
        } else {
            cfg.keys.node_key(&new_path)
        };
        if new_key.len() > MAX_KEY_LEN {
            return Err(ENAMETOOLONG);
        }

        if src_is_dir {
            if let Some(d) = dst {
                let dir = self.dir(d)?;
                if !dir.children.is_empty() {
                    return Err(ENOTEMPTY);
                }
            }
            if self.is_ancestor(src, newparent) {
                return Err(EINVAL);
            }
            let old_prefix = cfg.keys.dir_key(&self.path(src));
            if self.index.any_visible_with_prefix(&old_prefix) {
                // Remote content cannot be moved without copying it.
                return Err(EXDEV);
            }
            if let Some(d) = dst {
                self.detach(d);
                self.retire_key(&new_key, None, due);
            }
            self.move_node(src, newparent, newname);
            let moved: Vec<(String, PendingOp, Option<Instant>)> = self
                .overlay
                .range(old_prefix.clone()..)
                .take_while(|(k, _)| k.starts_with(&old_prefix))
                .map(|(k, p)| (k.clone(), p.op.clone(), p.due))
                .collect();
            for (key, op, pdue) in moved {
                if let PendingOp::Put(ino) = op {
                    let suffix = &key[old_prefix.len()..];
                    self.retire_key(&key, None, due);
                    self.set_op(&format!("{new_key}{suffix}"), PendingOp::Put(ino), pdue);
                }
            }
            self.pin(parent);
            return Ok(None);
        }

        if let Some(d) = dst {
            if matches!(self.node(d)?.body, Body::Dir(_)) {
                return Err(EISDIR);
            }
        }
        match &self.node(src)?.body {
            Body::File(File {
                content: Content::Remote(rd),
                ..
            }) => return Ok(Some(RenameNeeds::Copy(src, rd.clone()))),
            Body::Symlink(Symlink { target: None, .. }) => {
                return Ok(Some(RenameNeeds::Target(src)));
            }
            _ => {}
        }
        let old_key = cfg.keys.node_key(&self.path(src));
        if let Some(d) = dst {
            self.detach(d);
        }
        self.move_node(src, newparent, newname);
        let writers = match &mut self.node_mut(src)?.body {
            Body::File(f) => {
                f.dirty = true;
                f.generation += 1;
                f.writers
            }
            Body::Symlink(s) => {
                s.dirty = true;
                s.generation += 1;
                0
            }
            Body::Dir(_) => unreachable!("handled above"),
        };
        self.set_op(
            &new_key,
            PendingOp::Put(src),
            if writers > 0 { None } else { due },
        );
        self.retire_key(&old_key, Some(new_key), due);
        self.pin(parent);
        Ok(None)
    }

    // ---- unmount ----------------------------------------------------------

    /// Closes all handles, writes markers for directories that would vanish,
    /// and makes every pending operation due now.
    pub fn prepare_drain(&mut self, cfg: &VfsConfig, store: &Arc<DataStore>) {
        let now = Some(Instant::now());
        let handles: Vec<Handle> = self.handles.drain().map(|(_, h)| h).collect();
        for h in &handles {
            self.close_handle(h, cfg, store, now);
        }
        let empty_dirs: Vec<Ino> = self
            .nodes
            .iter()
            .filter(|(ino, n)| {
                **ino != ROOT
                    && n.attached
                    && matches!(&n.body, Body::Dir(d) if d.pinned && d.children.is_empty() && d.marker.is_none())
            })
            .map(|(ino, _)| *ino)
            .collect();
        for ino in empty_dirs {
            let key = cfg.keys.dir_key(&self.path(ino));
            if key.len() <= MAX_KEY_LEN {
                self.set_op(&key, PendingOp::Put(ino), now);
            }
        }
        let keys: Vec<String> = self.overlay.keys().cloned().collect();
        for key in keys {
            let p = self.overlay.get_mut(&key).expect("listed");
            p.force = true;
            p.attempts = 0;
            self.schedule(&key, now);
        }
    }

    /// Nothing left that the committer will attempt.
    pub fn quiescent(&self) -> bool {
        self.overlay.values().all(|p| {
            p.inflight.is_none()
                && (p.failed
                    || matches!(&p.op, PendingOp::Delete { after: Some(k) }
                        if self.overlay.get(k).is_some_and(|q| q.failed)))
        })
    }

    /// A creation time for our own new entry that sorts after the current
    /// winner of its key in our scope.
    pub fn fresh_created(&self, key: &str) -> SystemTime {
        let now = SystemTime::now();
        match self.index.winner(key) {
            Some(w) if w.scope == 0 && w.created >= now => w.created + Duration::from_micros(1),
            _ => now,
        }
    }
}

fn is_dirty(body: &Body) -> bool {
    match body {
        Body::File(f) => f.dirty || f.writers > 0,
        Body::Symlink(s) => s.dirty,
        Body::Dir(_) => false,
    }
}

fn current_entry_id(body: &Body) -> Option<i64> {
    match body {
        Body::File(f) => match &f.content {
            Content::Remote(rd) => Some(rd.entry.id),
            Content::Local(_) => f.committed.as_ref().map(|e| e.id),
        },
        Body::Symlink(s) => s.remote.as_ref().map(|rd| rd.entry.id),
        Body::Dir(_) => None,
    }
}

fn body_from_entry(store: &Arc<DataStore>, e: &RemoteEntry) -> Body {
    match e.meta.kind {
        Kind::Symlink => Body::Symlink(Symlink {
            target: None,
            remote: Some(RemoteData::new(store, e.clone())),
            mtime: e.meta.mtime,
            dirty: false,
            generation: 0,
        }),
        _ => Body::File(File {
            content: Content::Remote(RemoteData::new(store, e.clone())),
            size: e.logical_size(),
            mode: e.meta.mode,
            mtime: e.meta.mtime,
            writers: 0,
            writes_inflight: 0,
            generation: 0,
            dirty: false,
            committed: None,
        }),
    }
}

/// Committed local content becomes an evictable cache of its entry.
pub(super) fn make_evictable(f: &mut File, store: &Arc<DataStore>) {
    if f.dirty || f.writers > 0 {
        return;
    }
    if let (Content::Local(file), Some(entry)) = (&f.content, &f.committed) {
        f.content = Content::Remote(RemoteData::adopt(store, entry.clone(), file.clone()));
    }
}
