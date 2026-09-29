//! Clients for the three services behind the Actions cache: the Twirp cache
//! service (runner token), Azure Blob storage (SAS URLs), and the GitHub REST
//! API (`GITHUB_TOKEN`, for listing and deleting).

pub mod blob;
pub mod rest;
pub mod twirp;

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

pub use blob::Blob;
pub use rest::{CacheItem, Rest};
pub use twirp::Twirp;

use crate::config::Env;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApiError {
    #[error("not found")]
    NotFound,
    #[error("already exists")]
    AlreadyExists,
    #[error("permission denied: {0}")]
    Denied(String),
    #[error("invalid request: {0}")]
    Invalid(String),
    #[error("rate limited{}", retry_hint(.0))]
    RateLimited(Option<Duration>),
    #[error("server error: {0}")]
    Server(String),
    #[error("transport error: {0}")]
    Transport(String),
    /// A SAS URL was rejected, typically because it expired.
    #[error("signed URL rejected (expired?)")]
    Expired,
}

impl ApiError {
    /// Whether retrying the same request may succeed.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            ApiError::RateLimited(_) | ApiError::Server(_) | ApiError::Transport(_)
        )
    }

    fn transport(e: reqwest::Error) -> ApiError {
        // `without_url` keeps SAS signatures out of error messages and logs.
        ApiError::Transport(e.without_url().to_string())
    }
}

fn retry_hint(wait: &Option<Duration>) -> String {
    match wait {
        Some(d) => {
            let d = Duration::from_secs(d.as_secs().max(1));
            format!("; try again in {}", humantime::format_duration(d))
        }
        None => String::new(),
    }
}

/// Retry policy for transient failures.
#[derive(Debug, Clone, Copy)]
pub struct Retry {
    pub attempts: u32,
    pub base: Duration,
    pub max: Duration,
    /// The longest a request waits out a rate limit before it fails instead.
    pub max_wait: Duration,
}

impl Default for Retry {
    fn default() -> Self {
        Retry {
            attempts: 5,
            base: Duration::from_millis(500),
            max: Duration::from_secs(30),
            max_wait: Duration::from_secs(600),
        }
    }
}

impl Retry {
    /// For the REST API, whose exhausted budget can take an hour to reset:
    /// a mount would rather fail, and a refresh rather be skipped, than wait.
    pub fn rest() -> Retry {
        Retry {
            max_wait: Duration::from_secs(60),
            ..Retry::default()
        }
    }

    fn delay(&self, attempt: u32) -> Duration {
        let exp = self.base.saturating_mul(1u32 << attempt.min(16));
        let capped = exp.min(self.max);
        // Up to 25% jitter so parallel retries spread out.
        let jitter = capped.mul_f64((crate::entry::fresh_nonce() % 1000) as f64 / 4000.0);
        capped + jitter
    }
}

/// A pause, set when a service tells us to back off.
#[derive(Debug, Default)]
pub struct RateGate {
    until: parking_lot::Mutex<Option<Instant>>,
}

impl RateGate {
    /// How long the gate stays closed.
    fn remaining(&self) -> Option<Duration> {
        let until = (*self.until.lock())?;
        until.checked_duration_since(Instant::now())
    }

    pub async fn wait(&self) {
        loop {
            let until = *self.until.lock();
            match until {
                Some(t) if t > Instant::now() => tokio::time::sleep_until(t.into()).await,
                _ => return,
            }
        }
    }

    /// Closes the gate for `d` from now; returns how much that extended the pause.
    fn pause(&self, d: Duration) -> Duration {
        let now = Instant::now();
        let t = now + d.min(Duration::from_secs(600));
        let mut until = self.until.lock();
        let from = until.map_or(now, |u| u.max(now));
        if t <= from {
            return Duration::ZERO;
        }
        *until = Some(t);
        t - from
    }
}

/// Request counts per service, and what rate limiting cost.
#[derive(Debug, Default)]
pub struct ApiStats {
    pub twirp: AtomicU64,
    pub blob: AtomicU64,
    pub rest: AtomicU64,
    pub rate_limited: AtomicU64,
    /// How long the rate gates were closed, summed over the gates.
    pub paused_ms: AtomicU64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Requests {
    /// The Twirp cache service.
    pub cache_service: u64,
    /// Azure Blob storage.
    pub blob: u64,
    /// The GitHub REST API, whose `GITHUB_TOKEN` budget is shared by the repository.
    pub rest: u64,
}

impl ApiStats {
    pub fn requests(&self) -> Requests {
        Requests {
            cache_service: self.twirp.load(Ordering::Relaxed),
            blob: self.blob.load(Ordering::Relaxed),
            rest: self.rest.load(Ordering::Relaxed),
        }
    }
}

/// Shared HTTP plumbing.
#[derive(Clone, Debug)]
pub struct Http {
    pub client: reqwest::Client,
    /// Closed while the service behind this client asks us to back off.
    pub gate: Arc<RateGate>,
    pub retry: Retry,
    pub stats: Arc<ApiStats>,
}

impl Http {
    pub fn new(retry: Retry) -> anyhow::Result<Http> {
        install_crypto_provider();
        let client = reqwest::Client::builder()
            .user_agent(concat!("gha-cache-fusefs/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(30))
            .pool_max_idle_per_host(64)
            .tls_certs_only(root_certificates())
            .build()?;
        Ok(Http {
            client,
            gate: Arc::default(),
            retry,
            stats: Arc::default(),
        })
    }

    /// The same connections and statistics with a gate of its own, so that
    /// one service's rate limit pauses only its own requests.
    pub fn gated(&self, retry: Retry) -> Http {
        Http {
            gate: Arc::default(),
            retry,
            ..self.clone()
        }
    }

    /// Runs `f` until it succeeds, fails permanently, or runs out of attempts.
    pub async fn retrying<T, F, Fut>(&self, what: &str, mut f: F) -> Result<T, ApiError>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, ApiError>>,
    {
        let mut attempt = 0;
        loop {
            // Rather than wait longer than `max_wait`, fail now.
            if let Some(left) = self.gate.remaining().filter(|&l| l > self.retry.max_wait) {
                return Err(ApiError::RateLimited(Some(left)));
            }
            self.gate.wait().await;
            let err = match f().await {
                Ok(v) => return Ok(v),
                Err(e) => e,
            };
            let delay = match err {
                ApiError::RateLimited(d) => {
                    self.stats.rate_limited.fetch_add(1, Ordering::Relaxed);
                    // Close the gate even when giving up, so that the next
                    // request waits too, or fails at once.
                    let d = d.unwrap_or_else(|| self.retry.delay(attempt + 3));
                    let paused = self.gate.pause(d).as_millis() as u64;
                    self.stats.paused_ms.fetch_add(paused, Ordering::Relaxed);
                    if d > self.retry.max_wait {
                        return Err(err);
                    }
                    d
                }
                _ => self.retry.delay(attempt),
            };
            if !err.is_transient() || attempt + 1 >= self.retry.attempts {
                return Err(err);
            }
            tracing::debug!(%err, ?delay, attempt, "{what}: retrying");
            tokio::time::sleep(delay).await;
            attempt += 1;
        }
    }
}

fn install_crypto_provider() {
    // Fails harmlessly if a provider is already installed.
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Mozilla's roots, so a static binary works where the system has none (a
/// minimal container, the Nix sandbox), plus the system's, so custom CAs (GHES,
/// TLS-inspecting proxies) keep working.
fn root_certificates() -> Vec<reqwest::Certificate> {
    let system = rustls_native_certs::load_native_certs().certs;
    webpki_root_certs::TLS_SERVER_ROOT_CERTS
        .iter()
        .map(|der| der.as_ref())
        .chain(system.iter().map(|der| der.as_ref()))
        .filter_map(|der| reqwest::Certificate::from_der(der).ok())
        .collect()
}

fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let v = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    v.trim().parse::<u64>().ok().map(Duration::from_secs)
}

/// Replaces SAS signatures in a URL, for logging.
pub fn redact(url: &str) -> String {
    match url.find("sig=") {
        None => url.to_string(),
        Some(i) => {
            let end = url[i..].find('&').map_or(url.len(), |j| i + j);
            format!("{}sig=REDACTED{}", &url[..i], &url[end..])
        }
    }
}

/// Everything the filesystem needs to talk to the cache.
#[derive(Clone, Debug)]
pub struct Api {
    pub twirp: Twirp,
    pub blob: Blob,
    pub rest: Rest,
    pub stats: Arc<ApiStats>,
}

impl Api {
    pub fn new(env: &Env) -> anyhow::Result<Api> {
        let http = Http::new(Retry::default())?;
        let twirp = http.gated(Retry::default());
        Ok(Api {
            twirp: Twirp::new(twirp, &env.results_url, &env.runtime_token)?,
            blob: Blob::new(http.gated(Retry::default())),
            rest: Rest::new(
                http.gated(Retry::rest()),
                &env.api_url,
                &env.repository,
                env.github_token.as_deref(),
            ),
            stats: http.stats,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_signatures() {
        assert_eq!(
            redact("https://x/y?se=1&sig=abc%2B&sp=r"),
            "https://x/y?se=1&sig=REDACTED&sp=r"
        );
        assert_eq!(redact("https://x/y?sig=abc"), "https://x/y?sig=REDACTED");
        assert_eq!(redact("https://x/y?a=b"), "https://x/y?a=b");
    }

    #[test]
    fn delays_grow_and_cap() {
        let r = Retry {
            attempts: 10,
            base: Duration::from_millis(100),
            max: Duration::from_secs(1),
            ..Retry::default()
        };
        assert!(r.delay(0) >= Duration::from_millis(100));
        assert!(r.delay(0) < Duration::from_millis(126));
        assert!(r.delay(3) >= Duration::from_millis(800));
        assert!(r.delay(9) <= Duration::from_millis(1250));
    }

    #[tokio::test]
    async fn long_rate_limits_fail_fast_and_pause_only_their_service() {
        let http = Http::new(Retry::default()).unwrap();
        let rest = http.gated(Retry::rest());
        let other = http.gated(Retry::default());
        let hour = Duration::from_secs(3600);
        let t = Instant::now();
        let exhausted = || async { Err::<(), _>(ApiError::RateLimited(Some(hour))) };
        assert_eq!(
            rest.retrying("list", exhausted).await,
            Err(ApiError::RateLimited(Some(hour)))
        );
        // While the gate is closed, later requests fail without being sent.
        let err = rest
            .retrying("list", || async { Ok(()) })
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::RateLimited(Some(d)) if d > Duration::from_secs(500)));
        assert!(t.elapsed() < Duration::from_secs(1));
        // Other services go on.
        other.retrying("get", || async { Ok(()) }).await.unwrap();
        assert_eq!(http.stats.rate_limited.load(Ordering::Relaxed), 1);
        assert!(http.stats.paused_ms.load(Ordering::Relaxed) >= 599_000);
    }
}
