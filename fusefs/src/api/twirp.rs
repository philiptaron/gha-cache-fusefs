//! The Twirp/JSON cache service reachable with `ACTIONS_RUNTIME_TOKEN`.

use std::sync::atomic::Ordering;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};

use super::{ApiError, Http, retry_after};

const SERVICE: &str = "twirp/github.actions.results.api.v1.CacheService/";
const WRITE_DENIED: &str = "cache write denied:";
const READ_DENIED: &str = "cache read denied:";

/// One client per method, each with a gate of its own: creating entries is
/// rate limited, and waiting for that must not hold up downloads.
#[derive(Clone)]
pub struct Twirp {
    create: Http,
    finalize: Http,
    lookup: Http,
    base: String,
    token: String,
}

impl std::fmt::Debug for Twirp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Twirp").field("base", &self.base).finish()
    }
}

#[derive(Serialize)]
struct CreateReq<'a> {
    key: &'a str,
    version: &'a str,
}

#[derive(Deserialize)]
struct CreateResp {
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    signed_upload_url: String,
    #[serde(default)]
    message: String,
}

#[derive(Serialize)]
struct FinalizeReq<'a> {
    key: &'a str,
    version: &'a str,
    size_bytes: String,
}

#[derive(Deserialize)]
struct FinalizeResp {
    #[serde(default)]
    ok: bool,
    #[serde(default, deserialize_with = "int64")]
    entry_id: i64,
    #[serde(default)]
    message: String,
}

#[derive(Serialize)]
struct GetReq<'a> {
    key: &'a str,
    version: &'a str,
    restore_keys: &'a [String],
}

#[derive(Deserialize)]
struct GetResp {
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    signed_download_url: String,
    #[serde(default)]
    matched_key: String,
}

#[derive(Deserialize, Default)]
struct TwirpError {
    #[serde(default)]
    code: String,
    #[serde(default)]
    msg: String,
}

/// protojson encodes int64 as a string; accept numbers too.
fn int64<'de, D: Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Int {
        S(String),
        N(i64),
    }
    match Int::deserialize(d)? {
        Int::N(n) => Ok(n),
        Int::S(s) => s.parse().map_err(serde::de::Error::custom),
    }
}

impl Twirp {
    pub fn new(http: Http, results_url: &str, token: &str) -> anyhow::Result<Twirp> {
        // The toolkit resolves `/twirp/...` against the results URL, which
        // keeps only its origin.
        let url = url::Url::parse(results_url)?;
        let base = url.join(&format!("/{SERVICE}"))?.to_string();
        Ok(Twirp {
            create: http.gated(http.retry),
            finalize: http.gated(http.retry),
            lookup: http,
            base,
            token: token.to_string(),
        })
    }

    async fn call<Req: Serialize, Resp: DeserializeOwned>(
        &self,
        http: &Http,
        method: &str,
        req: &Req,
    ) -> Result<Resp, ApiError> {
        let url = format!("{}{method}", self.base);
        http.retrying(method, || async {
            http.stats.twirp.fetch_add(1, Ordering::Relaxed);
            let resp = http
                .client
                .post(&url)
                .bearer_auth(&self.token)
                .json(req)
                .timeout(Duration::from_secs(60))
                .send()
                .await
                .map_err(ApiError::transport)?;
            let status = resp.status();
            let after = retry_after(resp.headers());
            let body = resp.bytes().await.map_err(ApiError::transport)?;
            if status.is_success() {
                return serde_json::from_slice(&body)
                    .map_err(|e| ApiError::Server(format!("{method}: bad response: {e}")));
            }
            let err: TwirpError = serde_json::from_slice(&body).unwrap_or_else(|_| TwirpError {
                code: String::new(),
                msg: String::from_utf8_lossy(&body).chars().take(200).collect(),
            });
            Err(classify(status.as_u16(), &err.code, &err.msg, after))
        })
        .await
    }

    /// Reserves `(key, version)` and returns the SAS URL to upload the blob to.
    pub async fn create(&self, key: &str, version: &str) -> Result<String, ApiError> {
        let resp: CreateResp = self
            .call(
                &self.create,
                "CreateCacheEntry",
                &CreateReq { key, version },
            )
            .await?;
        if !resp.ok || resp.signed_upload_url.is_empty() {
            return Err(if resp.message.starts_with(WRITE_DENIED) {
                ApiError::Denied(resp.message)
            } else if resp.message.is_empty() {
                ApiError::AlreadyExists
            } else {
                ApiError::Invalid(resp.message)
            });
        }
        Ok(resp.signed_upload_url)
    }

    /// Commits an uploaded blob, returning the entry id.
    pub async fn finalize(&self, key: &str, version: &str, size: u64) -> Result<i64, ApiError> {
        let resp: FinalizeResp = self
            .call(
                &self.finalize,
                "FinalizeCacheEntryUpload",
                &FinalizeReq {
                    key,
                    version,
                    size_bytes: size.to_string(),
                },
            )
            .await?;
        if !resp.ok {
            return Err(ApiError::Invalid(if resp.message.is_empty() {
                "finalize was not ok".into()
            } else {
                resp.message
            }));
        }
        Ok(resp.entry_id)
    }

    /// Resolves the entry with exactly this key and version to a download URL.
    ///
    /// The service falls back to prefix matches on the key, so the matched key
    /// is checked; `None` means there is no such entry.
    pub async fn download_url(&self, key: &str, version: &str) -> Result<Option<String>, ApiError> {
        let resp: GetResp = self
            .call(
                &self.lookup,
                "GetCacheEntryDownloadURL",
                &GetReq {
                    key,
                    version,
                    restore_keys: &[],
                },
            )
            .await?;
        if !resp.ok || resp.matched_key != key || resp.signed_download_url.is_empty() {
            return Ok(None);
        }
        Ok(Some(resp.signed_download_url))
    }
}

fn classify(status: u16, code: &str, msg: &str, after: Option<Duration>) -> ApiError {
    let msg = msg.to_string();
    if msg.contains("insufficient usage") {
        return ApiError::Denied(format!("cache storage quota exceeded: {msg}"));
    }
    if msg.starts_with(WRITE_DENIED) || msg.contains(READ_DENIED) {
        return ApiError::Denied(msg);
    }
    match (status, code) {
        (429, _) | (_, "resource_exhausted") => ApiError::RateLimited(after),
        (409, _) | (_, "already_exists") => ApiError::AlreadyExists,
        (_, "bad_route") => ApiError::Invalid(msg),
        (404, _) | (_, "not_found") => ApiError::NotFound,
        (401 | 403, _) | (_, "permission_denied" | "unauthenticated") => ApiError::Denied(msg),
        (500.., _) | (_, "internal" | "unavailable" | "unknown") => ApiError::Server(msg),
        _ => ApiError::Invalid(msg),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_observed_errors() {
        assert_eq!(
            classify(
                409,
                "already_exists",
                "cache entry with the same key, version, and scope already exists",
                None
            ),
            ApiError::AlreadyExists
        );
        assert_eq!(
            classify(404, "not_found", "cache entry not found", None),
            ApiError::NotFound
        );
        assert!(matches!(
            classify(404, "bad_route", "no handler for path", None),
            ApiError::Invalid(_)
        ));
        assert!(matches!(
            classify(
                400,
                "invalid_argument",
                "size invalid cache size value",
                None
            ),
            ApiError::Invalid(_)
        ));
        assert!(matches!(
            classify(
                403,
                "permission_denied",
                "cache write denied: read-only token",
                None
            ),
            ApiError::Denied(_)
        ));
        assert_eq!(
            classify(429, "", "slow down", Some(Duration::from_secs(3))),
            ApiError::RateLimited(Some(Duration::from_secs(3)))
        );
        assert!(classify(503, "", "", None).is_transient());
    }

    #[test]
    fn entry_ids_accept_strings_and_numbers() {
        let a: FinalizeResp =
            serde_json::from_str(r#"{"ok":true,"entry_id":"8271625970"}"#).unwrap();
        let b: FinalizeResp = serde_json::from_str(r#"{"ok":true,"entry_id":8271625970}"#).unwrap();
        assert_eq!(a.entry_id, 8271625970);
        assert_eq!(b.entry_id, 8271625970);
    }

    #[test]
    fn base_url_keeps_only_the_origin() {
        let http = Http::new(Default::default()).unwrap();
        let t = Twirp::new(
            http,
            "https://results-receiver.actions.githubusercontent.com/some/path",
            "t",
        )
        .unwrap();
        assert_eq!(
            t.base,
            "https://results-receiver.actions.githubusercontent.com/twirp/github.actions.results.api.v1.CacheService/"
        );
    }
}
