//! Generic HTTP backend.
//!
//! Stores the ciphertext blob by issuing `PUT {url}/{blob_name}` (to upload)
//! and `GET {url}/{blob_name}` (to download). The body is the raw ciphertext
//! bytes — no JSON wrapping, no base64. The server is expected to store the
//! body verbatim.
//!
//! ## Use case
//!
//! Self-hosted cloud storage that exposes a single file via HTTP (e.g. a
//! Nextcloud WebDAV endpoint wrapped in a tiny proxy, a custom S3-like
//! service, or a simple file server with PUT support). This is the "other
//! online encrypted library" backend from the user's requirements.
//!
//! ## Authentication
//!
//! Optional bearer token, sent as `Authorization: Bearer {token}`. The
//! caller is responsible for ensuring the transport is HTTPS — we do not
//! allow tokens over plain HTTP in production (enforced only via the `debug`
//! feature in real codebases; here we trust the caller).
//!
//! ## 404 handling
//!
//! A `GET` that returns `404` is interpreted as "remote is empty" and yields
//! `Ok(None)`. Any other non-success status is an error.

use async_trait::async_trait;

use crate::backend::EncryptedSyncBackend;
use crate::config::HttpConfig;
use crate::error::{Result, SyncError};

/// A backend that stores the blob by issuing `PUT`/`GET` against a generic
/// HTTP endpoint.
pub struct HttpBackend {
    client: reqwest::Client,
    config: HttpConfig,
    blob_name: String,
}

impl HttpBackend {
    pub fn new(config: HttpConfig, blob_name: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            config,
            blob_name: blob_name.into(),
        }
    }

    fn blob_url(&self) -> String {
        let base = self.config.url.trim_end_matches('/');
        format!("{base}/{}", self.blob_name)
    }
}

#[async_trait]
impl EncryptedSyncBackend for HttpBackend {
    fn name(&self) -> &'static str {
        "http"
    }

    async fn push(&self, data: &[u8]) -> Result<()> {
        let url = self.blob_url();
        let mut req = self
            .client
            .put(&url)
            .header("Content-Type", "application/octet-stream")
            .body(data.to_vec());
        if let Some(token) = &self.config.token {
            req = req.bearer_auth(token.expect_inline());
        }
        let resp = req.send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(SyncError::HttpStatus {
                status: status.as_u16(),
                url,
                body,
            });
        }
        tracing::debug!(url = %url, bytes = data.len(), "http: pushed blob");
        Ok(())
    }

    async fn pull(&self) -> Result<Option<Vec<u8>>> {
        let url = self.blob_url();
        let mut req = self.client.get(&url);
        if let Some(token) = &self.config.token {
            req = req.bearer_auth(token.expect_inline());
        }
        let resp = req.send().await?;
        let status = resp.status();
        if status.as_u16() == 404 {
            return Ok(None);
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(SyncError::HttpStatus {
                status: status.as_u16(),
                url,
                body,
            });
        }
        let bytes = resp.bytes().await?.to_vec();
        tracing::debug!(url = %url, bytes = bytes.len(), "http: pulled blob");
        Ok(Some(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_url_joins_correctly() {
        let cfg = HttpConfig {
            url: "https://example.com/sync".into(),
            token: None,
        };
        let backend = HttpBackend::new(cfg, "blob.bin");
        assert_eq!(backend.blob_url(), "https://example.com/sync/blob.bin");
    }

    #[test]
    fn blob_url_strips_trailing_slash() {
        let cfg = HttpConfig {
            url: "https://example.com/sync/".into(),
            token: None,
        };
        let backend = HttpBackend::new(cfg, "blob.bin");
        assert_eq!(backend.blob_url(), "https://example.com/sync/blob.bin");
    }

    // Note: a full push→pull round-trip test against a real HTTP endpoint is
    // intentionally left as an integration-test concern. The in-process toy
    // HTTP server we tried here was too brittle to be worth maintaining;
    // the backend's logic is straightforward PUT/GET with bearer auth and
    // 404 handling, which is best validated against a real or mock server.
}
