//! A fake of the Actions cache: the three Twirp methods, SAS-style blob
//! endpoints, and the REST list/delete endpoints, with the semantics measured
//! against the real service (DESIGN.md §1). Used by tests, the VM test, and
//! benchmarks, which can also give it the real service's latency, bandwidth,
//! and rate limit ([`FakeConfig::hosted`]).
//!
//! Runtime tokens name the scopes: `fake:<write ref>:<read ref>,<read ref>` (refs
//! cannot contain colons).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::time::Instant;

use crate::config::{CacheMode, Env};

#[derive(Clone, Debug)]
pub struct FakeConfig {
    /// How long download URLs stay valid.
    pub url_ttl: Duration,
    pub default_branch: String,
    /// Fail every Nth blob request with a 503.
    pub fail_blob_every: Option<u64>,
    /// Reject REST requests (as if `actions: read` were missing).
    pub deny_rest: bool,
    /// How long every request takes before it is answered.
    pub latency: Duration,
    /// How fast blob bodies travel.
    pub download: Option<Link>,
    pub upload: Option<Link>,
    /// Beyond this many `CreateCacheEntry` calls per window, answer 429.
    pub create_limit: Option<RateLimit>,
    /// Until then, answer REST requests as if the `GITHUB_TOKEN` budget were spent.
    pub rest_exhausted_until: Option<SystemTime>,
}

/// Transfer rates, in bytes per second.
#[derive(Clone, Copy, Debug)]
pub struct Link {
    /// For one request on its own.
    pub per_request: u64,
    /// For all requests together.
    pub total: u64,
}

/// A fixed window: it opens with the first call, admits `calls`, and answers
/// the rest with a `Retry-After` of the time until it closes.
#[derive(Clone, Copy, Debug)]
pub struct RateLimit {
    pub calls: u32,
    pub window: Duration,
}

impl Default for FakeConfig {
    fn default() -> Self {
        FakeConfig {
            url_ttl: Duration::from_secs(600),
            default_branch: "main".into(),
            fail_blob_every: None,
            deny_rest: false,
            latency: Duration::ZERO,
            download: None,
            upload: None,
            create_limit: None,
            rest_exhausted_until: None,
        }
    }
}

impl FakeConfig {
    /// The service as measured from hosted runners (DESIGN.md §1): ~250 ms per
    /// request; 23 MB/s per download and 52 MB/s for parallel downloads;
    /// 84 MB/s for parallel uploads (one upload on its own was not measured,
    /// so it is assumed to match a download); and 200 creations, then 429s
    /// whose `Retry-After` of 30–40 s suggests a window of about 45 s.
    pub fn hosted() -> FakeConfig {
        FakeConfig {
            latency: Duration::from_millis(250),
            download: Some(Link {
                per_request: 23_000_000,
                total: 52_000_000,
            }),
            upload: Some(Link {
                per_request: 23_000_000,
                total: 84_000_000,
            }),
            create_limit: Some(RateLimit {
                calls: 200,
                window: Duration::from_secs(45),
            }),
            ..FakeConfig::default()
        }
    }
}

/// A link that blob transfers share. Each takes at least its size over the
/// per-request rate, and they queue, in arrival order, for their size over
/// the total rate. (Real links share bandwidth fairly instead; for the
/// benchmarks, which rarely mix large and small transfers, that is close
/// enough.)
#[derive(Default)]
struct Pipe {
    free_at: Mutex<Option<Instant>>,
}

impl Pipe {
    async fn carry(&self, link: Option<Link>, bytes: usize) {
        let Some(link) = link else { return };
        let time = |rate: u64| Duration::from_secs_f64(bytes as f64 / rate.max(1) as f64);
        let now = Instant::now();
        let queued = {
            let mut free = self.free_at.lock();
            let end = free.map_or(now, |f| f.max(now)) + time(link.total);
            *free = Some(end);
            end
        };
        tokio::time::sleep_until(queued.max(now + time(link.per_request))).await;
    }
}

#[derive(Clone, Debug)]
struct Entry {
    id: i64,
    key: String,
    version: String,
    scope: String,
    finalized: bool,
    size: u64,
    created: SystemTime,
    accessed: SystemTime,
}

#[derive(Default)]
struct BlobData {
    committed: Option<Bytes>,
    blocks: HashMap<String, Bytes>,
}

struct Inner {
    cfg: Mutex<FakeConfig>,
    base: String,
    entries: Mutex<Vec<Entry>>,
    blobs: Mutex<HashMap<i64, BlobData>>,
    next_id: AtomicI64,
    blob_requests: AtomicU64,
    twirp_requests: AtomicU64,
    downloads: Pipe,
    uploads: Pipe,
    /// When the creation window opened, and how many calls it admitted.
    window: Mutex<(Instant, u32)>,
}

impl Inner {
    async fn delay(&self) {
        let latency = self.cfg.lock().latency;
        if !latency.is_zero() {
            tokio::time::sleep(latency).await;
        }
    }

    /// Counts a creation; `Some(wait)` if the window is full.
    fn over_limit(&self) -> Option<Duration> {
        let limit = self.cfg.lock().create_limit?;
        let now = Instant::now();
        let mut w = self.window.lock();
        let (opened, calls) = &mut *w;
        if *calls == 0 || now >= *opened + limit.window {
            *opened = now;
            *calls = 0;
        }
        if *calls >= limit.calls {
            return Some(*opened + limit.window - now);
        }
        *calls += 1;
        None
    }
}

pub struct FakeServer {
    pub addr: SocketAddr,
    inner: Arc<Inner>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn twirp_error(status: StatusCode, code: &str, msg: &str) -> Response {
    (status, axum::Json(json!({"code": code, "msg": msg}))).into_response()
}

fn scopes_of(headers: &HeaderMap) -> Option<(String, Vec<String>)> {
    let token = headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")?;
    let mut parts = token.split(':');
    if parts.next()? != "fake" {
        return None;
    }
    let write = parts.next()?.to_string();
    let mut read = vec![write.clone()];
    if let Some(rest) = parts.next() {
        read.extend(
            rest.split(',')
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        );
    }
    Some((write, read))
}

fn fmt_time(t: SystemTime) -> String {
    humantime::format_rfc3339_micros(t).to_string()
}

fn int_field(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_str()?.parse().ok())
}

impl FakeServer {
    pub async fn start(cfg: FakeConfig) -> std::io::Result<FakeServer> {
        FakeServer::bind("127.0.0.1:0".parse().expect("valid"), cfg).await
    }

    pub async fn bind(addr: SocketAddr, cfg: FakeConfig) -> std::io::Result<FakeServer> {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        let addr = listener.local_addr()?;
        let inner = Arc::new(Inner {
            cfg: Mutex::new(cfg),
            base: format!("http://{addr}"),
            entries: Mutex::default(),
            blobs: Mutex::default(),
            next_id: AtomicI64::new(1000),
            blob_requests: AtomicU64::new(0),
            twirp_requests: AtomicU64::new(0),
            downloads: Pipe::default(),
            uploads: Pipe::default(),
            window: Mutex::new((Instant::now(), 0)),
        });
        let app = Router::new()
            .route(
                "/twirp/github.actions.results.api.v1.CacheService/{method}",
                post(twirp),
            )
            .route("/blob/{id}", put(blob_put).get(blob_get))
            .route(
                "/repos/{owner}/{repo}/actions/caches",
                get(list).delete(delete_by_key),
            )
            .route(
                "/repos/{owner}/{repo}/actions/caches/{id}",
                axum::routing::delete(delete_one),
            )
            .route("/repos/{owner}/{repo}", get(repo))
            .layer(DefaultBodyLimit::max(1 << 30))
            .with_state(inner.clone());
        let task = tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                tracing::error!("fake server: {e}");
            }
        });
        Ok(FakeServer { addr, inner, task })
    }

    pub fn base_url(&self) -> &str {
        &self.inner.base
    }

    /// An environment for a run on `git_ref` that may also read `readable`
    /// (the default branch is always readable, as on GitHub).
    pub fn env(&self, git_ref: &str, readable: &[&str]) -> Env {
        let default_branch = self.inner.cfg.lock().default_branch.clone();
        let mut read: Vec<String> = readable.iter().map(|s| s.to_string()).collect();
        let default_ref = format!("refs/heads/{default_branch}");
        if git_ref != default_ref && !read.contains(&default_ref) {
            read.push(default_ref);
        }
        Env {
            results_url: format!("{}/", self.inner.base),
            runtime_token: format!("fake:{git_ref}:{}", read.join(",")),
            cache_mode: CacheMode::ReadWrite,
            github_token: Some("fake-github-token".into()),
            api_url: self.inner.base.clone(),
            repository: "owner/repo".into(),
            git_ref: git_ref.into(),
            base_ref: None,
            default_branch: Some(default_branch),
        }
    }

    pub fn set_config(&self, f: impl FnOnce(&mut FakeConfig)) {
        f(&mut self.inner.cfg.lock());
    }

    /// Finalized entries, as `(key, scope, size)`.
    pub fn entries(&self) -> Vec<(String, String, u64)> {
        self.inner
            .entries
            .lock()
            .iter()
            .filter(|e| e.finalized)
            .map(|e| (e.key.clone(), e.scope.clone(), e.size))
            .collect()
    }

    pub fn blob_requests(&self) -> u64 {
        self.inner.blob_requests.load(Ordering::Relaxed)
    }

    pub fn twirp_requests(&self) -> u64 {
        self.inner.twirp_requests.load(Ordering::Relaxed)
    }

    /// Adds a finalized entry without going through the API, which is much
    /// faster when a benchmark needs a large listing.
    pub fn insert(&self, key: &str, version: &str, scope: &str, data: Bytes) {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let now = SystemTime::now();
        self.inner.entries.lock().push(Entry {
            id,
            key: key.into(),
            version: version.into(),
            scope: scope.into(),
            finalized: true,
            size: data.len() as u64,
            created: now,
            accessed: now,
        });
        self.inner.blobs.lock().insert(
            id,
            BlobData {
                committed: Some(data),
                blocks: HashMap::new(),
            },
        );
    }

    /// Deletes the entries with this key in `scope`, as eviction would.
    pub fn remove(&self, key: &str, scope: &str) {
        let mut entries = self.inner.entries.lock();
        let mut blobs = self.inner.blobs.lock();
        entries.retain(|e| {
            let gone = e.finalized && e.key == key && e.scope == scope;
            if gone {
                blobs.remove(&e.id);
            }
            !gone
        });
    }

    /// Moves every entry's creation and last use this far into the past.
    pub fn age(&self, by: Duration) {
        for e in self.inner.entries.lock().iter_mut() {
            e.created -= by;
            e.accessed -= by;
        }
    }

    /// When an entry with this key in `scope` was last used.
    pub fn last_used(&self, key: &str, scope: &str) -> Option<SystemTime> {
        let entries = self.inner.entries.lock();
        entries
            .iter()
            .filter(|e| e.finalized && e.key == key && e.scope == scope)
            .map(|e| e.accessed)
            .max()
    }

    /// Serves until the process is killed (for the `fake-server` subcommand).
    pub async fn wait(mut self) {
        let _ = (&mut self.task).await;
    }
}

async fn twirp(
    State(s): State<Arc<Inner>>,
    Path(method): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    s.twirp_requests.fetch_add(1, Ordering::Relaxed);
    s.delay().await;
    let Some((write, read)) = scopes_of(&headers) else {
        return twirp_error(StatusCode::UNAUTHORIZED, "unauthenticated", "bad token");
    };
    let Ok(req) = serde_json::from_slice::<Value>(&body) else {
        return twirp_error(StatusCode::BAD_REQUEST, "malformed", "bad json");
    };
    let key = req["key"].as_str().unwrap_or("").to_string();
    let version = req["version"].as_str().unwrap_or("").to_string();
    let n = key.chars().count();
    if !(1..=512).contains(&n) {
        return twirp_error(
            StatusCode::BAD_REQUEST,
            "invalid_argument",
            "key invalid length for cache entry key: must be between 1 and 512 characters",
        );
    }
    if !matches!(
        method.as_str(),
        "CreateCacheEntry" | "FinalizeCacheEntryUpload" | "GetCacheEntryDownloadURL"
    ) {
        return twirp_error(StatusCode::NOT_FOUND, "bad_route", "no handler for path");
    }
    if version.len() != 64 || !version.bytes().all(|b| b.is_ascii_hexdigit()) {
        return twirp_error(
            StatusCode::BAD_REQUEST,
            "invalid_argument",
            "version invalid length for cache entry version: must be between 1 and 64 characters",
        );
    }
    let now = SystemTime::now();
    match method.as_str() {
        "CreateCacheEntry" => {
            if let Some(wait) = s.over_limit() {
                let mut resp = twirp_error(
                    StatusCode::TOO_MANY_REQUESTS,
                    "resource_exhausted",
                    "too many cache entries created; try again later",
                );
                let secs = wait.as_secs_f64().ceil().max(1.0) as u64;
                resp.headers_mut().insert(header::RETRY_AFTER, secs.into());
                return resp;
            }
            let mut entries = s.entries.lock();
            if entries
                .iter()
                .any(|e| e.key == key && e.version == version && e.scope == write)
            {
                return twirp_error(
                    StatusCode::CONFLICT,
                    "already_exists",
                    "cache entry with the same key, version, and scope already exists",
                );
            }
            let id = s.next_id.fetch_add(1, Ordering::Relaxed);
            entries.push(Entry {
                id,
                key,
                version,
                scope: write,
                finalized: false,
                size: 0,
                created: now,
                accessed: now,
            });
            s.blobs.lock().insert(id, BlobData::default());
            let se = fmt_time(now + Duration::from_secs(3600));
            let url = format!("{}/blob/{id}?se={se}&sp=cw&sig=fake", s.base);
            axum::Json(json!({"ok": true, "signed_upload_url": url, "message": ""})).into_response()
        }
        "FinalizeCacheEntryUpload" => {
            let size = int_field(&req["size_bytes"]).unwrap_or(0);
            if size < 1 {
                return twirp_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_argument",
                    "size invalid cache size value: must be between 1 and 9223372036854775807 bytes",
                );
            }
            let mut entries = s.entries.lock();
            let Some(e) = entries
                .iter_mut()
                .find(|e| e.key == key && e.version == version && e.scope == write && !e.finalized)
            else {
                return twirp_error(StatusCode::NOT_FOUND, "not_found", "cache entry not found");
            };
            let blob_size = s
                .blobs
                .lock()
                .get(&e.id)
                .and_then(|b| b.committed.as_ref().map(|c| c.len() as i64));
            if blob_size != Some(size) {
                return twirp_error(StatusCode::NOT_FOUND, "not_found", "cache entry not found");
            }
            e.finalized = true;
            e.size = size as u64;
            e.created = now;
            e.accessed = now;
            axum::Json(json!({"ok": true, "entry_id": e.id.to_string(), "message": ""}))
                .into_response()
        }
        _ => {
            let restore: Vec<String> = req["restore_keys"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let mut entries = s.entries.lock();
            let mut found: Option<usize> = None;
            {
                let newest = |pred: &dyn Fn(&Entry) -> bool| {
                    entries
                        .iter()
                        .enumerate()
                        .filter(|(_, e)| e.finalized && e.version == version && pred(e))
                        .max_by_key(|(_, e)| e.created)
                        .map(|(i, _)| i)
                };
                // Per scope: the exact key, then prefix matches on the key,
                // then on each restore key.
                'scopes: for scope in &read {
                    if let Some(i) = newest(&|e| e.scope == *scope && e.key == key) {
                        found = Some(i);
                        break;
                    }
                    for prefix in std::iter::once(&key).chain(restore.iter()) {
                        if let Some(i) =
                            newest(&|e| e.scope == *scope && e.key.starts_with(prefix.as_str()))
                        {
                            found = Some(i);
                            break 'scopes;
                        }
                    }
                }
            }
            match found {
                None => {
                    axum::Json(json!({"ok": false, "signed_download_url": "", "matched_key": ""}))
                        .into_response()
                }
                Some(i) => {
                    let e = &mut entries[i];
                    e.accessed = now;
                    let ttl = s.cfg.lock().url_ttl;
                    let se = fmt_time(now + ttl);
                    let url = format!("{}/blob/{}?se={se}&sp=r&sig=fake", s.base, e.id);
                    axum::Json(
                        json!({"ok": true, "signed_download_url": url, "matched_key": e.key}),
                    )
                    .into_response()
                }
            }
        }
    }
}

/// Checks a SAS URL's permission and expiry.
fn sas_ok(q: &HashMap<String, String>, perm: char) -> bool {
    let perm_ok = q.get("sp").is_some_and(|sp| sp.contains(perm));
    let fresh = q
        .get("se")
        .and_then(|se| humantime::parse_rfc3339_weak(se.trim_end_matches('Z')).ok())
        .is_some_and(|t| t > SystemTime::now());
    perm_ok && fresh && q.get("sig").is_some_and(|s| s == "fake")
}

fn inject_failure(s: &Inner) -> bool {
    let n = s.blob_requests.fetch_add(1, Ordering::Relaxed) + 1;
    s.cfg
        .lock()
        .fail_blob_every
        .is_some_and(|every| n % every == 0)
}

async fn blob_put(
    State(s): State<Arc<Inner>>,
    Path(id): Path<i64>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    s.delay().await;
    if inject_failure(&s) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    if !sas_ok(&q, 'w') {
        return StatusCode::FORBIDDEN.into_response();
    }
    let link = s.cfg.lock().upload;
    s.uploads.carry(link, body.len()).await;
    let mut blobs = s.blobs.lock();
    let Some(blob) = blobs.get_mut(&id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match q.get("comp").map(String::as_str) {
        Some("block") => {
            let Some(bid) = q.get("blockid") else {
                return StatusCode::BAD_REQUEST.into_response();
            };
            blob.blocks.insert(bid.clone(), body);
        }
        Some("blocklist") => {
            let xml = String::from_utf8_lossy(&body);
            let mut data = Vec::new();
            for part in xml.split("<Latest>").skip(1) {
                let Some((bid, _)) = part.split_once("</Latest>") else {
                    return StatusCode::BAD_REQUEST.into_response();
                };
                let Some(block) = blob.blocks.get(bid) else {
                    return StatusCode::BAD_REQUEST.into_response();
                };
                data.extend_from_slice(block);
            }
            blob.committed = Some(Bytes::from(data));
            blob.blocks.clear();
        }
        _ => {
            if headers.get("x-ms-blob-type").and_then(|v| v.to_str().ok()) != Some("BlockBlob") {
                return StatusCode::BAD_REQUEST.into_response();
            }
            blob.committed = Some(body);
        }
    }
    StatusCode::CREATED.into_response()
}

async fn blob_get(
    State(s): State<Arc<Inner>>,
    Path(id): Path<i64>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    s.delay().await;
    if inject_failure(&s) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    if !sas_ok(&q, 'r') {
        return StatusCode::FORBIDDEN.into_response();
    }
    let data = {
        let blobs = s.blobs.lock();
        match blobs.get(&id).and_then(|b| b.committed.clone()) {
            Some(d) => d,
            None => return StatusCode::NOT_FOUND.into_response(),
        }
    };
    let total = data.len() as u64;
    let range = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("bytes="))
        .and_then(|v| {
            let (a, b) = v.split_once('-')?;
            Some((a.parse::<u64>().ok()?, b.parse::<u64>().ok()))
        });
    let link = s.cfg.lock().download;
    match range {
        None => {
            s.downloads.carry(link, data.len()).await;
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_LENGTH, total)
                .body(Body::from(data))
                .expect("valid response")
        }
        Some((start, _)) if start >= total => Response::builder()
            .status(StatusCode::RANGE_NOT_SATISFIABLE)
            .header(header::CONTENT_RANGE, format!("bytes */{total}"))
            .body(Body::empty())
            .expect("valid response"),
        Some((start, end)) => {
            let end = end.unwrap_or(total - 1).min(total - 1);
            let body = data.slice(start as usize..=end as usize);
            s.downloads.carry(link, body.len()).await;
            Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header(
                    header::CONTENT_RANGE,
                    format!("bytes {start}-{end}/{total}"),
                )
                .body(Body::from(body))
                .expect("valid response")
        }
    }
}

async fn rest_denied(s: &Inner, headers: &HeaderMap) -> Option<Response> {
    s.delay().await;
    let authed = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("Bearer "));
    if !authed || s.cfg.lock().deny_rest {
        return Some(
            (
                StatusCode::FORBIDDEN,
                axum::Json(json!({"message": "Resource not accessible by integration"})),
            )
                .into_response(),
        );
    }
    let exhausted = s.cfg.lock().rest_exhausted_until;
    if let Some(reset) = exhausted.filter(|t| *t > SystemTime::now()) {
        let reset = reset
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default();
        return Some(
            (
                StatusCode::FORBIDDEN,
                [
                    ("x-ratelimit-remaining", "0".to_string()),
                    ("x-ratelimit-reset", reset.as_secs().to_string()),
                ],
                axum::Json(json!({"message": "API rate limit exceeded"})),
            )
                .into_response(),
        );
    }
    None
}

fn item(e: &Entry) -> Value {
    json!({
        "id": e.id,
        "ref": e.scope,
        "key": e.key,
        "version": e.version,
        "last_accessed_at": fmt_time(e.accessed),
        "created_at": fmt_time(e.created),
        "size_in_bytes": e.size,
    })
}

async fn list(
    State(s): State<Arc<Inner>>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    if let Some(r) = rest_denied(&s, &headers).await {
        return r;
    }
    let per_page: usize = q
        .get("per_page")
        .and_then(|v| v.parse().ok())
        .unwrap_or(30)
        .clamp(1, 100);
    let page: usize = q
        .get("page")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1)
        .max(1);
    let entries = s.entries.lock();
    let mut matching: Vec<&Entry> = entries
        .iter()
        .filter(|e| e.finalized)
        .filter(|e| q.get("ref").is_none_or(|r| *r == e.scope))
        .filter(|e| q.get("key").is_none_or(|k| e.key.starts_with(k.as_str())))
        .collect();
    let by_created = q.get("sort").map(String::as_str) == Some("created_at");
    matching.sort_by_key(|e| (if by_created { e.created } else { e.accessed }, e.id));
    if q.get("direction").map(String::as_str) != Some("asc") {
        matching.reverse();
    }
    let total = matching.len();
    let items: Vec<Value> = matching
        .iter()
        .skip((page - 1) * per_page)
        .take(per_page)
        .map(|e| item(e))
        .collect();
    axum::Json(json!({"total_count": total, "actions_caches": items})).into_response()
}

async fn delete_one(
    State(s): State<Arc<Inner>>,
    Path((_, _, id)): Path<(String, String, i64)>,
    headers: HeaderMap,
) -> Response {
    if let Some(r) = rest_denied(&s, &headers).await {
        return r;
    }
    let mut entries = s.entries.lock();
    let before = entries.len();
    entries.retain(|e| !(e.id == id && e.finalized));
    if entries.len() == before {
        return StatusCode::NOT_FOUND.into_response();
    }
    s.blobs.lock().remove(&id);
    StatusCode::NO_CONTENT.into_response()
}

async fn delete_by_key(
    State(s): State<Arc<Inner>>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    if let Some(r) = rest_denied(&s, &headers).await {
        return r;
    }
    let Some(key) = q.get("key") else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let mut entries = s.entries.lock();
    let (gone, kept): (Vec<Entry>, Vec<Entry>) = entries
        .drain(..)
        .partition(|e| e.finalized && e.key == *key && q.get("ref").is_none_or(|r| *r == e.scope));
    *entries = kept;
    let mut blobs = s.blobs.lock();
    for e in &gone {
        blobs.remove(&e.id);
    }
    let items: Vec<Value> = gone.iter().map(item).collect();
    axum::Json(json!({"total_count": items.len(), "actions_caches": items})).into_response()
}

async fn repo(State(s): State<Arc<Inner>>, headers: HeaderMap) -> Response {
    if let Some(r) = rest_denied(&s, &headers).await {
        return r;
    }
    let default_branch = s.cfg.lock().default_branch.clone();
    axum::Json(json!({"default_branch": default_branch})).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn transfers_share_the_link() {
        let pipe = Pipe::default();
        let shared = Some(Link {
            per_request: 10_000_000,
            total: 1_000_000,
        });
        let t = Instant::now();
        tokio::join!(pipe.carry(shared, 100_000), pipe.carry(shared, 100_000));
        assert!(
            t.elapsed() >= Duration::from_millis(200),
            "{:?}",
            t.elapsed()
        );
        let slow = Some(Link {
            per_request: 1_000_000,
            total: 10_000_000,
        });
        let t = Instant::now();
        pipe.carry(slow, 100_000).await;
        assert!(
            t.elapsed() >= Duration::from_millis(100),
            "{:?}",
            t.elapsed()
        );
    }
}
