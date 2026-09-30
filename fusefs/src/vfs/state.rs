//! The mutable state behind the filesystem's lock: inodes, handles, the remote
//! index, and the overlay of pending operations.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use super::{
    Attr, DirEntry, EEXIST, EINVAL, EISDIR, ENOENT, ENOTDIR, ENOTEMPTY, Errno, FileKind, Ino, ROOT,
    RenameMode, Result, VfsConfig, io_err,
};
use crate::data::{DataFile, DataStore, RemoteData, RemoteFile};
use crate::entry::{Mark, join, split, valid_component};
use crate::index::{FileRef, Index, Listed, Node as ViewNode, NodeId, Where};

#[derive(Clone, Debug)]
pub(super) enum Content {
    /// Authoritative local data (pending, or not yet evictable).
    Local(Arc<DataFile>),
    /// A file of the view, and its local cache.
    Remote(Arc<RemoteFile>),
}

#[derive(Debug)]
pub(super) struct Dir {
    pub children: BTreeMap<String, Ino>,
    /// The view keeps it: exists even when empty.
    pub kept: bool,
    /// Exists even without children (created or emptied locally).
    pub pinned: bool,
    /// A mark to commit: `keep` after `mkdir`, `attrs` after `chmod`.
    pub mark: Option<Mark>,
    /// Bumped by every change to the mark or attributes; a commit retires
    /// the mark only if it did not move.
    pub generation: u64,
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
    /// The file of the view that holds this local content, once committed.
    pub committed: Option<Arc<RemoteFile>>,
}

#[derive(Debug)]
pub(super) struct Symlink {
    pub target: String,
    /// The node of the view it came from, or was committed as.
    pub id: Option<NodeId>,
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
    /// Commit the node now at this path: a file, a symlink, or a
    /// directory's mark.
    Put(Ino),
    /// Hide what the view shows here: a file or symlink with a whiteout, a
    /// kept directory with a `drop` mark. A rename's whiteout waits while
    /// the renamed inode, `after`, has a `Put` pending wherever it is now.
    Remove { after: Option<Ino> },
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

pub(super) struct State {
    pub nodes: HashMap<Ino, Node>,
    next_ino: Ino,
    pub handles: HashMap<u64, Handle>,
    pub dir_handles: HashMap<u64, Vec<DirEntry>>,
    next_fh: u64,
    next_op: u64,
    pub index: Index,
    /// Pending operations by path below the mount root.
    pub overlay: BTreeMap<String, Pending>,
    /// Overlay ops by due time, earliest first, so that the committer looks
    /// only at what is due. An entry lapses when its op is rescheduled,
    /// starts, fails, or goes away; `queued` tells.
    pub queue: BinaryHeap<Reverse<(Instant, String)>>,
    pub last_refresh: Instant,
    /// Directories whose small files were already prefetched.
    pub prefetched_dirs: HashSet<Ino>,
    /// The caches of the layers and blobs files are read from, by entry id.
    entries: HashMap<i64, Arc<RemoteData>>,
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
                    kept: false,
                    pinned: true,
                    mark: None,
                    generation: 0,
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
            entries: HashMap::new(),
        }
    }

    /// Remote files in `dir` small enough to fetch speculatively.
    pub fn small_remote_files(
        &self,
        dir: Ino,
        max_size: u64,
        limit: usize,
    ) -> Vec<Arc<RemoteFile>> {
        let Ok(d) = self.dir(dir) else {
            return Vec::new();
        };
        d.children
            .values()
            .filter_map(|c| match self.nodes.get(c).map(|n| &n.body) {
                Some(Body::File(File {
                    content: Content::Remote(rf),
                    ..
                })) if (1..=max_size).contains(&rf.size()) && rf.range_of().is_some() => {
                    Some(rf.clone())
                }
                _ => None,
            })
            .take(limit)
            .collect()
    }

    /// The cache of a layer or blob.
    pub fn entry_data(&mut self, store: &Arc<DataStore>, entry: &Listed) -> Arc<RemoteData> {
        self.entries
            .entry(entry.id)
            .or_insert_with(|| RemoteData::new(store, entry.clone()))
            .clone()
    }

    /// The content of a file of the view.
    pub fn remote_file(&mut self, store: &Arc<DataStore>, f: &FileRef) -> Arc<RemoteFile> {
        match &f.data {
            Where::Inline(bytes) => RemoteFile::inline(f.id, bytes.clone()),
            Where::Layer { layer, offset } => {
                let rd = self.entry_data(store, &layer.entry);
                if let Some(url) = &layer.url {
                    rd.seed_url(url);
                }
                RemoteFile::range(f.id, rd, *offset, f.size)
            }
            Where::Blob { blob, offset } => {
                let rd = self.entry_data(store, blob);
                RemoteFile::range(f.id, rd, *offset, f.size)
            }
        }
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

    pub fn dir_mut(&mut self, ino: Ino) -> Result<&mut Dir> {
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

    pub fn resolve(&self, path: &str) -> Option<Ino> {
        let mut ino = ROOT;
        if path.is_empty() {
            return Some(ino);
        }
        for part in path.split('/') {
            ino = self.child(ino, part)?;
        }
        Some(ino)
    }

    /// A directory and everything below it, parents first, each with its
    /// path relative to the directory ("" for the directory itself).
    fn tree(&self, dir: Ino) -> Vec<(Ino, String)> {
        let mut out = vec![(dir, String::new())];
        let mut i = 0;
        while i < out.len() {
            let (ino, path) = out[i].clone();
            if let Ok(d) = self.dir(ino) {
                for (name, &child) in &d.children {
                    out.push((child, format!("{path}/{name}")));
                }
            }
            i += 1;
        }
        out
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
                    Content::Remote(rf) => rf.size(),
                };
                (FileKind::File, size, f.mtime, f.mode, 1)
            }
            Body::Symlink(s) => (FileKind::Symlink, s.target.len() as u64, s.mtime, 0o777, 1),
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

    /// Adds a node, as a local operation: the parent's mtime moves.
    pub fn alloc(&mut self, parent: Ino, name: &str, body: Body) -> Ino {
        let ino = self.alloc_remote(parent, name, body);
        if let Ok(d) = self.dir_mut(parent) {
            d.mtime = SystemTime::now();
        }
        ino
    }

    /// Adds a node the view has: the parent keeps the view's mtime.
    fn alloc_remote(&mut self, parent: Ino, name: &str, body: Body) -> Ino {
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
        }
        ino
    }

    /// Unlinks a node (and, for directories, everything below it), as a
    /// local operation: the parent's mtime moves. Open nodes stay alive
    /// until their last handle is released.
    pub fn detach(&mut self, ino: Ino) {
        let parent = self.nodes.get(&ino).map(|n| n.parent);
        self.detach_remote(ino);
        if let Some(Ok(d)) = parent.map(|p| self.dir_mut(p)) {
            d.mtime = SystemTime::now();
        }
    }

    /// Unlinks a node the view no longer has.
    fn detach_remote(&mut self, ino: Ino) {
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
            }
        }
        for c in children {
            self.detach_remote(c);
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
                Body::Dir(d)
                    if !d.pinned && !d.kept && d.mark.is_none() && d.children.is_empty() =>
                {
                    self.detach_remote(ino);
                    ino = parent;
                }
                _ => return,
            }
        }
    }

    // ---- the overlay ------------------------------------------------------

    pub fn set_op(&mut self, path: &str, op: PendingOp, due: Option<Instant>) {
        let op_id = self.next_op;
        self.next_op += 1;
        let p = self
            .overlay
            .entry(path.to_string())
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
        self.schedule(path, due);
    }

    /// Sets when the op at `path` becomes eligible, and queues it for then.
    pub fn schedule(&mut self, path: &str, due: Option<Instant>) {
        let Some(p) = self.overlay.get_mut(path) else {
            return;
        };
        p.due = due;
        if let Some(t) = due {
            self.queue.push(Reverse((t, path.to_string())));
        }
    }

    /// Whether a queue entry still stands for an op waiting to be attempted.
    pub fn queued(&self, path: &str, due: Instant) -> bool {
        self.overlay
            .get(path)
            .is_some_and(|p| p.due == Some(due) && p.inflight.is_none() && !p.failed)
    }

    /// When the next queued op becomes eligible. Drops lapsed entries.
    pub fn next_due(&mut self) -> Option<Instant> {
        while let Some(Reverse((due, path))) = self.queue.peek() {
            if self.queued(path, *due) {
                return Some(*due);
            }
            self.queue.pop();
        }
        None
    }

    /// Drops an op that turned out to be unnecessary.
    pub fn clear_op(&mut self, path: &str) {
        if self.overlay.get(path).is_some_and(|p| p.inflight.is_none()) {
            self.overlay.remove(path);
        }
    }

    fn inflight(&self, path: &str) -> bool {
        self.overlay.get(path).is_some_and(|p| p.inflight.is_some())
    }

    /// The node at `path` went away locally: hide what the view shows there,
    /// if anything (or soon, if an upload is under way). Whatever went away,
    /// the view may show something else there, such as the file a local
    /// directory replaced.
    pub fn retire(
        &mut self,
        cfg: &VfsConfig,
        path: &str,
        after: Option<Ino>,
        due: Option<Instant>,
    ) {
        let full = cfg.full(path);
        if self.inflight(path) || self.index.leaf_at(&full) || self.index.kept(&full) {
            self.set_op(path, PendingOp::Remove { after }, due);
        } else {
            self.overlay.remove(path);
        }
    }

    /// Whether ops are pending below `path`.
    fn ops_below(&self, path: &str) -> bool {
        let prefix = format!("{path}/");
        self.overlay
            .range(prefix.clone()..)
            .next()
            .is_some_and(|(k, _)| k.starts_with(&prefix))
    }

    // ---- merging the remote view -----------------------------------------

    /// Makes the tree agree with the view at `path` (below the mount root),
    /// unless a local operation there is pending.
    pub fn apply_remote(&mut self, cfg: &VfsConfig, store: &Arc<DataStore>, path: &str) {
        if self.overlay.contains_key(path) {
            return;
        }
        let node = self.index.get(&cfg.full(path)).cloned();
        let (parent_path, name) = split(path);
        match node {
            Some(ViewNode::Dir { meta, keep }) => {
                let ino = if path.is_empty() {
                    ROOT
                } else {
                    match self.ensure_dir(path) {
                        Some(ino) => ino,
                        None => return,
                    }
                };
                if let Ok(d) = self.dir_mut(ino) {
                    if d.mark.is_none() {
                        d.mode = meta.mode;
                        d.mtime = meta.mtime;
                    }
                    d.kept = keep;
                }
            }
            Some(leaf) if !path.is_empty() => {
                let Some(parent) = self.ensure_dir(parent_path) else {
                    return;
                };
                match self.child(parent, name) {
                    None => {
                        let body = self.body_from_view(store, &leaf);
                        self.alloc_remote(parent, name, body);
                    }
                    Some(ino) => {
                        let node = self.nodes.get(&ino).expect("child exists");
                        match &node.body {
                            Body::Dir(d) => {
                                // A file replaced this directory, unless we
                                // are still changing something in it.
                                if d.pinned || self.ops_below(path) {
                                    return;
                                }
                                self.detach_remote(ino);
                                let body = self.body_from_view(store, &leaf);
                                self.alloc_remote(parent, name, body);
                            }
                            b if is_dirty(b) => {} // local changes win
                            b => {
                                if current_id(b) != view_id(&leaf) {
                                    let body = self.body_from_view(store, &leaf);
                                    let node = self.nodes.get_mut(&ino).expect("child exists");
                                    node.body = body;
                                    node.epoch += 1;
                                }
                            }
                        }
                    }
                }
            }
            Some(_) => {} // the mount root is always a directory
            None => {
                let Some(ino) = self.resolve(path) else {
                    return;
                };
                let node = self.nodes.get(&ino).expect("resolved");
                let parent = node.parent;
                match &node.body {
                    Body::Dir(_) => {
                        if let Ok(d) = self.dir_mut(ino) {
                            d.kept = false;
                        }
                        self.prune(ino);
                    }
                    b if is_dirty(b) => {}
                    _ => {
                        self.detach_remote(ino);
                        self.prune(parent);
                    }
                }
            }
        }
    }

    /// Finds or creates (as implicit) the directory at `path`. A clean remote
    /// file in the way is replaced: the view says a directory is there.
    fn ensure_dir(&mut self, path: &str) -> Option<Ino> {
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
                        self.detach_remote(c);
                        self.alloc_implicit_dir(ino, part)
                    }
                },
                None => self.alloc_implicit_dir(ino, part),
            };
        }
        Some(ino)
    }

    fn alloc_implicit_dir(&mut self, parent: Ino, name: &str) -> Ino {
        self.alloc_remote(
            parent,
            name,
            Body::Dir(Dir {
                children: BTreeMap::new(),
                kept: false,
                pinned: false,
                mark: None,
                generation: 0,
                mode: 0o755,
                mtime: SystemTime::now(),
            }),
        )
    }

    fn body_from_view(&mut self, store: &Arc<DataStore>, node: &ViewNode) -> Body {
        match node {
            ViewNode::Symlink { id, meta, target } => Body::Symlink(Symlink {
                target: target.clone(),
                id: Some(*id),
                mtime: meta.mtime,
                dirty: false,
                generation: 0,
            }),
            ViewNode::File(f) => Body::File(File {
                content: Content::Remote(self.remote_file(store, f)),
                size: f.size,
                mode: f.meta.mode,
                mtime: f.meta.mtime,
                writers: 0,
                writes_inflight: 0,
                generation: 0,
                dirty: false,
                committed: None,
            }),
            ViewNode::Dir { .. } => unreachable!("directories are made by ensure_dir"),
        }
    }

    // ---- files ------------------------------------------------------------

    pub fn add_handle(&mut self, h: Handle) -> u64 {
        if let Content::Remote(rf) = &h.content {
            rf.pin();
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
        let path = self.path(ino);
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
            Content::Remote(rf) if !writable && !trunc => {
                return Ok(Some((Content::Remote(rf), false)));
            }
            Content::Remote(rf) => {
                if !trunc && rf.size() > 0 {
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
                    f.committed = Some(rf);
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
                &path,
                PendingOp::Put(ino),
                if writers > 0 { None } else { due },
            );
        }
        Ok(Some((content, writable)))
    }

    /// Switches a node from `rf` to a local copy of it. False if the node no
    /// longer holds `rf`.
    pub fn make_local(&mut self, ino: Ino, rf: &Arc<RemoteFile>, local: Arc<DataFile>) -> bool {
        let Ok(f) = self.file_mut(ino) else {
            return false;
        };
        match &f.content {
            Content::Remote(cur) if Arc::ptr_eq(cur, rf) => {
                f.size = rf.size();
                f.committed = Some(rf.clone());
                f.content = Content::Local(local);
                true
            }
            _ => false,
        }
    }

    pub fn close_handle(&mut self, h: &Handle, due: Option<Instant>) {
        if let Content::Remote(rf) = &h.content {
            rf.unpin();
        }
        let path = self.path(h.ino);
        let Some(node) = self.nodes.get_mut(&h.ino) else {
            return;
        };
        node.open = node.open.saturating_sub(1);
        let attached = node.attached;
        let open = node.open;
        let mut evictable = false;
        if let (true, Body::File(f)) = (h.writable, &mut node.body) {
            f.writers = f.writers.saturating_sub(1);
            if f.writers == 0 {
                if f.dirty && attached {
                    self.set_op(&path, PendingOp::Put(h.ino), due);
                } else {
                    evictable = true;
                    if attached
                        && self
                            .overlay
                            .get(&path)
                            .is_some_and(|p| p.op == PendingOp::Put(h.ino))
                    {
                        self.clear_op(&path);
                    }
                }
            }
        }
        if evictable {
            self.make_evictable(h.ino);
        }
        if !attached && open == 0 {
            self.nodes.remove(&h.ino);
        }
    }

    /// Committed local content becomes an evictable cache of the committed
    /// file: the entry that holds it takes the local copy.
    pub fn make_evictable(&mut self, ino: Ino) {
        let Some(Body::File(f)) = self.nodes.get_mut(&ino).map(|n| &mut n.body) else {
            return;
        };
        if f.dirty || f.writers > 0 {
            return;
        }
        let (Content::Local(file), Some(rf)) = (&f.content, &f.committed) else {
            return;
        };
        if let Some((rd, 0)) = rf.range_of() {
            if rd.size() == rf.size() {
                // A blob: the local copy is all of it.
                rd.offer(file.clone());
            }
        }
        f.content = Content::Remote(rf.clone());
    }

    // ---- rename -----------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    pub fn rename(
        &mut self,
        cfg: &VfsConfig,
        parent: Ino,
        name: &str,
        newparent: Ino,
        newname: &str,
        mode: RenameMode,
        due: Option<Instant>,
    ) -> Result<()> {
        self.dir(parent)?;
        self.dir(newparent)?;
        if !valid_component(newname) {
            return Err(EINVAL);
        }
        let src = self.child(parent, name).ok_or(ENOENT)?;
        if parent == newparent && name == newname {
            return Ok(());
        }
        let dst = self.child(newparent, newname);
        if dst.is_some() && mode == RenameMode::NoReplace {
            return Err(EEXIST);
        }
        let new_path = join(&self.path(newparent), newname);
        let old_path = self.path(src);
        let src_is_dir = matches!(self.node(src)?.body, Body::Dir(_));

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
            let tree = self.tree(src);
            if let Some(d) = dst {
                self.detach(d);
                self.retire(cfg, &new_path, None, due);
            }
            self.move_node(src, newparent, newname);
            // Everything in the directory moves with it: what is pending
            // keeps its due time, and what is committed is committed again
            // under its new name, referring to the data it has (LAYERS.md
            // §3), with a keep mark for each directory. Each old name is
            // removed once its new name has landed.
            for (ino, suffix) in tree {
                let (old, new) = (format!("{old_path}{suffix}"), format!("{new_path}{suffix}"));
                let pending = self
                    .overlay
                    .get(&old)
                    .filter(|p| p.op == PendingOp::Put(ino))
                    .map(|p| p.due);
                let put_due = match (&mut self.node_mut(ino)?.body, pending) {
                    (Body::Dir(d), pending) => {
                        d.mark = Some(Mark::Keep);
                        d.generation += 1;
                        pending.unwrap_or(due)
                    }
                    (_, Some(pdue)) => pdue,
                    (Body::File(f), None) => {
                        f.dirty = true;
                        f.generation += 1;
                        if f.writers > 0 { None } else { due }
                    }
                    (Body::Symlink(s), None) => {
                        s.dirty = true;
                        s.generation += 1;
                        due
                    }
                };
                self.set_op(&new, PendingOp::Put(ino), put_due);
                self.retire(cfg, &old, Some(ino), due);
            }
            self.pin(parent);
            return Ok(());
        }

        if let Some(d) = dst {
            if matches!(self.node(d)?.body, Body::Dir(_)) {
                return Err(EISDIR);
            }
        }
        // A remote file is committed again under its new name, referring to
        // the data it has.
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
            &new_path,
            PendingOp::Put(src),
            if writers > 0 { None } else { due },
        );
        self.retire(cfg, &old_path, Some(src), due);
        self.pin(parent);
        Ok(())
    }

    // ---- unmount ----------------------------------------------------------

    /// Closes all handles, keeps directories that would vanish, and makes
    /// every pending operation due now.
    pub fn prepare_drain(&mut self) {
        let now = Some(Instant::now());
        let handles: Vec<Handle> = self.handles.drain().map(|(_, h)| h).collect();
        for h in &handles {
            self.close_handle(h, now);
        }
        let empty_dirs: Vec<Ino> = self
            .nodes
            .iter()
            .filter(|(ino, n)| {
                **ino != ROOT
                    && n.attached
                    && matches!(&n.body, Body::Dir(d) if d.pinned && d.children.is_empty() && !d.kept)
            })
            .map(|(ino, _)| *ino)
            .collect();
        for ino in empty_dirs {
            let path = self.path(ino);
            if let Ok(d) = self.dir_mut(ino) {
                d.mark = Some(Mark::Keep);
                d.generation += 1;
            }
            self.set_op(&path, PendingOp::Put(ino), now);
        }
        let paths: Vec<String> = self.overlay.keys().cloned().collect();
        for path in paths {
            let p = self.overlay.get_mut(&path).expect("listed");
            p.force = true;
            p.attempts = 0;
            self.schedule(&path, now);
        }
    }

    /// Nothing left that the committer will attempt.
    pub fn quiescent(&self) -> bool {
        self.overlay.values().all(|p| {
            p.inflight.is_none()
                && (p.failed
                    || matches!(&p.op, PendingOp::Remove { after: Some(ino) }
                        if self.pending_put(*ino).is_some_and(|(_, q)| q.failed)))
        })
    }

    /// The `Put` pending for an inode, wherever it is now, and its path.
    pub fn pending_put(&self, ino: Ino) -> Option<(String, &Pending)> {
        if !self.nodes.get(&ino)?.attached {
            return None;
        }
        let path = self.path(ino);
        let p = self
            .overlay
            .get(&path)
            .filter(|p| p.op == PendingOp::Put(ino))?;
        Some((path, p))
    }
}

pub(super) fn is_dirty(body: &Body) -> bool {
    match body {
        Body::File(f) => f.dirty || f.writers > 0,
        Body::Symlink(s) => s.dirty,
        Body::Dir(_) => false,
    }
}

fn current_id(body: &Body) -> Option<NodeId> {
    match body {
        Body::File(f) => match &f.content {
            Content::Remote(rf) => Some(rf.id),
            Content::Local(_) => f.committed.as_ref().map(|rf| rf.id),
        },
        Body::Symlink(s) => s.id,
        Body::Dir(_) => None,
    }
}

fn view_id(node: &ViewNode) -> Option<NodeId> {
    match node {
        ViewNode::File(f) => Some(f.id),
        ViewNode::Symlink { id, .. } => Some(*id),
        ViewNode::Dir { .. } => None,
    }
}
