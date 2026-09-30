//! Local backing files: authoritative content for pending writes, and sparse
//! caches of remote entries (layers and blobs) that fill in 1 MiB chunks as
//! they are read, and the files that are ranges of them.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant, SystemTime};

use parking_lot::Mutex;
use tokio::sync::{Semaphore, watch};

use crate::api::{Api, ApiError};
use crate::index::{Listed, NodeId};

/// Granularity of the presence bitmap.
pub const CHUNK: u64 = 1 << 20;
/// Largest single range request.
const MAX_RUN_CHUNKS: u64 = 8;
/// The same, in bytes.
pub const RUN: u64 = MAX_RUN_CHUNKS * CHUNK;

/// Resolves when an in-flight range fetch finishes.
type Fetching = watch::Receiver<Option<Result<(), String>>>;

/// A file in the data directory, deleted when dropped.
#[derive(Debug)]
pub struct DataFile {
    path: PathBuf,
    file: File,
}

impl DataFile {
    pub fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        let mut done = 0;
        while done < buf.len() {
            match self.file.read_at(&mut buf[done..], offset + done as u64) {
                Ok(0) => break,
                Ok(n) => done += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(done)
    }

    pub fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        self.file.write_all_at(buf, offset)
    }

    pub fn set_len(&self, len: u64) -> io::Result<()> {
        self.file.set_len(len)
    }

    pub fn size(&self) -> io::Result<u64> {
        Ok(self.file.metadata()?.len())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for DataFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[derive(Debug)]
pub struct DataStore {
    dir: PathBuf,
    next_id: AtomicU64,
    limit: u64,
    cached: AtomicU64,
    tick: AtomicU64,
    caches: Mutex<HashMap<u64, Weak<RemoteData>>>,
    fetch_slots: Semaphore,
    pub stats: FetchStats,
}

#[derive(Debug, Default)]
pub struct FetchStats {
    pub requests: AtomicU64,
    pub bytes: AtomicU64,
}

impl DataStore {
    /// `dir` is emptied: anything in it belongs to a previous daemon.
    pub fn new(dir: &Path, limit: u64, fetch_concurrency: usize) -> io::Result<Arc<DataStore>> {
        if dir.exists() {
            std::fs::remove_dir_all(dir)?;
        }
        std::fs::create_dir_all(dir)?;
        Ok(Arc::new(DataStore {
            dir: dir.to_path_buf(),
            next_id: AtomicU64::new(1),
            limit,
            cached: AtomicU64::new(0),
            tick: AtomicU64::new(0),
            caches: Mutex::default(),
            fetch_slots: Semaphore::new(fetch_concurrency.max(1)),
            stats: FetchStats::default(),
        }))
    }

    pub fn create(&self) -> io::Result<Arc<DataFile>> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let path = self.dir.join(format!("{id}"));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        Ok(Arc::new(DataFile { path, file }))
    }

    /// A private copy of the first `len` bytes of `src`.
    pub fn copy(&self, src: &DataFile, len: u64) -> io::Result<Arc<DataFile>> {
        let dst = self.create()?;
        // `std::fs::copy` uses copy_file_range / clonefile where available.
        std::fs::copy(&src.path, &dst.path)?;
        dst.set_len(len)?;
        Ok(dst)
    }

    /// A private copy of `[offset, offset+len)` of `src`.
    pub fn copy_range(&self, src: &DataFile, offset: u64, len: u64) -> io::Result<Arc<DataFile>> {
        if offset == 0 {
            return self.copy(src, len);
        }
        let dst = self.create()?;
        let mut buf = vec![0u8; CHUNK.min(len) as usize];
        let mut done = 0;
        while done < len {
            let n = (len - done).min(buf.len() as u64) as usize;
            if src.read_at(&mut buf[..n], offset + done)? < n {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "short cache file",
                ));
            }
            dst.write_all_at(&buf[..n], done)?;
            done += n as u64;
        }
        Ok(dst)
    }

    /// Bytes of remote content currently cached.
    pub fn cached_bytes(&self) -> u64 {
        self.cached.load(Ordering::Relaxed)
    }

    fn register(&self, rd: &Arc<RemoteData>) {
        self.caches.lock().insert(rd.cache_id, Arc::downgrade(rd));
    }

    fn touch(&self, rd: &RemoteData) {
        rd.last_use
            .store(self.tick.fetch_add(1, Ordering::Relaxed), Ordering::Relaxed);
    }

    /// Evicts least-recently-used, unpinned caches until under the limit.
    fn evict(&self) {
        if self.cached_bytes() <= self.limit {
            return;
        }
        let mut candidates: Vec<Arc<RemoteData>> = {
            let mut caches = self.caches.lock();
            caches.retain(|_, w| w.strong_count() > 0);
            caches.values().filter_map(Weak::upgrade).collect()
        };
        candidates.sort_by_key(|rd| rd.last_use.load(Ordering::Relaxed));
        for rd in candidates {
            if self.cached_bytes() <= self.limit {
                break;
            }
            if rd.pins.load(Ordering::Relaxed) == 0 {
                rd.clear();
            }
        }
    }
}

#[derive(Clone, Debug)]
enum Chunk {
    Missing,
    Present,
    Fetching(Fetching),
}

#[derive(Debug)]
struct Cache {
    file: Option<Arc<DataFile>>,
    chunks: Vec<Chunk>,
    present_bytes: u64,
    /// Bumped by eviction, so in-flight fetches into the old file are discarded.
    epoch: u64,
}

/// The locally cached content of one cache entry: a layer or a blob.
#[derive(Debug)]
pub struct RemoteData {
    pub entry: Listed,
    store: Arc<DataStore>,
    cache: Mutex<Cache>,
    url: tokio::sync::Mutex<Option<(String, Instant)>>,
    cache_id: u64,
    last_use: AtomicU64,
    /// Open handles; pinned caches are not evicted.
    pins: AtomicU32,
}

impl Drop for RemoteData {
    fn drop(&mut self) {
        let present = self.cache.get_mut().present_bytes;
        self.store.cached.fetch_sub(present, Ordering::Relaxed);
    }
}

fn chunk_count(size: u64) -> usize {
    size.div_ceil(CHUNK) as usize
}

/// When a SAS URL expires, from its `se=` parameter.
fn sas_expiry(url: &str) -> Option<Instant> {
    let url = url::Url::parse(url).ok()?;
    let se = url.query_pairs().find(|(k, _)| k == "se")?.1;
    let t = humantime::parse_rfc3339_weak(se.trim_end_matches('Z')).ok()?;
    let remaining = t
        .duration_since(SystemTime::now())
        .unwrap_or(Duration::ZERO);
    Some(Instant::now() + remaining)
}

impl RemoteData {
    pub fn new(store: &Arc<DataStore>, entry: Listed) -> Arc<RemoteData> {
        let n = chunk_count(entry.size);
        let rd = Arc::new(RemoteData {
            entry,
            store: store.clone(),
            cache: Mutex::new(Cache {
                file: None,
                chunks: vec![Chunk::Missing; n],
                present_bytes: 0,
                epoch: 0,
            }),
            url: tokio::sync::Mutex::new(None),
            cache_id: store.next_id.fetch_add(1, Ordering::Relaxed),
            last_use: AtomicU64::new(0),
            pins: AtomicU32::new(0),
        });
        store.register(&rd);
        rd
    }

    /// Takes a local file that holds exactly this entry's content as the
    /// cache, unless something is cached already.
    pub fn offer(&self, file: Arc<DataFile>) {
        {
            let mut c = self.cache.lock();
            if c.file.is_some() || c.chunks.is_empty() {
                return;
            }
            let size = self.size();
            c.chunks.fill(Chunk::Present);
            c.present_bytes = size;
            c.file = Some(file);
            self.store.cached.fetch_add(size, Ordering::Relaxed);
        }
        self.store.touch(self);
        self.store.evict();
    }

    /// A download URL resolved elsewhere, to use until it expires.
    pub fn seed_url(&self, url: &str) {
        if let Ok(mut cached) = self.url.try_lock() {
            if cached.is_none() {
                let expiry =
                    sas_expiry(url).unwrap_or_else(|| Instant::now() + Duration::from_secs(300));
                *cached = Some((url.to_string(), expiry));
            }
        }
    }

    pub fn size(&self) -> u64 {
        self.entry.size
    }

    pub fn pin(&self) {
        self.pins.fetch_add(1, Ordering::Relaxed);
    }

    pub fn unpin(&self) {
        self.pins.fetch_sub(1, Ordering::Relaxed);
    }

    /// Whether `[offset, offset+len)` is cached.
    pub fn present(&self, offset: u64, len: u64) -> bool {
        let end = offset.saturating_add(len).min(self.size());
        if offset >= end {
            return true;
        }
        let c = self.cache.lock();
        (offset / CHUNK..=(end - 1) / CHUNK).all(|i| matches!(c.chunks[i as usize], Chunk::Present))
    }

    fn clear(&self) {
        let mut c = self.cache.lock();
        if c.chunks.iter().any(|c| matches!(c, Chunk::Fetching(_))) {
            return;
        }
        c.epoch += 1;
        c.file = None;
        c.chunks.fill(Chunk::Missing);
        self.store
            .cached
            .fetch_sub(c.present_bytes, Ordering::Relaxed);
        c.present_bytes = 0;
    }

    async fn download_url(&self, api: &Api, force: bool) -> Result<String, String> {
        let mut cached = self.url.lock().await;
        if !force {
            if let Some((url, expiry)) = &*cached {
                if Instant::now() + Duration::from_secs(60) < *expiry {
                    return Ok(url.clone());
                }
            }
        }
        match api
            .twirp
            .download_url(&self.entry.key, &self.entry.version)
            .await
        {
            Ok(Some(url)) => {
                let expiry =
                    sas_expiry(&url).unwrap_or_else(|| Instant::now() + Duration::from_secs(300));
                *cached = Some((url.clone(), expiry));
                Ok(url)
            }
            Ok(None) => Err(format!(
                "{}: entry is no longer in the cache",
                self.entry.key
            )),
            Err(e) => Err(format!("{}: resolving download URL: {e}", self.entry.key)),
        }
    }

    /// Starts fetching every missing chunk overlapping `[offset, offset+len)`,
    /// and returns receivers for all chunks there that are not yet present.
    fn start(self: &Arc<Self>, api: &Api, offset: u64, len: u64) -> io::Result<Vec<Fetching>> {
        let size = self.size();
        let end = offset.saturating_add(len).min(size);
        if offset >= end {
            return Ok(Vec::new());
        }
        let (first, last) = (offset / CHUNK, (end - 1) / CHUNK);
        // Recently used before the fetch lands, so eviction does not pick it.
        self.store.touch(self);
        let mut c = self.cache.lock();
        if c.file.is_none() {
            let f = self.store.create()?;
            f.set_len(size)?;
            c.file = Some(f);
        }
        let file = c.file.clone().expect("created above");
        let mut waits = Vec::new();
        let mut i = first;
        while i <= last {
            match &c.chunks[i as usize] {
                Chunk::Present => i += 1,
                Chunk::Fetching(rx) => {
                    waits.push(rx.clone());
                    i += 1;
                }
                Chunk::Missing => {
                    let start = i;
                    let mut stop = i + 1;
                    while stop <= last
                        && stop - start < MAX_RUN_CHUNKS
                        && matches!(c.chunks[stop as usize], Chunk::Missing)
                    {
                        stop += 1;
                    }
                    let (tx, rx) = watch::channel(None);
                    for j in start..stop {
                        c.chunks[j as usize] = Chunk::Fetching(rx.clone());
                    }
                    waits.push(rx);
                    let (rd, api, file, epoch) = (self.clone(), api.clone(), file.clone(), c.epoch);
                    tokio::spawn(async move { rd.fetch(&api, file, epoch, start, stop, tx).await });
                    i = stop;
                }
            }
        }
        Ok(waits)
    }

    async fn fetch(
        self: Arc<Self>,
        api: &Api,
        file: Arc<DataFile>,
        epoch: u64,
        start: u64,
        stop: u64,
        tx: watch::Sender<Option<Result<(), String>>>,
    ) {
        let offset = start * CHUNK;
        let len = (stop * CHUNK).min(self.size()) - offset;
        let result = async {
            let _slot = self
                .store
                .fetch_slots
                .acquire()
                .await
                .map_err(|e| e.to_string())?;
            for attempt in 0..3 {
                let url = self.download_url(api, attempt > 0).await?;
                self.store.stats.requests.fetch_add(1, Ordering::Relaxed);
                match api.blob.get_range(&url, offset, len).await {
                    Ok(bytes) if bytes.len() as u64 == len => {
                        file.write_all_at(&bytes, offset)
                            .map_err(|e| e.to_string())?;
                        self.store.stats.bytes.fetch_add(len, Ordering::Relaxed);
                        return Ok(());
                    }
                    Ok(bytes) => {
                        return Err(format!(
                            "{}: short read at {offset}: {} of {len} bytes",
                            self.entry.key,
                            bytes.len()
                        ));
                    }
                    // Expired URLs, or a blob that moved: resolve again.
                    Err(ApiError::Expired | ApiError::NotFound) if attempt < 2 => continue,
                    Err(e) => return Err(format!("{}: reading at {offset}: {e}", self.entry.key)),
                }
            }
            Err(format!(
                "{}: download URL keeps being rejected",
                self.entry.key
            ))
        }
        .await;
        if let Err(e) = &result {
            tracing::warn!("fetch failed: {e}");
        }
        {
            let mut c = self.cache.lock();
            if c.epoch == epoch {
                let ok = result.is_ok();
                for j in start..stop {
                    c.chunks[j as usize] = if ok { Chunk::Present } else { Chunk::Missing };
                }
                if ok {
                    c.present_bytes += len;
                    self.store.cached.fetch_add(len, Ordering::Relaxed);
                }
            }
        }
        let _ = tx.send(Some(result));
        self.store.evict();
    }

    /// Makes `[offset, offset+len)` present locally.
    pub async fn ensure(self: &Arc<Self>, api: &Api, offset: u64, len: u64) -> Result<(), String> {
        let waits = self.start(api, offset, len).map_err(|e| e.to_string())?;
        for mut rx in waits {
            let r = rx
                .wait_for(Option::is_some)
                .await
                .map_err(|_| "fetch abandoned".to_string())?;
            if let Some(Err(e)) = &*r {
                return Err(e.clone());
            }
        }
        self.store.touch(self);
        Ok(())
    }

    /// Starts fetching `[offset, offset+len)` without waiting.
    pub fn prefetch(self: &Arc<Self>, api: &Api, offset: u64, len: u64) {
        if let Err(e) = self.start(api, offset, len) {
            tracing::debug!("prefetch of {} failed to start: {e}", self.entry.key);
        }
    }

    /// Reads cached bytes. Callers `ensure` the range first; if the cache was
    /// evicted in between, the range is fetched again.
    pub async fn read(
        self: &Arc<Self>,
        api: &Api,
        offset: u64,
        len: u64,
    ) -> Result<Vec<u8>, String> {
        let size = self.size();
        if offset >= size {
            return Ok(Vec::new());
        }
        let len = len.min(size - offset);
        for _ in 0..3 {
            self.ensure(api, offset, len).await?;
            let file = {
                let c = self.cache.lock();
                let (first, last) = (offset / CHUNK, (offset + len - 1) / CHUNK);
                let all = (first..=last).all(|i| matches!(c.chunks[i as usize], Chunk::Present));
                if !all {
                    continue;
                }
                c.file.clone()
            };
            let Some(file) = file else { continue };
            let mut buf = vec![0u8; len as usize];
            let n = file.read_at(&mut buf, offset).map_err(|e| e.to_string())?;
            buf.truncate(n);
            return Ok(buf);
        }
        Err(format!(
            "{}: cache kept being evicted while reading",
            self.entry.key
        ))
    }

    /// A private local copy of `[offset, offset+len)`.
    pub async fn copy_range(
        self: &Arc<Self>,
        api: &Api,
        offset: u64,
        len: u64,
    ) -> Result<Arc<DataFile>, String> {
        self.pin();
        let result = async {
            for _ in 0..3 {
                self.ensure(api, offset, len).await?;
                let file = self.cache.lock().file.clone();
                if let Some(file) = file {
                    if self.present(offset, len) {
                        return self
                            .store
                            .copy_range(&file, offset, len)
                            .map_err(|e| e.to_string());
                    }
                }
            }
            Err(format!(
                "{}: cache kept being evicted while copying",
                self.entry.key
            ))
        }
        .await;
        self.unpin();
        result
    }
}

/// A file's remote content: bytes at hand, or a range of a cache entry.
#[derive(Debug)]
pub struct RemoteFile {
    /// The node of the view it came from.
    pub id: NodeId,
    size: u64,
    source: Source,
}

#[derive(Debug)]
enum Source {
    Inline(Arc<[u8]>),
    Range { rd: Arc<RemoteData>, offset: u64 },
}

impl RemoteFile {
    pub fn inline(id: NodeId, bytes: Arc<[u8]>) -> Arc<RemoteFile> {
        Arc::new(RemoteFile {
            id,
            size: bytes.len() as u64,
            source: Source::Inline(bytes),
        })
    }

    pub fn range(id: NodeId, rd: Arc<RemoteData>, offset: u64, size: u64) -> Arc<RemoteFile> {
        Arc::new(RemoteFile {
            id,
            size,
            source: Source::Range { rd, offset },
        })
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    /// Its bytes, if they are at hand.
    pub fn bytes(&self) -> Option<&Arc<[u8]>> {
        match &self.source {
            Source::Inline(b) => Some(b),
            Source::Range { .. } => None,
        }
    }

    /// The entry and offset it is read from, unless it is at hand.
    pub fn range_of(&self) -> Option<(&Arc<RemoteData>, u64)> {
        match &self.source {
            Source::Inline(_) => None,
            Source::Range { rd, offset } => Some((rd, *offset)),
        }
    }

    pub fn pin(&self) {
        if let Some((rd, _)) = self.range_of() {
            rd.pin();
        }
    }

    pub fn unpin(&self) {
        if let Some((rd, _)) = self.range_of() {
            rd.unpin();
        }
    }

    /// Clamps a read to the file.
    fn clamp(&self, offset: u64, len: u64) -> u64 {
        len.min(self.size.saturating_sub(offset))
    }

    pub async fn read(&self, api: &Api, offset: u64, len: u64) -> Result<Vec<u8>, String> {
        let len = self.clamp(offset, len);
        match &self.source {
            Source::Inline(b) => Ok(b[offset.min(self.size) as usize..][..len as usize].to_vec()),
            Source::Range { rd, offset: base } => {
                if len == 0 {
                    return Ok(Vec::new());
                }
                rd.read(api, base + offset, len).await
            }
        }
    }

    /// Makes `[offset, offset+len)` present locally.
    pub async fn ensure(&self, api: &Api, offset: u64, len: u64) -> Result<(), String> {
        let len = self.clamp(offset, len);
        match &self.source {
            Source::Range { rd, offset: base } if len > 0 => {
                rd.ensure(api, base + offset, len).await
            }
            _ => Ok(()),
        }
    }

    /// Starts fetching `[offset, offset+len)` without waiting.
    pub fn prefetch(&self, api: &Api, offset: u64, len: u64) {
        let len = self.clamp(offset, len);
        if let Source::Range { rd, offset: base } = &self.source {
            if len > 0 {
                rd.prefetch(api, base + offset, len);
            }
        }
    }

    /// A private, complete local copy of the content.
    pub async fn materialize(&self, api: &Api, store: &DataStore) -> Result<Arc<DataFile>, String> {
        match &self.source {
            Source::Inline(b) => {
                let file = store.create().map_err(|e| e.to_string())?;
                file.write_all_at(b, 0).map_err(|e| e.to_string())?;
                Ok(file)
            }
            Source::Range { .. } if self.size == 0 => store.create().map_err(|e| e.to_string()),
            Source::Range { rd, offset } => rd.copy_range(api, *offset, self.size).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sas_expiry() {
        let in_ten = SystemTime::now() + Duration::from_secs(600);
        let se = humantime::format_rfc3339_seconds(in_ten).to_string();
        let url = format!(
            "https://a.blob.core.windows.net/c/b?se={}&sp=r&sig=x",
            se.replace(':', "%3A")
        );
        let exp = sas_expiry(&url).unwrap();
        let left = exp - Instant::now();
        assert!(
            left > Duration::from_secs(590) && left <= Duration::from_secs(600),
            "{left:?}"
        );
        assert!(sas_expiry("https://a/b?sp=r").is_none());
    }

    #[test]
    fn data_files_are_removed_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let store = DataStore::new(&dir.path().join("data"), 1 << 30, 4).unwrap();
        let f = store.create().unwrap();
        f.write_all_at(b"hello", 3).unwrap();
        let mut buf = [0u8; 8];
        assert_eq!(f.read_at(&mut buf, 0).unwrap(), 8);
        assert_eq!(&buf, b"\0\0\0hello");
        let copy = store.copy(&f, 4).unwrap();
        assert_eq!(copy.size().unwrap(), 4);
        let path = f.path().to_path_buf();
        drop(f);
        assert!(!path.exists());
    }
}
