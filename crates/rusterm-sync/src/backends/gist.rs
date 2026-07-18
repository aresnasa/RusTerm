//! GitHub Gist backend.
//!
//! Uses the GitHub Gist REST API to store the ciphertext blob as a single
//! file inside a gist. The gist can be public or secret (default: secret).
//!
//! ## Endpoints used
//!
//! - `POST   /gists`          — create a new gist, return its `id`
//! - `PATCH  /gists/{id}`     — update the gist's file content
//! - `GET    /gists/{id}`      — fetch the gist (returns all files)
//!
//! ## Authentication
//!
//! Bearer token via `Authorization: Bearer {pat}`. The PAT needs the `gist`
//! scope (classic PAT) or read/write gist access (fine-grained PAT).
//!
//! ## Security note
//!
//! Even a *public* gist is safe in our threat model — the blob is opaque
//! ciphertext, and the master key never leaves the local machine. We default
//! to `secret: true` anyway to avoid leaking metadata (gist existence, file
//! size, update timestamps) to scrapers.

use std::sync::Mutex;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::backend::EncryptedSyncBackend;
use crate::config::GistConfig;
use crate::error::{Result, SyncError};

const API_BASE: &str = "https://api.github.com";

/// A backend that stores the blob as a single file inside a GitHub gist.
///
/// If `gist_id` is `None` on the first `push`, a new gist is created and the
/// resulting ID is stored in `self.gist_id` (interior mutability via a
/// `Mutex`). The caller should read `gist_id()` after a successful push and
/// persist it so subsequent pushes update the same gist instead of creating
/// new ones.
pub struct GistBackend {
    client: reqwest::Client,
    config: GistConfig,
    blob_name: String,
    /// `gist_id` may be filled in by `push` (after creating a new gist), so
    /// we need interior mutability. The mutex is held only briefly.
    gist_id: Mutex<Option<String>>,
}

impl GistBackend {
    /// Construct a new Gist backend from a user-supplied config.
    pub fn new(config: GistConfig, blob_name: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            config,
            blob_name: blob_name.into(),
            gist_id: Mutex::new(None),
        }
    }

    /// Returns the gist ID currently in use, if any. After a successful
    /// `push` that created a new gist, this will return `Some(id)`.
    pub fn gist_id(&self) -> Option<String> {
        self.gist_id.lock().ok().and_then(|g| g.clone())
    }

    /// Internal helper: get the gist ID to use for an update, or `None` if
    /// we need to create a new gist.
    fn current_gist_id(&self) -> Option<String> {
        // Prefer the runtime-discovered ID; fall back to the configured one.
        self.gist_id
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .or_else(|| self.config.gist_id.clone())
    }

    /// Internal helper: remember a gist ID discovered via `POST /gists`.
    fn remember_gist_id(&self, id: String) {
        if let Ok(mut g) = self.gist_id.lock() {
            *g = Some(id);
        }
    }

    async fn create_gist(&self, data: &[u8]) -> Result<String> {
        let body = CreateGistRequest {
            description: self
                .config
                .description
                .clone()
                .unwrap_or_else(|| "RusTerm Config".to_string()),
            public: !self.config.secret,
            files: std::iter::once((
                self.blob_name.clone(),
                GistFile {
                    content: base64_encode(data),
                },
            ))
            .collect(),
        };

        let url = format!("{API_BASE}/gists");
        let resp = self
            .client
            .post(&url)
            .bearer_auth(self.config.token.expect_inline())
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("User-Agent", "rusterm-sync")
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(SyncError::HttpStatus {
                status: status.as_u16(),
                url,
                body: text,
            });
        }

        let gist: GistResponse = resp.json().await?;
        self.remember_gist_id(gist.id.clone());
        Ok(gist.id)
    }

    async fn update_gist(&self, gist_id: &str, data: &[u8]) -> Result<()> {
        let body = UpdateGistRequest {
            description: self
                .config
                .description
                .clone()
                .unwrap_or_else(|| "RusTerm Config".to_string()),
            files: std::iter::once((
                self.blob_name.clone(),
                GistFileUpdate {
                    content: Some(base64_encode(data)),
                },
            ))
            .collect(),
        };

        let url = format!("{API_BASE}/gists/{gist_id}");
        let resp = self
            .client
            .patch(&url)
            .bearer_auth(self.config.token.expect_inline())
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("User-Agent", "rusterm-sync")
            .json(&body)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(SyncError::HttpStatus {
                status: status.as_u16(),
                url,
                body: text,
            });
        }
        Ok(())
    }
}

#[async_trait]
impl EncryptedSyncBackend for GistBackend {
    fn name(&self) -> &'static str {
        "gist"
    }

    async fn push(&self, data: &[u8]) -> Result<()> {
        // If we already have a gist ID, update it. Otherwise create a new
        // gist and remember the ID.
        if let Some(id) = self.current_gist_id() {
            self.update_gist(&id, data).await?;
            tracing::debug!(gist_id = %id, bytes = data.len(), "gist: updated blob");
        } else {
            let id = self.create_gist(data).await?;
            tracing::debug!(gist_id = %id, bytes = data.len(), "gist: created new gist");
        }
        Ok(())
    }

    async fn pull(&self) -> Result<Option<Vec<u8>>> {
        let Some(id) = self.current_gist_id() else {
            // No gist configured — nothing to pull.
            return Ok(None);
        };

        let url = format!("{API_BASE}/gists/{id}");
        let resp = self
            .client
            .get(&url)
            .bearer_auth(self.config.token.expect_inline())
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("User-Agent", "rusterm-sync")
            .send()
            .await?;

        let status = resp.status();
        if status.as_u16() == 404 {
            return Ok(None);
        }
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(SyncError::HttpStatus {
                status: status.as_u16(),
                url,
                body: text,
            });
        }

        let gist: GistResponse = resp.json().await?;
        let file = gist
            .files
            .get(&self.blob_name)
            .or_else(|| gist.files.values().next())
            .ok_or_else(|| SyncError::Backend("gist has no files".into()))?;

        let raw = file
            .content
            .as_ref()
            .ok_or_else(|| SyncError::Backend("gist file has no content".into()))?;
        let bytes = base64_decode(raw)?;
        tracing::debug!(gist_id = %id, bytes = bytes.len(), "gist: pulled blob");
        Ok(Some(bytes))
    }
}

// --- GitHub API request/response types ---

#[derive(Serialize)]
struct CreateGistRequest {
    description: String,
    public: bool,
    files: std::collections::BTreeMap<String, GistFile>,
}

#[derive(Serialize)]
struct GistFile {
    content: String,
}

#[derive(Serialize)]
struct UpdateGistRequest {
    description: String,
    // The GitHub API expects a map of filename → { content: "..." | null }.
    // `null` content deletes the file. We always set content.
    files: std::collections::BTreeMap<String, GistFileUpdate>,
}

#[derive(Serialize)]
struct GistFileUpdate {
    content: Option<String>,
}

#[derive(Deserialize)]
struct GistResponse {
    id: String,
    files: std::collections::BTreeMap<String, GistFileContent>,
}

#[derive(Deserialize)]
struct GistFileContent {
    content: Option<String>,
}

// --- base64 helpers ---
//
// The GitHub Gist API stores file content as UTF-8 strings, so we base64-
// encode the raw ciphertext bytes before uploading. This keeps the wire
// format opaque even to GitHub (which would otherwise interpret the bytes
// as text). The base64 layer is *transport encoding* only — it is not a
// security boundary.

fn base64_encode(data: &[u8]) -> String {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    STANDARD.encode(data)
}

fn base64_decode(s: &str) -> Result<Vec<u8>> {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    STANDARD
        .decode(s)
        .map_err(|e| SyncError::Backend(format!("base64 decode failed: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_roundtrip() {
        let bytes = b"some \x00 binary \xff data";
        let encoded = base64_encode(bytes);
        let decoded = base64_decode(&encoded).unwrap();
        assert_eq!(decoded, bytes);
    }
}
