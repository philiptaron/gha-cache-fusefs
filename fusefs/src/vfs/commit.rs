//! The committer: turns pending overlay operations into cache entries.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime};

use bytes::Bytes;
use futures_util::{StreamExt, stream};
use tokio::sync::Semaphore;

use super::state::{Body, Content, PendingOp, State, make_evictable};
use super::{Inner, Ino};
use crate::api::ApiError;
use crate::api::blob::block_id;
use crate::data::{DataFile, RemoteData};
use crate::entry::{Kind, Meta, fresh_nonce};
use crate::index::RemoteEntry;

/// Blobs up to this size are uploaded in one request.
const SINGLE_SHOT: u64 = 16 << 20;
const BLOCK: u64 = 8 << 20;
const BLOCKS_IN_FLIGHT: usize = 4;

pub(super) struct Job {
    key: String,
    attempt: u64,
    op_id: u64,
    what: What,
}

enum What {
    File {
        ino: Ino,
        meta: Meta,
        file: Arc<DataFile>,
        size: u64,
        generation: u64,
    },
    Symlink {
        ino: Ino,
        meta: Meta,
        target: String,
        generation: u64,
    },
    Marker {
        ino: Ino,
        meta: Meta,
    },
    Whiteout {
        meta: Meta,
    },
}

impl What {
    fn meta(&self) -> Meta {
        match self {
            What::File { meta, .. }
            | What::Symlink { meta, .. }
            | What::Marker { meta, .. }
            | What::Whiteout { meta } => *meta,
        }
    }
}

enum Failure {
    /// The node changed during the upload; a later attempt uploads the new state.
    Superseded,
    Api(ApiError),
}

pub(super) async fn run(inner: Arc<Inner>) {
    let slots = Arc::new(Semaphore::new(inner.cfg.upload_concurrency.max(1)));
    loop {
        let notified = inner.wake.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let (jobs, next) = {
            let mut st = inner.st.lock();
            collect(&mut st, &inner, slots.available_permits(), Instant::now())
        };
        for job in jobs {
            let permit = slots.clone().acquire_owned().await.expect("never closed");
            let inner = inner.clone();
            tokio::spawn(async move {
                let result = upload(&inner, &job).await;
                finish(&inner, job, result);
                drop(permit);
                inner.done.notify_waiters();
                inner.wake.notify_one();
            });
        }
        match next {
            Some(t) => {
                let _ = tokio::time::timeout_at(t.into(), notified).await;
            }
            None => notified.await,
        }
    }
}

/// Picks eligible operations and marks them in flight.
fn collect(
    st: &mut State,
    inner: &Inner,
    capacity: usize,
    now: Instant,
) -> (Vec<Job>, Option<Instant>) {
    let mut jobs = Vec::new();
    let mut next: Option<Instant> = None;
    let soon = |t: Instant, next: &mut Option<Instant>| {
        if next.is_none_or(|n| t < n) {
            *next = Some(t);
        }
    };
    let mut stale = Vec::new();
    let keys: Vec<String> = st.overlay.keys().cloned().collect();
    for key in keys {
        if jobs.len() >= capacity {
            break;
        }
        let p = &st.overlay[&key];
        if p.inflight.is_some() || p.failed {
            continue;
        }
        let Some(due) = p.due else { continue };
        if due > now {
            soon(due, &mut next);
            continue;
        }
        let what = match p.op.clone() {
            PendingOp::Put(ino) => {
                let Some(node) = st.nodes.get(&ino).filter(|n| n.attached) else {
                    stale.push(key);
                    continue;
                };
                let path = st.path(ino);
                match &node.body {
                    Body::File(f) => {
                        if !f.dirty || inner.cfg.keys.node_key(&path) != key {
                            stale.push(key);
                            continue;
                        }
                        if f.writes_inflight > 0 {
                            soon(now + Duration::from_millis(20), &mut next);
                            continue;
                        }
                        if f.writers > 0 && !p.force {
                            continue;
                        }
                        let Content::Local(file) = &f.content else {
                            stale.push(key);
                            continue;
                        };
                        let mut meta = Meta::new(Kind::File, f.mode, f.mtime);
                        meta.empty = f.size == 0;
                        What::File {
                            ino,
                            meta,
                            file: file.clone(),
                            size: f.size,
                            generation: f.generation,
                        }
                    }
                    Body::Symlink(s) => match (&s.target, s.dirty) {
                        (Some(target), true) if inner.cfg.keys.node_key(&path) == key => {
                            What::Symlink {
                                ino,
                                meta: Meta::new(Kind::Symlink, 0o777, s.mtime),
                                target: target.clone(),
                                generation: s.generation,
                            }
                        }
                        _ => {
                            stale.push(key);
                            continue;
                        }
                    },
                    Body::Dir(d) => {
                        if !d.children.is_empty()
                            || d.marker.is_some()
                            || inner.cfg.keys.dir_key(&path) != key
                        {
                            stale.push(key);
                            continue;
                        }
                        let mut meta = Meta::new(Kind::Dir, d.mode, d.mtime);
                        meta.empty = true;
                        What::Marker { ino, meta }
                    }
                }
            }
            PendingOp::Delete { after } => {
                let blocked = after
                    .as_ref()
                    .and_then(|k| st.overlay.get(k))
                    .is_some_and(|q| matches!(q.op, PendingOp::Put(_)));
                if blocked {
                    continue;
                }
                if st.index.visible(&key).is_none() {
                    // Nothing to hide (any upload we raced with was abandoned).
                    stale.push(key);
                    continue;
                }
                let mut meta = Meta::new(Kind::Whiteout, 0, SystemTime::now());
                meta.empty = true;
                What::Whiteout { meta }
            }
        };
        let attempt = inner.next_attempt.fetch_add(1, Ordering::Relaxed);
        let p = st.overlay.get_mut(&key).expect("present");
        p.inflight = Some(attempt);
        jobs.push(Job {
            op_id: p.op_id,
            key,
            attempt,
            what,
        });
    }
    for key in stale {
        st.clear_op(&key);
        st.apply_remote(&inner.cfg, &inner.store, &key);
    }
    if jobs.len() >= capacity && capacity > 0 {
        // More may be eligible; a finishing job wakes us.
        next = None;
    }
    (jobs, next)
}

fn still_valid(inner: &Inner, job: &Job) -> bool {
    let st = inner.st.lock();
    let Some(p) = st.overlay.get(&job.key) else {
        return false;
    };
    if p.op_id != job.op_id || p.inflight != Some(job.attempt) {
        return false;
    }
    let body = |ino: &Ino| st.nodes.get(ino).map(|n| &n.body);
    match &job.what {
        What::File {
            ino, generation, ..
        } => {
            matches!(body(ino), Some(Body::File(f)) if f.generation == *generation && f.writes_inflight == 0)
        }
        What::Symlink {
            ino, generation, ..
        } => matches!(body(ino), Some(Body::Symlink(s)) if s.generation == *generation),
        What::Marker { ino, .. } => {
            matches!(body(ino), Some(Body::Dir(d)) if d.children.is_empty())
        }
        What::Whiteout { .. } => true,
    }
}

fn read_exact(file: &DataFile, offset: u64, len: u64) -> Result<Bytes, Failure> {
    let mut buf = vec![0u8; len as usize];
    match file.read_at(&mut buf, offset) {
        Ok(n) if n as u64 == len => Ok(Bytes::from(buf)),
        // The file shrank under us; its generation moved too.
        Ok(_) => Err(Failure::Superseded),
        Err(e) => Err(Failure::Api(ApiError::Invalid(format!(
            "reading local data: {e}"
        )))),
    }
}

/// Uploads the blob and returns its size.
async fn put_blob(inner: &Inner, url: &str, what: &What) -> Result<u64, Failure> {
    let blob = &inner.api.blob;
    let single = |data: Bytes| async move {
        let n = data.len() as u64;
        blob.put_blob(url, data).await.map_err(Failure::Api)?;
        Ok(n)
    };
    match what {
        What::File { file, size, .. } if *size > SINGLE_SHOT => {
            let size = *size;
            let n = size.div_ceil(BLOCK) as usize;
            let ids: Vec<String> = (0..n).map(block_id).collect();
            let results: Vec<Result<(), Failure>> = stream::iter(0..n)
                .map(|i| {
                    let (file, id) = (file.clone(), ids[i].clone());
                    async move {
                        let offset = i as u64 * BLOCK;
                        let data = read_exact(&file, offset, BLOCK.min(size - offset))?;
                        blob.put_block(url, &id, data).await.map_err(Failure::Api)
                    }
                })
                .buffer_unordered(BLOCKS_IN_FLIGHT)
                .collect()
                .await;
            results.into_iter().collect::<Result<(), Failure>>()?;
            blob.put_block_list(url, &ids).await.map_err(Failure::Api)?;
            Ok(size)
        }
        What::File { size: 0, .. } | What::Marker { .. } | What::Whiteout { .. } => {
            // The service rejects empty entries; store a placeholder byte.
            single(Bytes::from_static(&[0])).await
        }
        What::File { file, size, .. } => single(read_exact(file, 0, *size)?).await,
        What::Symlink { target, .. } => single(Bytes::from(target.clone().into_bytes())).await,
    }
}

async fn upload(inner: &Inner, job: &Job) -> Result<(Meta, u64, i64), Failure> {
    let api = &inner.api;
    for _ in 0..3 {
        let meta = Meta {
            nonce: fresh_nonce(),
            ..job.what.meta()
        };
        let version = meta.encode();
        let url = match api.twirp.create(&job.key, &version).await {
            Ok(url) => url,
            // A retried reservation that had in fact succeeded; use a new nonce.
            Err(ApiError::AlreadyExists) => continue,
            Err(e) => return Err(Failure::Api(e)),
        };
        let size = put_blob(inner, &url, &job.what).await?;
        if !still_valid(inner, job) {
            return Err(Failure::Superseded);
        }
        match api.twirp.finalize(&job.key, &version, size).await {
            Ok(id) => {
                tracing::debug!("committed {} ({size} bytes, entry {id})", job.key);
                return Ok((meta, size, id));
            }
            // The reservation is gone or the size did not match; start over.
            Err(ApiError::NotFound) => continue,
            Err(e) => return Err(Failure::Api(e)),
        }
    }
    Err(Failure::Api(ApiError::Server(
        "could not commit after 3 reservations".into(),
    )))
}

fn backoff(attempts: u32) -> Duration {
    Duration::from_secs(1u64 << attempts.min(6)).min(Duration::from_secs(60))
}

fn finish(inner: &Arc<Inner>, job: Job, result: Result<(Meta, u64, i64), Failure>) {
    let mut st = inner.st.lock();
    let current = st
        .overlay
        .get(&job.key)
        .is_some_and(|p| p.op_id == job.op_id);
    if let Some(p) = st.overlay.get_mut(&job.key) {
        if p.inflight == Some(job.attempt) {
            p.inflight = None;
        }
    }
    match result {
        Ok((meta, size, id)) => {
            let stats = &inner.stats;
            match &job.what {
                What::File { .. } | What::Symlink { .. } => {
                    stats.uploaded_files.fetch_add(1, Ordering::Relaxed);
                    stats.uploaded_bytes.fetch_add(size, Ordering::Relaxed);
                }
                What::Marker { .. } => {
                    stats.dir_markers.fetch_add(1, Ordering::Relaxed);
                }
                What::Whiteout { .. } => {
                    stats.whiteouts.fetch_add(1, Ordering::Relaxed);
                }
            }
            let entry = RemoteEntry {
                key: job.key.clone(),
                version: meta.encode(),
                meta,
                size,
                created: st.fresh_created(&job.key),
                id,
                scope: 0,
            };
            st.index.insert(entry.clone());
            let retire = current
                && match job.what {
                    What::File {
                        ino,
                        generation,
                        file,
                        ..
                    } => match st.nodes.get_mut(&ino).map(|n| &mut n.body) {
                        Some(Body::File(f)) if f.generation == generation => {
                            f.dirty = false;
                            f.committed = Some(entry.clone());
                            if matches!(&f.content, Content::Local(l) if Arc::ptr_eq(l, &file)) {
                                make_evictable(f, &inner.store);
                            }
                            true
                        }
                        _ => false,
                    },
                    What::Symlink {
                        ino, generation, ..
                    } => match st.nodes.get_mut(&ino).map(|n| &mut n.body) {
                        Some(Body::Symlink(s)) if s.generation == generation => {
                            s.dirty = false;
                            s.remote = Some(RemoteData::new(&inner.store, entry.clone()));
                            true
                        }
                        _ => false,
                    },
                    What::Marker { ino, .. } => {
                        if let Some(Body::Dir(d)) = st.nodes.get_mut(&ino).map(|n| &mut n.body) {
                            d.marker = Some(entry.clone());
                        }
                        true
                    }
                    What::Whiteout { .. } => true,
                };
            if retire {
                st.overlay.remove(&job.key);
            }
            if !st.overlay.contains_key(&job.key) {
                st.apply_remote(&inner.cfg, &inner.store, &job.key);
            }
            if inner.cfg.gc {
                collect_garbage(inner, &mut st);
            }
        }
        Err(Failure::Superseded) => {
            tracing::debug!("{}: changed during upload; will retry", job.key);
        }
        Err(Failure::Api(e)) => {
            let permanent = !e.is_transient() && e != ApiError::Expired;
            let draining = inner.draining.load(Ordering::SeqCst);
            tracing::warn!("uploading {}: {e}", job.key);
            if let Some(p) = st.overlay.get_mut(&job.key) {
                if p.op_id == job.op_id {
                    p.attempts += 1;
                    p.error = Some(e.to_string());
                    if permanent || (draining && p.attempts >= inner.cfg.drain_attempts) {
                        p.failed = true;
                    } else {
                        p.due = Some(Instant::now() + backoff(p.attempts));
                    }
                }
            }
        }
    }
}

/// Deletes entries of our own scope that newer entries superseded.
fn collect_garbage(inner: &Arc<Inner>, st: &mut State) {
    let old = st.index.take_superseded_own();
    if old.is_empty() {
        return;
    }
    let inner = inner.clone();
    tokio::spawn(async move {
        for e in old {
            match inner.api.rest.delete(e.id).await {
                Ok(()) | Err(ApiError::NotFound) => {
                    inner.stats.gc_deleted.fetch_add(1, Ordering::Relaxed);
                }
                Err(err) => {
                    tracing::warn!("garbage collection of {} failed: {err}", e.key);
                    break;
                }
            }
        }
    });
}
