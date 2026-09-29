//! Clients for the three services behind the Actions cache: the Twirp cache
//! service (runner token), Azure Blob storage (SAS URLs), and the GitHub REST
//! API (`GITHUB_TOKEN`, for listing and deleting).

pub mod blob;
pub mod rest;
pub mod twirp;

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

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
    #[error("rate limited")]
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

/// Retry policy for transient failures.
#[derive(Debug, Clone, Copy)]
pub struct Retry {
    pub attempts: u32,
    pub base: Duration,
    pub max: Duration,
}

impl Default for Retry {
    fn default() -> Self {
        Retry {
            attempts: 5,
            base: Duration::from_millis(500),
            max: Duration::from_secs(30),
        }
    }
}

impl Retry {
    fn delay(&self, attempt: u32) -> Duration {
        let exp = self.base.saturating_mul(1u32 << attempt.min(16));
        let capped = exp.min(self.max);
        // Up to 25% jitter so parallel retries spread out.
        let jitter = capped.mul_f64((crate::entry::fresh_nonce() % 1000) as f64 / 4000.0);
        capped + jitter
    }
}

/// A process-wide pause, set when a service tells us to back off.
#[derive(Debug, Default)]
pub struct RateGate {
    until: parking_lot::Mutex<Option<Instant>>,
}

impl RateGate {
    pub async fn wait(&self) {
        loop {
            let until = *self.until.lock();
            match until {
                Some(t) if t > Instant::now() => tokio::time::sleep_until(t.into()).await,
                _ => return,
            }
        }
    }

    fn pause(&self, d: Duration) {
        let t = Instant::now() + d.min(Duration::from_secs(600));
        let mut until = self.until.lock();
        if until.is_none_or(|u| u < t) {
            *until = Some(t);
        }
    }
}

/// Shared HTTP plumbing.
#[derive(Clone, Debug)]
pub struct Http {
    pub client: reqwest::Client,
    pub gate: Arc<RateGate>,
    pub retry: Retry,
}

impl Http {
    pub fn new(retry: Retry) -> anyhow::Result<Http> {
        install_crypto_provider();
        let client = reqwest::Client::builder()
            .user_agent(concat!("gha-cache-fusefs/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(30))
            .pool_max_idle_per_host(64)
            .build()?;
        Ok(Http {
            client,
            gate: Arc::default(),
            retry,
        })
    }

    /// Runs `f` until it succeeds, fails permanently, or runs out of attempts.
    pub async fn retrying<T, F, Fut>(&self, what: &str, mut f: F) -> Result<T, ApiError>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, ApiError>>,
    {
        let mut attempt = 0;
        loop {
            self.gate.wait().await;
            let err = match f().await {
                Ok(v) => return Ok(v),
                Err(e) => e,
            };
            if !err.is_transient() || attempt + 1 >= self.retry.attempts {
                return Err(err);
            }
            let delay = match err {
                ApiError::RateLimited(Some(d)) => {
                    self.gate.pause(d);
                    d
                }
                ApiError::RateLimited(None) => {
                    let d = self.retry.delay(attempt + 3);
                    self.gate.pause(d);
                    d
                }
                _ => self.retry.delay(attempt),
            };
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
}

impl Api {
    pub fn new(env: &Env) -> anyhow::Result<Api> {
        let http = Http::new(Retry::default())?;
        Ok(Api {
            twirp: Twirp::new(http.clone(), &env.results_url, &env.runtime_token)?,
            blob: Blob::new(http.clone()),
            rest: Rest::new(
                http,
                &env.api_url,
                &env.repository,
                env.github_token.as_deref(),
            ),
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
        };
        assert!(r.delay(0) >= Duration::from_millis(100));
        assert!(r.delay(0) < Duration::from_millis(126));
        assert!(r.delay(3) >= Duration::from_millis(800));
        assert!(r.delay(9) <= Duration::from_millis(1250));
    }
}
