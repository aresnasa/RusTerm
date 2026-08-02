//! Append-only JSON-lines audit log for relay operations. Every exec
//! attempt — success, rejection or failure — is recorded with the account,
//! client IP, target host, command and outcome. Written next to the app's
//! own logs so log-rotation tooling can pick it up.

use std::path::PathBuf;
use std::sync::Mutex;

use serde::Serialize;

const AUDIT_FILE_NAME: &str = "relay-audit.jsonl";
/// Rotate into a new file (by truncate-and-log-warning strategy) past this.
const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Debug, Clone, Serialize)]
pub struct AuditEntry {
    pub ts: String,
    pub account: String,
    pub client_ip: String,
    pub action: AuditAction,
    pub host_id: Option<String>,
    pub command: Option<String>,
    pub outcome: AuditOutcome,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditAction {
    AuthFailure,
    ExecAccepted,
    ExecRejected,
    ExecFailed,
    ParseCurl,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuditOutcome {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

impl AuditOutcome {
    pub fn ok(exit_code: Option<u32>, duration_ms: u64) -> Self {
        Self {
            success: true,
            exit_code,
            reason: None,
            duration_ms: Some(duration_ms),
        }
    }
    pub fn rejected(reason: impl Into<String>) -> Self {
        Self {
            success: false,
            exit_code: None,
            reason: Some(reason.into()),
            duration_ms: None,
        }
    }
}

/// Write the audit file into the app log dir when available (so it rotates
/// with the rest), falling back to the config dir when the logger isn't
/// initialized yet (unit tests).
fn resolve_audit_path() -> PathBuf {
    rusterm_core::logging::log_dir()
        .unwrap_or_else(|| {
            rusterm_core::paths::resolve_config_file_path(AUDIT_FILE_NAME)
                .ok()
                .and_then(|p| p.parent().map(|d| d.to_path_buf()))
                .unwrap_or_else(|| PathBuf::from("."))
        })
        .join(AUDIT_FILE_NAME)
}

/// Synchronously append entries. Held behind a mutex so concurrent request
/// handlers never interleave a write. Volumes are small enough (relays are
/// rate-limited anyway) that blocking file I/O here is fine.
#[derive(Debug)]
pub struct AuditLog {
    path: PathBuf,
    file: Mutex<Option<std::fs::File>>,
}

impl Default for AuditLog {
    fn default() -> Self {
        Self::new()
    }
}

impl AuditLog {
    pub fn new() -> Self {
        Self {
            path: resolve_audit_path(),
            file: Mutex::new(None),
        }
    }

    /// Variant for tests that want an isolated file.
    #[cfg(test)]
    pub fn at(path: PathBuf) -> Self {
        Self {
            path,
            file: Mutex::new(None),
        }
    }

    pub fn log(&self, entry: AuditEntry) {
        use std::io::Write;
        let line = match serde_json::to_string(&entry) {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!("[relay] failed to serialize audit entry: {e}");
                return;
            }
        };
        let mut guard = self.file.lock().unwrap();
        if guard.is_none() {
            if let Some(parent) = self.path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
            {
                Ok(f) => *guard = Some(f),
                Err(e) => {
                    tracing::warn!("[relay] cannot open audit log {}: {e}", self.path.display());
                    return;
                }
            }
        }
        // Rotate when the file grows past the cap: rename to `.1`, reopen.
        let oversized = guard
            .as_ref()
            .and_then(|f| f.metadata().ok())
            .is_some_and(|m| m.len() > MAX_FILE_BYTES);
        if oversized {
            *guard = None;
            let _ = std::fs::rename(&self.path, self.path.with_extension("jsonl.1"));
            *guard = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
                .ok();
        }
        let Some(file) = guard.as_mut() else { return };
        let _ = writeln!(file, "{line}");
    }
}

pub fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_json_lines() {
        let dir = std::env::temp_dir().join(format!("rusterm-audit-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("audit.jsonl");
        let _ = std::fs::remove_file(&path);

        let log = AuditLog::at(path.clone());
        log.log(AuditEntry {
            ts: now_iso(),
            account: "ops".into(),
            client_ip: "127.0.0.1".into(),
            action: AuditAction::ExecAccepted,
            host_id: Some("h1".into()),
            command: Some("uptime".into()),
            outcome: AuditOutcome::ok(Some(0), 42),
        });

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("\"action\":\"exec_accepted\""));
        assert!(content.contains("\"command\":\"uptime\""));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
