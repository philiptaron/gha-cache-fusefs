//! The GitHub REST API: the only way to list and delete cache entries.

use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime};

use reqwest::StatusCode;
use serde::Deserialize;

use super::{ApiError, Http, retry_after};

pub const PER_PAGE: usize = 100;

#[derive(Clone)]
pub struct Rest {
    http: Http,
    api: String,
    repo: String,
    token: Option<String>,
}

impl std::fmt::Debug for Rest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rest")
            .field("api", &self.api)
            .field("repo", &self.repo)
            .finish()
    }
}

/// One entry of `GET /repos/{owner}/{repo}/actions/caches`.
#[derive(Clone, Debug, Deserialize, serde::Serialize, PartialEq, Eq)]
pub struct CacheItem {
    pub id: i64,
    #[serde(rename = "ref")]
    pub git_ref: String,
    pub key: String,
    pub version: String,
    pub size_in_bytes: u64,
    pub created_at: String,
    #[serde(default)]
    pub last_accessed_at: String,
}

impl CacheItem {
    pub fn created(&self) -> SystemTime {
        parse_time(&self.created_at).unwrap_or(SystemTime::UNIX_EPOCH)
    }

    /// When the entry was last downloaded, or else created.
    pub fn accessed(&self) -> SystemTime {
        parse_time(&self.last_accessed_at).unwrap_or_else(|| self.created())
    }
}

fn parse_time(t: &str) -> Option<SystemTime> {
    humantime::parse_rfc3339_weak(t.trim_end_matches('Z'))
        .or_else(|_| humantime::parse_rfc3339(t))
        .ok()
}

#[derive(Deserialize)]
struct ListResp {
    #[serde(default)]
    total_count: u64,
    #[serde(default)]
    actions_caches: Vec<CacheItem>,
}

#[derive(Deserialize)]
struct RepoResp {
    default_branch: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Asc,
    Desc,
}

impl Rest {
    pub fn new(http: Http, api: &str, repo: &str, token: Option<&str>) -> Rest {
        Rest {
            http,
            api: api.trim_end_matches('/').to_string(),
            repo: repo.to_string(),
            token: token.map(str::to_string),
        }
    }

    fn request(
        &self,
        method: reqwest::Method,
        path: &str,
    ) -> Result<reqwest::RequestBuilder, ApiError> {
        let token = self.token.as_deref().ok_or_else(|| {
            ApiError::Denied(
                "no GITHUB_TOKEN; listing the cache needs one with `actions: read`".into(),
            )
        })?;
        Ok(self
            .http
            .client
            .request(method, format!("{}/repos/{}{path}", self.api, self.repo))
            .bearer_auth(token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .timeout(Duration::from_secs(60)))
    }

    async fn send(
        &self,
        req: reqwest::RequestBuilder,
        what: &str,
    ) -> Result<bytes::Bytes, ApiError> {
        self.http.stats.rest.fetch_add(1, Ordering::Relaxed);
        let resp = req.send().await.map_err(ApiError::transport)?;
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = resp.bytes().await.map_err(ApiError::transport)?;
        if status.is_success() {
            return Ok(body);
        }
        let text: String = String::from_utf8_lossy(&body).chars().take(300).collect();
        let exhausted = headers
            .get("x-ratelimit-remaining")
            .and_then(|v| v.to_str().ok())
            == Some("0");
        Err(match status {
            StatusCode::TOO_MANY_REQUESTS => ApiError::RateLimited(retry_after(&headers)),
            StatusCode::FORBIDDEN if exhausted => {
                let reset = headers
                    .get("x-ratelimit-reset")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .map(|t| SystemTime::UNIX_EPOCH + Duration::from_secs(t))
                    .and_then(|t| t.duration_since(SystemTime::now()).ok());
                ApiError::RateLimited(reset.or(retry_after(&headers)))
            }
            StatusCode::NOT_FOUND => ApiError::NotFound,
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                ApiError::Denied(format!("{what}: HTTP {status}: {text}"))
            }
            s if s.is_server_error() => ApiError::Server(format!("{what}: HTTP {status}")),
            _ => ApiError::Invalid(format!("{what}: HTTP {status}: {text}")),
        })
    }

    /// One page of entries whose key starts with `prefix`, in `git_ref`,
    /// sorted by creation time. Returns the total count and the page.
    pub async fn list_page(
        &self,
        prefix: &str,
        git_ref: &str,
        direction: Direction,
        page: usize,
    ) -> Result<(u64, Vec<CacheItem>), ApiError> {
        let mut query = vec![
            ("ref", git_ref.to_string()),
            ("sort", "created_at".to_string()),
            (
                "direction",
                match direction {
                    Direction::Asc => "asc",
                    Direction::Desc => "desc",
                }
                .to_string(),
            ),
            ("per_page", PER_PAGE.to_string()),
            ("page", page.to_string()),
        ];
        if !prefix.is_empty() {
            query.push(("key", prefix.to_string()));
        }
        let body = self
            .http
            .retrying("list caches", || async {
                let req = self
                    .request(reqwest::Method::GET, "/actions/caches")?
                    .query(&query);
                self.send(req, "list caches").await
            })
            .await?;
        let resp: ListResp = serde_json::from_slice(&body)
            .map_err(|e| ApiError::Server(format!("list caches: bad response: {e}")))?;
        Ok((resp.total_count, resp.actions_caches))
    }

    pub async fn delete(&self, id: i64) -> Result<(), ApiError> {
        self.http
            .retrying("delete cache", || async {
                let req =
                    self.request(reqwest::Method::DELETE, &format!("/actions/caches/{id}"))?;
                self.send(req, "delete cache").await.map(drop)
            })
            .await
    }

    pub async fn default_branch(&self) -> Result<String, ApiError> {
        let body = self
            .http
            .retrying("get repository", || async {
                self.send(self.request(reqwest::Method::GET, "")?, "get repository")
                    .await
            })
            .await?;
        let repo: RepoResp = serde_json::from_slice(&body)
            .map_err(|e| ApiError::Server(format!("get repository: bad response: {e}")))?;
        Ok(repo.default_branch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_listing_timestamps() {
        let item = CacheItem {
            id: 1,
            git_ref: "refs/heads/main".into(),
            key: "k".into(),
            version: "v".into(),
            size_in_bytes: 1,
            created_at: "2026-09-29T15:54:34.175138Z".into(),
            last_accessed_at: String::new(),
        };
        let t = item.created();
        let d = t.duration_since(SystemTime::UNIX_EPOCH).unwrap();
        assert_eq!(d.as_secs(), 1790697274);
        assert_eq!(d.subsec_micros(), 175138);
    }
}
