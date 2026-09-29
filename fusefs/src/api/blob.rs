//! Azure Blob storage, addressed through SAS URLs handed out by the cache service.

use std::time::Duration;

use base64::Engine;
use bytes::Bytes;
use reqwest::StatusCode;
use reqwest::header::{CONTENT_RANGE, RANGE};

use super::{ApiError, Http, retry_after};

#[derive(Clone, Debug)]
pub struct Blob {
    http: Http,
}

/// A generous per-request timeout: the minimum plus 1 s per 256 KiB.
fn transfer_timeout(bytes: u64) -> Duration {
    Duration::from_secs(60 + bytes / (256 * 1024))
}

fn with_query(sas: &str, pairs: &[(&str, &str)]) -> Result<url::Url, ApiError> {
    let mut url =
        url::Url::parse(sas).map_err(|e| ApiError::Invalid(format!("bad SAS URL: {e}")))?;
    {
        let mut q = url.query_pairs_mut();
        for (k, v) in pairs {
            q.append_pair(k, v);
        }
    }
    Ok(url)
}

/// Block ids must all have the same length; use the zero-padded index.
pub fn block_id(index: usize) -> String {
    base64::engine::general_purpose::STANDARD.encode(format!("{index:08}"))
}

fn classify(status: StatusCode, headers: &reqwest::header::HeaderMap, what: &str) -> ApiError {
    match status.as_u16() {
        403 => ApiError::Expired,
        404 => ApiError::NotFound,
        416 => ApiError::Invalid(format!("{what}: range not satisfiable")),
        429 => ApiError::RateLimited(retry_after(headers)),
        500.. => ApiError::Server(format!("{what}: HTTP {status}")),
        _ => ApiError::Invalid(format!("{what}: HTTP {status}")),
    }
}

impl Blob {
    pub fn new(http: Http) -> Blob {
        Blob { http }
    }

    async fn put(
        &self,
        url: url::Url,
        body: Bytes,
        what: &str,
        blob_type: bool,
    ) -> Result<(), ApiError> {
        let len = body.len() as u64;
        self.http
            .retrying(what, || async {
                let mut req = self
                    .http
                    .client
                    .put(url.clone())
                    .timeout(transfer_timeout(len))
                    .body(body.clone());
                if blob_type {
                    req = req.header("x-ms-blob-type", "BlockBlob");
                }
                let resp = req.send().await.map_err(ApiError::transport)?;
                let status = resp.status();
                if status.is_success() {
                    return Ok(());
                }
                Err(classify(status, resp.headers(), what))
            })
            .await
    }

    /// Uploads a whole blob in one request.
    pub async fn put_blob(&self, sas: &str, body: Bytes) -> Result<(), ApiError> {
        self.put(with_query(sas, &[])?, body, "Put Blob", true)
            .await
    }

    /// Stages one block of a block blob.
    pub async fn put_block(&self, sas: &str, id: &str, body: Bytes) -> Result<(), ApiError> {
        let url = with_query(sas, &[("comp", "block"), ("blockid", id)])?;
        self.put(url, body, "Put Block", false).await
    }

    /// Commits the staged blocks, in order.
    pub async fn put_block_list(&self, sas: &str, ids: &[String]) -> Result<(), ApiError> {
        let mut xml = String::from(r#"<?xml version="1.0" encoding="utf-8"?><BlockList>"#);
        for id in ids {
            xml.push_str("<Latest>");
            xml.push_str(id);
            xml.push_str("</Latest>");
        }
        xml.push_str("</BlockList>");
        let url = with_query(sas, &[("comp", "blocklist")])?;
        self.put(url, Bytes::from(xml), "Put Block List", false)
            .await
    }

    /// Reads `len` bytes at `offset`. Fewer bytes come back only at the end of the blob.
    pub async fn get_range(&self, sas: &str, offset: u64, len: u64) -> Result<Bytes, ApiError> {
        let url = with_query(sas, &[])?;
        let range = format!("bytes={}-{}", offset, offset + len - 1);
        self.http
            .retrying("Get Blob", || async {
                let resp = self
                    .http
                    .client
                    .get(url.clone())
                    .header(RANGE, &range)
                    .timeout(transfer_timeout(len))
                    .send()
                    .await
                    .map_err(ApiError::transport)?;
                let status = resp.status();
                match status {
                    StatusCode::PARTIAL_CONTENT => {
                        let start = resp
                            .headers()
                            .get(CONTENT_RANGE)
                            .and_then(|v| v.to_str().ok())
                            .and_then(|v| v.strip_prefix("bytes "))
                            .and_then(|v| v.split('-').next())
                            .and_then(|v| v.parse::<u64>().ok());
                        if start != Some(offset) {
                            return Err(ApiError::Server(format!(
                                "Get Blob: asked for offset {offset}, got {start:?}"
                            )));
                        }
                        let body = resp.bytes().await.map_err(ApiError::transport)?;
                        if body.len() as u64 > len {
                            return Ok(body.slice(..len as usize));
                        }
                        Ok(body)
                    }
                    // The server ignored the range and sent everything.
                    StatusCode::OK => {
                        let body = resp.bytes().await.map_err(ApiError::transport)?;
                        let start = (offset as usize).min(body.len());
                        let end = (offset.saturating_add(len) as usize).min(body.len());
                        Ok(body.slice(start..end))
                    }
                    _ => Err(classify(status, resp.headers(), "Get Blob")),
                }
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_ids_have_equal_length() {
        assert_eq!(block_id(0), "MDAwMDAwMDA=");
        assert_eq!(block_id(0).len(), block_id(49_999).len());
    }

    #[test]
    fn query_pairs_are_appended_and_encoded() {
        let url = with_query(
            "https://a.blob/x/1?sv=2025&sig=a%2Bb",
            &[("comp", "block"), ("blockid", "MDA+/=")],
        )
        .unwrap();
        assert_eq!(
            url.as_str(),
            "https://a.blob/x/1?sv=2025&sig=a%2Bb&comp=block&blockid=MDA%2B%2F%3D"
        );
    }
}
