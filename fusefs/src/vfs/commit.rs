//! The committer: turns each batch of due overlay operations into a layer,
//! and large files into blobs (LAYERS.md §5).

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime};

use bytes::Bytes;
use futures_util::{StreamExt, stream};

use super::state::{Body, Content, PendingOp, State};
use super::{Inner, Ino};
use crate::api::ApiError;
use crate::api::blob::block_id;
use crate::data::{DataFile, RemoteFile};
use crate::entry::{self, DIR_XATTR, Mark, Version, WRITER_XATTR};
use crate::erofs;
use crate::index::{self, Layer, Listed, Node as ViewNode};

/// Files larger than this become blobs; smaller ones go in the layer.
pub const LAYER_FILE_MAX: u64 = 8 << 20;
/// A batch holds at most this many paths, and this much data in its layer.
const BATCH_PATHS: usize = 10_000;
const BATCH_BYTES: u64 = 256 << 20;

/// Entries up to this size are uploaded in one request.
const SINGLE_SHOT: u64 = 16 << 20;
const BLOCK: u64 = 8 << 20;
const BLOCKS_IN_FLIGHT: usize = 4;

/// One path of a batch.
struct Member {
    path: String,
    op_id: u64,
    what: What,
    /// For the removal of a rename's old name: the new name, which must be
    /// in the same layer or an earlier one.
    after: Option<String>,
}

enum What {
    File {
        ino: Ino,
        meta: erofs::Meta,
        file: Arc<DataFile>,
        size: u64,
        generation: u64,
    },
    /// A file of the view under a new name or with new attributes: the
    /// layer refers to its data where it is, or holds it if it is inline.
    Moved {
        ino: Ino,
        meta: erofs::Meta,
        rf: Arc<RemoteFile>,
        generation: u64,
    },
    Symlink {
        ino: Ino,
        meta: erofs::Meta,
        target: String,
        generation: u64,
    },
    Dir {
        ino: Ino,
        meta: erofs::Meta,
        mark: Mark,
        generation: u64,
    },
    Whiteout,
    Drop {
        meta: erofs::Meta,
    },
}

impl What {
    /// Bytes it adds to the layer.
    fn layer_bytes(&self) -> u64 {
        match self {
            What::File { size, .. } if *size <= LAYER_FILE_MAX => *size,
            What::Moved { rf, .. } if rf.bytes().is_some() => rf.size(),
            _ => 0,
        }
    }

    fn is_blob(&self) -> bool {
        matches!(self, What::File { size, .. } if *size > LAYER_FILE_MAX)
    }
}

struct Batch {
    attempt: u64,
    members: Vec<Member>,
    /// The attributes of the directories above the members, by path in the
    /// volume, the root "" included.
    dirs: BTreeMap<String, erofs::Meta>,
}

/// What became of a batch.
struct Committed {
    layer: Option<Layer>,
    /// The sealed image, which holds all of the layer's data.
    sealed: Option<Arc<DataFile>>,
    /// Members in the layer, by index.
    included: HashSet<usize>,
    /// Blobs uploaded for it.
    uploaded: Vec<(Listed, String)>,
    /// Members left out because an upload of theirs failed.
    failed: Vec<(usize, ApiError)>,
}

pub(super) async fn run(inner: Arc<Inner>) {
    loop {
        let notified = inner.wake.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let (batch, next) = {
            let mut st = inner.st.lock();
            collect(&mut st, &inner, Instant::now())
        };
        if let Some(batch) = batch {
            // Layers upload one at a time, so they stack in the order they
            // were sealed.
            let result = commit(&inner, &batch).await;
            finish(&inner, batch, result);
            inner.done.notify_waiters();
            continue;
        }
        match next {
            Some(t) => {
                let _ = tokio::time::timeout_at(t.into(), notified).await;
            }
            None => notified.await,
        }
    }
}

/// What a due operation needs.
enum Next {
    Start(What),
    /// The removal of a rename's old name, which waits for the new name,
    /// now at this path.
    After(String, What),
    /// To be looked at again this much later.
    Later(Duration),
    /// Nothing until something reschedules it.
    Idle,
    /// Nothing at all any more.
    Stale,
}

/// How soon to look again at a file with a write landing.
const WRITE_LANDING: Duration = Duration::from_millis(20);
/// How soon to look again at a rename's whiteout whose new name is not due.
const RENAME_WAITING: Duration = Duration::from_secs(1);
/// How soon to look again at a member that could not go in this batch.
const CONFLICT_WAITING: Duration = Duration::from_millis(100);

/// Takes the due operations, up to a batch's worth, marks them in flight,
/// and says when the next one is due if there are none.
fn collect(st: &mut State, inner: &Inner, now: Instant) -> (Option<Batch>, Option<Instant>) {
    let attempt = inner.next_attempt.fetch_add(1, Ordering::Relaxed);
    let mut members: Vec<Member> = Vec::new();
    let mut waiting: Vec<(String, String, What)> = Vec::new();
    let mut stale = Vec::new();
    let mut bytes = 0;
    while members.len() + waiting.len() < BATCH_PATHS && bytes < BATCH_BYTES {
        match st.queue.peek() {
            Some(Reverse((due, _))) if *due <= now => {}
            _ => break,
        }
        let Reverse((due, path)) = st.queue.pop().expect("peeked");
        if !st.queued(&path, due) {
            continue;
        }
        match examine(st, inner, &path) {
            Next::Start(what) => {
                bytes += what.layer_bytes();
                let p = st.overlay.get_mut(&path).expect("queued");
                p.inflight = Some(attempt);
                members.push(Member {
                    op_id: p.op_id,
                    path,
                    what,
                    after: None,
                });
            }
            Next::After(new_name, what) => waiting.push((path, new_name, what)),
            Next::Later(wait) => st.schedule(&path, Some(now + wait)),
            Next::Idle => {}
            Next::Stale => stale.push(path),
        }
    }
    for path in stale {
        st.clear_op(&path);
        st.apply_remote(&inner.cfg, &inner.store, &path);
    }

    // Nothing may sit below a member that is not a directory: that member
    // replaces what is below it anyway. Those wait for a later batch.
    let leaves: HashSet<String> = members
        .iter()
        .filter(|m| !matches!(m.what, What::Dir { .. } | What::Drop { .. }))
        .map(|m| m.path.clone())
        .collect();
    let below_leaf = |path: &str| {
        let mut p = path;
        while !p.is_empty() {
            p = entry::parent(p);
            if leaves.contains(p) {
                return true;
            }
        }
        false
    };
    let (mut members, conflicts): (Vec<Member>, Vec<Member>) =
        members.into_iter().partition(|m| !below_leaf(&m.path));
    for m in conflicts {
        if let Some(p) = st.overlay.get_mut(&m.path) {
            p.inflight = None;
        }
        st.schedule(&m.path, Some(now + CONFLICT_WAITING));
    }

    // The removal of a rename's old name goes in the layer that has its new
    // name, or in a later one.
    let in_batch: HashSet<String> = members.iter().map(|m| m.path.clone()).collect();
    for (path, new_name, what) in waiting {
        if below_leaf(&path) || !in_batch.contains(&new_name) {
            st.schedule(&path, Some(now + RENAME_WAITING));
            continue;
        }
        let p = st.overlay.get_mut(&path).expect("queued");
        p.inflight = Some(attempt);
        members.push(Member {
            op_id: p.op_id,
            path,
            what,
            after: Some(new_name),
        });
    }

    if members.is_empty() {
        return (None, st.next_due());
    }
    let mut dirs = BTreeMap::new();
    for m in &members {
        let full = inner.cfg.full(&m.path);
        let mut p = full.as_str();
        while !p.is_empty() {
            p = entry::parent(p);
            if dirs.contains_key(p) {
                break;
            }
            dirs.insert(p.to_string(), dir_meta(st, inner, p));
        }
    }
    (
        Some(Batch {
            attempt,
            members,
            dirs,
        }),
        None,
    )
}

/// The attributes a directory passing through a layer carries: the local
/// directory's, else the view's.
fn dir_meta(st: &State, inner: &Inner, full: &str) -> erofs::Meta {
    let local = inner
        .cfg
        .local(full)
        .and_then(|p| st.resolve(p))
        .and_then(|ino| st.dir(ino).ok());
    match (local, st.index.get(full)) {
        (Some(d), _) => meta(d.mode, d.mtime),
        (None, Some(ViewNode::Dir { meta, .. })) => *meta,
        _ => meta(0o755, SystemTime::now()),
    }
}

pub(super) fn meta(mode: u16, mtime: SystemTime) -> erofs::Meta {
    erofs::Meta {
        mode,
        uid: unsafe { libc::geteuid() },
        gid: unsafe { libc::getegid() },
        mtime,
    }
}

/// Decides what the due op at `path` needs.
fn examine(st: &State, inner: &Inner, path: &str) -> Next {
    let p = &st.overlay[path];
    let full = inner.cfg.full(path);
    match &p.op {
        &PendingOp::Put(ino) => {
            let Some(node) = st.nodes.get(&ino).filter(|n| n.attached) else {
                return Next::Stale;
            };
            if st.path(ino) != path {
                return Next::Stale;
            }
            match &node.body {
                Body::File(f) => {
                    if !f.dirty {
                        return Next::Stale;
                    }
                    if f.writes_inflight > 0 {
                        return Next::Later(WRITE_LANDING);
                    }
                    if f.writers > 0 && !p.force {
                        // Closing the file schedules it again.
                        return Next::Idle;
                    }
                    let file = match &f.content {
                        Content::Local(file) => file,
                        Content::Remote(rf) => {
                            return Next::Start(What::Moved {
                                ino,
                                meta: meta(f.mode, f.mtime),
                                rf: rf.clone(),
                                generation: f.generation,
                            });
                        }
                    };
                    Next::Start(What::File {
                        ino,
                        meta: meta(f.mode, f.mtime),
                        file: file.clone(),
                        size: f.size,
                        generation: f.generation,
                    })
                }
                Body::Symlink(s) if s.dirty => Next::Start(What::Symlink {
                    ino,
                    meta: meta(0o777, s.mtime),
                    target: s.target.clone(),
                    generation: s.generation,
                }),
                Body::Symlink(_) => Next::Stale,
                Body::Dir(d) => match d.mark {
                    Some(mark) => Next::Start(What::Dir {
                        ino,
                        meta: meta(d.mode, d.mtime),
                        mark,
                        generation: d.generation,
                    }),
                    None => Next::Stale,
                },
            }
        }
        PendingOp::Remove { after } => {
            let what = match st.index.get(&full) {
                Some(ViewNode::Dir { meta, keep: true }) => What::Drop { meta: *meta },
                Some(ViewNode::File(_) | ViewNode::Symlink { .. }) => What::Whiteout,
                // Nothing to hide (any upload we raced with was abandoned).
                _ => return Next::Stale,
            };
            match after.and_then(|ino| st.pending_put(ino)) {
                Some((new_name, _)) => Next::After(new_name, what),
                None => Next::Start(what),
            }
        }
    }
}

/// Whether a member's node is still what the batch took.
fn unchanged(inner: &Inner, m: &Member) -> bool {
    let st = inner.st.lock();
    let body = |ino: &Ino| st.nodes.get(ino).map(|n| &n.body);
    match &m.what {
        What::File {
            ino, generation, ..
        } => {
            matches!(body(ino), Some(Body::File(f)) if f.generation == *generation && f.writes_inflight == 0)
        }
        What::Moved {
            ino, generation, ..
        } => {
            matches!(body(ino), Some(Body::File(f)) if f.generation == *generation)
        }
        _ => true,
    }
}

/// Uploads the batch's blobs, seals its layer, and uploads that.
async fn commit(inner: &Arc<Inner>, batch: &Batch) -> Result<Committed, ApiError> {
    let mut out = Committed {
        layer: None,
        sealed: None,
        included: HashSet::new(),
        uploaded: Vec::new(),
        failed: Vec::new(),
    };
    let mut left_out: HashSet<usize> = HashSet::new();

    // Blobs first, so that the layer that refers to them lands after them.
    let blobs = upload_blobs(inner, batch).await;
    let mut devices: HashMap<usize, String> = HashMap::new();
    for (i, result) in blobs {
        match result {
            Ok(Some((sha, uploaded))) => {
                devices.insert(i, sha.clone());
                if let Some(entry) = uploaded {
                    out.uploaded.push((entry, sha));
                }
            }
            Ok(None) => {
                left_out.insert(i);
            }
            Err(e) => {
                left_out.insert(i);
                out.failed.push((i, e));
            }
        }
    }

    // Seal. A member that changes while it is copied is left for the next
    // batch, and the layer sealed again without it.
    let (sealed, written) = loop {
        let members: Vec<usize> = (0..batch.members.len())
            .filter(|i| !left_out.contains(i) && !orphaned(batch, *i, &left_out))
            .collect();
        if members.is_empty() {
            return Ok(out);
        }
        let image = image(inner, batch, &members, &devices);
        let file = inner
            .store
            .create()
            .map_err(|e| ApiError::Local(format!("sealing a layer: {e}")))?;
        let sealing = file.clone();
        let written = tokio::task::spawn_blocking(move || {
            let mut w = DataWriter {
                file: &sealing,
                pos: 0,
            };
            image.and_then(|image| image.write_to(&mut w))
        })
        .await
        .expect("sealing does not panic");
        let changed: Vec<usize> = members
            .iter()
            .copied()
            .filter(|&i| !batch.members[i].what.is_blob() && !unchanged(inner, &batch.members[i]))
            .collect();
        match written {
            Ok(w) if changed.is_empty() => {
                out.included = members.into_iter().collect();
                break (file, w);
            }
            Err(erofs::Error::Io(e)) if changed.is_empty() => {
                return Err(ApiError::Local(format!("sealing a layer: {e}")));
            }
            Err(e) if changed.is_empty() => {
                return Err(ApiError::Invalid(format!("sealing a layer: {e}")));
            }
            _ => {
                tracing::debug!(
                    "{} files changed while sealing; sealing again",
                    changed.len()
                );
                left_out.extend(changed);
            }
        }
    };

    // Upload it.
    let size = written.blocks * erofs::BLOCK;
    let (key, version, id) = upload_image(inner, &sealed, size, || {
        Version::layer(written.meta_blocks as u32)
    })
    .await?;
    tracing::debug!(
        "committed layer {key}: {} paths, {size} bytes, entry {id}",
        out.included.len()
    );
    let created = inner.st.lock().index.fresh_created();
    let entry = Listed {
        key,
        version,
        size,
        created,
        accessed: created,
        id,
        scope: 0,
    };
    let mut metadata = vec![0u8; (written.meta_blocks * erofs::BLOCK) as usize];
    sealed
        .read_at(&mut metadata, 0)
        .map_err(|e| ApiError::Local(format!("reading a layer: {e}")))?;
    let layer = Layer::parse(entry, written.meta_blocks as u32, &metadata)
        .map_err(|e| ApiError::Invalid(format!("reading a layer: {e}")))?;
    out.layer = Some(layer);
    out.sealed = Some(sealed);
    Ok(out)
}

/// Uploads a sealed image of `size` bytes as a new layer or snapshot, and
/// returns its key, version, and entry id. Every reservation gets a fresh
/// version from `version`.
pub(super) async fn upload_image(
    inner: &Inner,
    sealed: &Arc<DataFile>,
    size: u64,
    version: impl Fn() -> Version,
) -> Result<(String, String, i64), ApiError> {
    let api = &inner.api;
    for _ in 0..3 {
        let version = version();
        let key = inner.cfg.volume.layer_key(version.nonce);
        let encoded = version.encode();
        let url = match api.twirp.create(&key, &encoded).await {
            Ok(url) => url,
            // A retried reservation that had in fact succeeded; use a new nonce.
            Err(ApiError::AlreadyExists) => continue,
            Err(e) => return Err(e),
        };
        put(inner, &url, sealed, size).await?;
        match api.twirp.finalize(&key, &encoded, size).await {
            Ok(id) => return Ok((key, encoded, id)),
            // The reservation is gone or the size did not match; start over.
            Err(ApiError::NotFound) => continue,
            Err(e) => return Err(e),
        }
    }
    Err(ApiError::Server(
        "could not commit a layer after 3 reservations".into(),
    ))
}

/// Whether a rename's whiteout lost its new name.
fn orphaned(batch: &Batch, i: usize, left_out: &HashSet<usize>) -> bool {
    let Some(k) = &batch.members[i].after else {
        return false;
    };
    batch
        .members
        .iter()
        .enumerate()
        .any(|(j, m)| &m.path == k && left_out.contains(&j))
}

/// The layer's image, not yet written.
fn image(
    inner: &Inner,
    batch: &Batch,
    members: &[usize],
    devices: &HashMap<usize, String>,
) -> erofs::Result<erofs::Image> {
    use erofs::{FileData, Image, Node, NodeKind};
    let root = batch
        .dirs
        .get("")
        .copied()
        .unwrap_or_else(|| meta(0o755, SystemTime::now()));
    let mut image = Image::new(root);
    let writer = writer();
    let mut nodes: BTreeMap<String, Node> = BTreeMap::new();
    let mut slots: HashMap<String, u16> = HashMap::new();
    for &i in members {
        let m = &batch.members[i];
        let (kind, meta, mark) = match &m.what {
            What::File {
                file, size, meta, ..
            } => {
                let data = match devices.get(&i) {
                    Some(sha) => {
                        let device = match slots.get(sha) {
                            Some(d) => *d,
                            None => {
                                let d = image.add_device(erofs::Device {
                                    tag: sha.clone(),
                                    blocks: size.div_ceil(erofs::BLOCK),
                                })?;
                                slots.insert(sha.clone(), d);
                                d
                            }
                        };
                        FileData::Device {
                            len: *size,
                            device,
                            start: 0,
                        }
                    }
                    None => FileData::Here {
                        len: *size,
                        source: Box::new(DataReader {
                            file: file.clone(),
                            pos: 0,
                        }),
                    },
                };
                (NodeKind::File(data), *meta, None)
            }
            What::Moved { meta, rf, .. } => {
                let len = rf.size();
                let data = match (rf.bytes(), rf.range_of()) {
                    (Some(bytes), _) => FileData::Here {
                        len,
                        source: Box::new(std::io::Cursor::new(bytes.clone())),
                    },
                    (None, Some((rd, offset))) => reference(
                        &mut image,
                        &mut slots,
                        &inner.cfg.volume,
                        &rd.entry,
                        offset,
                        len,
                    )?,
                    (None, None) => unreachable!("remote data is inline or a range"),
                };
                (NodeKind::File(data), *meta, None)
            }
            What::Symlink { meta, target, .. } => (NodeKind::Symlink(target.clone()), *meta, None),
            What::Dir { meta, mark, .. } => (NodeKind::Dir, *meta, Some(*mark)),
            What::Whiteout => (NodeKind::Whiteout, self::meta(0, SystemTime::now()), None),
            What::Drop { meta } => (NodeKind::Dir, *meta, Some(Mark::Drop)),
        };
        let xattrs = mark
            .map(|m| vec![(DIR_XATTR.to_string(), m.as_bytes().to_vec())])
            .unwrap_or_default();
        nodes.insert(inner.cfg.full(&m.path), Node { meta, kind, xattrs });
    }
    for (path, meta) in &batch.dirs {
        if !nodes.contains_key(path) {
            nodes.insert(
                path.clone(),
                Node {
                    meta: *meta,
                    kind: NodeKind::Dir,
                    xattrs: Vec::new(),
                },
            );
        }
    }
    for (path, mut node) in nodes {
        if path.is_empty() {
            node.xattrs
                .push((WRITER_XATTR.to_string(), writer.clone().into_bytes()));
        }
        image.insert(&path, node)?;
    }
    Ok(image)
}

/// What a layer's root says about who wrote it (`WRITER_XATTR`).
pub(super) fn writer() -> String {
    format!(
        "gha-cache-fusefs {}{}",
        env!("CARGO_PKG_VERSION"),
        match (
            std::env::var("GITHUB_RUN_ID"),
            std::env::var("GITHUB_RUN_ATTEMPT")
        ) {
            (Ok(id), Ok(attempt)) => format!(", run {id} attempt {attempt}"),
            _ => String::new(),
        }
    )
}

/// Data of `len` bytes at `offset` in the layer or blob `entry`, through
/// its device slot, which is added if the image has none yet (LAYERS.md §3).
pub(super) fn reference(
    image: &mut erofs::Image,
    slots: &mut HashMap<String, u16>,
    volume: &entry::Volume,
    entry: &Listed,
    offset: u64,
    len: u64,
) -> erofs::Result<erofs::FileData> {
    let tag = volume
        .device_tag(&entry.key)
        .ok_or_else(|| erofs::Error::Tree(format!("{}: not ours", entry.key)))?;
    let device = match slots.get(&tag) {
        Some(d) => *d,
        None => {
            let d = image.add_device(erofs::Device {
                tag: tag.clone(),
                blocks: entry.size.div_ceil(erofs::BLOCK),
            })?;
            slots.insert(tag, d);
            d
        }
    };
    Ok(erofs::FileData::Device {
        len,
        device,
        start: offset / erofs::BLOCK,
    })
}

/// What became of a large file's blob.
enum BlobOutcome {
    /// A readable scope has it, and it is still there.
    Known,
    Uploaded(Listed),
    /// The file changed; it waits for the next batch.
    Changed,
}

/// Uploads the blobs of the batch's large files, in parallel. Per member:
/// the digest and the entry uploaded, if one was; `None` if the file
/// changed.
#[allow(clippy::type_complexity)]
async fn upload_blobs(
    inner: &Arc<Inner>,
    batch: &Batch,
) -> Vec<(usize, Result<Option<(String, Option<Listed>)>, ApiError>)> {
    let large: Vec<usize> = (0..batch.members.len())
        .filter(|&i| batch.members[i].what.is_blob())
        .collect();
    if large.is_empty() {
        return Vec::new();
    }
    // Digests first, so that files with the same content upload one blob.
    let digests: Vec<(usize, Result<Option<String>, ApiError>)> = stream::iter(large)
        .map(|i| async move {
            let What::File { file, size, .. } = &batch.members[i].what else {
                unreachable!("blobs are files")
            };
            let (file, size) = (file.clone(), *size);
            let started = Instant::now();
            let sha = tokio::task::spawn_blocking(move || digest(file, size))
                .await
                .expect("hashing does not panic");
            tracing::debug!(
                "{}: hashed {size} bytes in {:?}",
                batch.members[i].path,
                started.elapsed()
            );
            let result = match (sha, unchanged(inner, &batch.members[i])) {
                (_, false) => Ok(None),
                (Ok(sha), true) => Ok(Some(sha)),
                (Err(e), true) => Err(ApiError::Local(format!("hashing: {e}"))),
            };
            (i, result)
        })
        .buffer_unordered(inner.cfg.upload_concurrency.max(1))
        .collect()
        .await;
    let mut by_sha: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    let mut out = Vec::new();
    for (i, sha) in digests {
        match sha {
            Ok(Some(sha)) => by_sha.entry(sha).or_default().push(i),
            Ok(None) => out.push((i, Ok(None))),
            Err(e) => out.push((i, Err(e))),
        }
    }
    let uploads: Vec<(Vec<usize>, String, Result<BlobOutcome, ApiError>)> = stream::iter(by_sha)
        .map(|(sha, members)| async move {
            let result = match reusable(inner, &sha).await {
                true => Ok(BlobOutcome::Known),
                false => upload_blob(inner, batch, &members, &sha).await,
            };
            (members, sha, result)
        })
        .buffer_unordered(inner.cfg.upload_concurrency.max(1))
        .collect()
        .await;
    for (members, sha, result) in uploads {
        match result {
            Ok(BlobOutcome::Changed) => {
                for i in members {
                    out.push((i, Ok(None)));
                }
            }
            Ok(outcome) => {
                let entry = match outcome {
                    BlobOutcome::Uploaded(entry) => Some(entry),
                    _ => None,
                };
                // The first member carries the new entry; the rest share it.
                for (n, i) in members.into_iter().enumerate() {
                    let entry = if n == 0 { entry.clone() } else { None };
                    out.push((i, Ok(Some((sha.clone(), entry)))));
                }
            }
            Err(e) => {
                for i in members {
                    out.push((i, Err(e.clone())));
                }
            }
        }
    }
    out
}

/// Whether a readable scope has a blob with this digest that is still in
/// the cache. The listing may be old, and a layer that refers to an evicted
/// blob hides its file; resolving a download URL confirms the blob, and
/// counts as use.
async fn reusable(inner: &Inner, sha: &str) -> bool {
    let Some(blob) = inner.st.lock().index.blob(sha).cloned() else {
        return false;
    };
    match inner.api.twirp.download_url(&blob.key, &blob.version).await {
        Ok(Some(_)) => true,
        Ok(None) => {
            tracing::debug!("{}: gone; uploading it again", blob.key);
            false
        }
        Err(e) => {
            tracing::debug!("{}: {e}; uploading it again", blob.key);
            false
        }
    }
}

/// The SHA-256 of the first `size` bytes of a file, in hex.
fn digest(file: Arc<DataFile>, size: u64) -> std::io::Result<String> {
    let mut hasher = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buf = vec![0u8; (1 << 20).min(size as usize)];
    let mut done = 0;
    while done < size {
        let n = (size - done).min(buf.len() as u64) as usize;
        if file.read_at(&mut buf[..n], done)? < n {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "the file shrank",
            ));
        }
        hasher.update(&buf[..n]);
        done += n as u64;
    }
    Ok(hex::encode(hasher.finish()))
}

/// Uploads one blob from the first of `members`, which all have its content.
/// `Changed` if they changed meanwhile.
async fn upload_blob(
    inner: &Arc<Inner>,
    batch: &Batch,
    members: &[usize],
    sha: &str,
) -> Result<BlobOutcome, ApiError> {
    let m = &batch.members[members[0]];
    let What::File { file, size, .. } = &m.what else {
        unreachable!("blobs are files")
    };
    let key = inner.cfg.volume.blob_key(sha);
    let api = &inner.api;
    for _ in 0..3 {
        let version = Version::blob().encode();
        let url = match api.twirp.create(&key, &version).await {
            Ok(url) => url,
            Err(ApiError::AlreadyExists) => continue,
            Err(e) => return Err(e),
        };
        match put(inner, &url, file, *size).await {
            Ok(()) => {}
            // The file shrank under us; its generation moved too.
            Err(_) if !unchanged(inner, m) => return Ok(BlobOutcome::Changed),
            Err(e) => return Err(e),
        }
        if !members.iter().all(|&i| unchanged(inner, &batch.members[i])) {
            return Ok(BlobOutcome::Changed);
        }
        match api.twirp.finalize(&key, &version, *size).await {
            Ok(id) => {
                inner.stats.blobs.fetch_add(1, Ordering::Relaxed);
                let now = SystemTime::now();
                return Ok(BlobOutcome::Uploaded(Listed {
                    key: key.clone(),
                    version,
                    size: *size,
                    created: now,
                    accessed: now,
                    id,
                    scope: 0,
                }));
            }
            Err(ApiError::NotFound) => continue,
            Err(e) => return Err(e),
        }
    }
    Err(ApiError::Server(format!(
        "{key}: could not commit after 3 reservations"
    )))
}

fn read_exact(file: &DataFile, offset: u64, len: u64) -> Result<Bytes, ApiError> {
    let mut buf = vec![0u8; len as usize];
    match file.read_at(&mut buf, offset) {
        Ok(n) if n as u64 == len => Ok(Bytes::from(buf)),
        Ok(_) => Err(ApiError::Local(
            "a local file shrank while uploading".into(),
        )),
        Err(e) => Err(ApiError::Local(format!("reading local data: {e}"))),
    }
}

/// Uploads the first `size` bytes of `file` to an entry's blob.
async fn put(inner: &Inner, url: &str, file: &Arc<DataFile>, size: u64) -> Result<(), ApiError> {
    let blob = &inner.api.blob;
    if size <= SINGLE_SHOT {
        return blob.put_blob(url, read_exact(file, 0, size)?).await;
    }
    let n = size.div_ceil(BLOCK) as usize;
    let ids: Vec<String> = (0..n).map(block_id).collect();
    let results: Vec<Result<(), ApiError>> = stream::iter(0..n)
        .map(|i| {
            let (file, id) = (file.clone(), ids[i].clone());
            async move {
                let offset = i as u64 * BLOCK;
                let data = read_exact(&file, offset, BLOCK.min(size - offset))?;
                blob.put_block(url, &id, data).await
            }
        })
        .buffer_unordered(BLOCKS_IN_FLIGHT)
        .collect()
        .await;
    results.into_iter().collect::<Result<(), ApiError>>()?;
    blob.put_block_list(url, &ids).await
}

fn backoff(attempts: u32) -> Duration {
    Duration::from_secs(1u64 << attempts.min(6)).min(Duration::from_secs(60))
}

fn finish(inner: &Arc<Inner>, batch: Batch, result: Result<Committed, ApiError>) {
    let mut st = inner.st.lock();
    // Anything that rescheduled a member while it was in flight lapsed.
    for m in &batch.members {
        let landed = match st.overlay.get_mut(&m.path) {
            Some(p) if p.inflight == Some(batch.attempt) => {
                p.inflight = None;
                Some(p.due)
            }
            _ => None,
        };
        if let Some(due) = landed {
            st.schedule(&m.path, due);
        }
    }
    let current =
        |st: &State, m: &Member| st.overlay.get(&m.path).is_some_and(|p| p.op_id == m.op_id);
    let c = match result {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("uploading a layer of {} paths: {e}", batch.members.len());
            let failed: Vec<(usize, ApiError)> =
                (0..batch.members.len()).map(|i| (i, e.clone())).collect();
            fail(inner, &mut st, &batch, failed);
            return;
        }
    };
    for (entry, sha) in c.uploaded {
        st.index.insert_blob(entry, sha);
    }
    let Some(layer) = c.layer else {
        fail(inner, &mut st, &batch, c.failed);
        return;
    };
    let layer_id = layer.entry.id;
    let layer_entry = layer.entry.clone();
    st.index.insert_layer(layer);
    let changed = st.index.restack();
    if let Some(sealed) = c.sealed {
        st.entry_data(&inner.store, &layer_entry).offer(sealed);
    }

    let stats = &inner.stats;
    stats.layers.fetch_add(1, Ordering::Relaxed);
    for (i, m) in batch.members.iter().enumerate() {
        if !c.included.contains(&i) {
            continue;
        }
        match &m.what {
            What::File { size, .. } => {
                stats.uploaded_files.fetch_add(1, Ordering::Relaxed);
                stats.uploaded_bytes.fetch_add(*size, Ordering::Relaxed);
            }
            What::Moved { .. } | What::Symlink { .. } => {
                stats.uploaded_files.fetch_add(1, Ordering::Relaxed);
            }
            What::Dir { .. } | What::Drop { .. } => {
                stats.dir_markers.fetch_add(1, Ordering::Relaxed);
            }
            What::Whiteout => {
                stats.whiteouts.fetch_add(1, Ordering::Relaxed);
            }
        }
        if !current(&st, m) {
            continue;
        }
        let full = inner.cfg.full(&m.path);
        let view = st.index.get(&full).cloned();
        let ours = |id: index::NodeId| id.layer == layer_id;
        let retire = match &m.what {
            What::File {
                ino, generation, ..
            } => {
                let committed = match &view {
                    Some(ViewNode::File(f)) if ours(f.id) => Some(st.remote_file(&inner.store, f)),
                    _ => None,
                };
                let clean = match st.nodes.get_mut(ino).map(|n| &mut n.body) {
                    Some(Body::File(f)) if f.generation == *generation => {
                        f.dirty = false;
                        f.committed = committed;
                        true
                    }
                    _ => false,
                };
                if clean {
                    st.make_evictable(*ino);
                }
                clean
            }
            What::Moved {
                ino, generation, ..
            } => {
                // It now reads through the file this layer holds.
                let committed = match &view {
                    Some(ViewNode::File(f)) if ours(f.id) => Some(st.remote_file(&inner.store, f)),
                    _ => None,
                };
                match st.nodes.get_mut(ino).map(|n| &mut n.body) {
                    Some(Body::File(f)) if f.generation == *generation => {
                        f.dirty = false;
                        if let (Some(rf), Content::Remote(_)) = (committed, &f.content) {
                            f.content = Content::Remote(rf);
                        }
                        true
                    }
                    _ => false,
                }
            }
            What::Symlink {
                ino, generation, ..
            } => match st.nodes.get_mut(ino).map(|n| &mut n.body) {
                Some(Body::Symlink(s)) if s.generation == *generation => {
                    s.dirty = false;
                    s.id = match &view {
                        Some(ViewNode::Symlink { id, .. }) if ours(*id) => Some(*id),
                        _ => None,
                    };
                    true
                }
                _ => false,
            },
            What::Dir {
                ino, generation, ..
            } => match st.dir_mut(*ino) {
                // A chmod, or a mark changed at unmount, while this was in
                // flight: the op stays, and commits the directory as it is.
                Ok(d) if d.generation != *generation => false,
                Ok(d) => {
                    d.mark = None;
                    true
                }
                Err(_) => true,
            },
            What::Whiteout | What::Drop { .. } => true,
        };
        if retire {
            st.overlay.remove(&m.path);
        }
    }
    for full in &changed {
        if let Some(path) = inner.cfg.local(full) {
            st.apply_remote(&inner.cfg, &inner.store, path);
        }
    }
    fail(inner, &mut st, &batch, c.failed);
}

/// Records failed uploads: retried later, or given up on.
fn fail(inner: &Inner, st: &mut State, batch: &Batch, failed: Vec<(usize, ApiError)>) {
    let draining = inner.draining.load(Ordering::SeqCst);
    for (i, e) in failed {
        let m = &batch.members[i];
        let permanent = !e.is_transient() && e != ApiError::Expired;
        let mut retry = None;
        if let Some(p) = st.overlay.get_mut(&m.path) {
            if p.op_id == m.op_id {
                p.attempts += 1;
                p.error = Some(e.to_string());
                if permanent || (draining && p.attempts >= inner.cfg.drain_attempts) {
                    p.failed = true;
                } else {
                    retry = Some(Instant::now() + backoff(p.attempts));
                }
            }
        }
        if retry.is_some() {
            st.schedule(&m.path, retry);
        }
    }
}

/// Reads a data file from the start, for sealing.
struct DataReader {
    file: Arc<DataFile>,
    pos: u64,
}

impl Read for DataReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.file.read_at(buf, self.pos)?;
        self.pos += n as u64;
        Ok(n)
    }
}

/// Writes a data file from the start, for sealing.
pub(super) struct DataWriter<'a> {
    pub file: &'a DataFile,
    pub pos: u64,
}

impl std::io::Write for DataWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.file.write_all_at(buf, self.pos)?;
        self.pos += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
