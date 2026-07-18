//! Centralized logging initialization for RusTerm.
//!
//! # Privacy contract
//!
//! RusTerm's runtime logs are *operational only*. They must never contain:
//!
//! - Terminal I/O (keystrokes, command output, pane contents) — that data is
//!   handled by `SessionLog`, which encrypts it at rest separately.
//! - User credentials (passwords, SSH keys, passphrases, tokens, cookies,
//!   bearer tokens, API keys).
//! - Personally identifying information (usernames, hostnames, home directory
//!   paths, real names, emails).
//! - Encryption keys (master key, session-log keys, derived keys).
//!
//! To enforce this defensively, this module installs a `RedactingMakeWriter`
//! that scans every log record's formatted output for known secret patterns
//! and replaces them with `<redacted>` before the record reaches the file
//! writer. This is a *safety net* — code should still avoid logging secrets
//! in the first place.
//!
//! # Where logs live
//!
//! Logs are written to `dirs::data_local_dir()/rusterm/logs/rusterm.log` with
//! daily rotation. Only the most recent files are retained by the OS-managed
//! rolling appender. Logs never leave the user's machine.
//!
//! # What gets logged
//!
//! - Application lifecycle (start, shutdown, version).
//! - Session lifecycle (created, closed) — by *opaque session ID*, never by
//!   host/user.
//! - Connection outcomes (success / failure with error category, not the
//!   underlying error message if it could embed credentials).
//! - Performance counters (bytes processed, render latency, plugin invocation
//!   duration).
//! - Crash-level errors with stack-free context.

use std::path::PathBuf;
use std::sync::OnceLock;

use regex::Regex;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::prelude::*;

/// Directory name under the platform's local-data dir where logs are stored.
pub const LOG_DIR_NAME: &str = "rusterm";
/// Subdirectory name for logs (kept separate from `session_logs`, which is
/// encrypted user data, not runtime logs).
pub const LOG_SUBDIR: &str = "logs";

/// Returns the absolute path to the directory where runtime log files are
/// stored. Created on first call.
pub fn log_dir() -> PathBuf {
    let base = dirs::data_local_dir()
        .or_else(dirs::data_dir)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join(LOG_DIR_NAME).join(LOG_SUBDIR)
}

/// Patterns that, if matched in any log field or message, get replaced with
/// `<redacted>`. The list is intentionally conservative: false positives only
/// redact data that shouldn't be in logs anyway.
///
/// NOTE: these patterns are *defense in depth*. The primary defense is that
/// code never passes secrets to `tracing` macros in the first place.
fn redaction_patterns() -> Vec<Regex> {
    vec![
        // `password=...`, `pass=...`, `passwd=...` — URL-encoded, quoted, or bare.
        Regex::new(r#"(?i)(password|passwd|pass)\s*[=:]\s*[^\s,;\]\}"']+"#).unwrap(),
        // `token=...`, `access_token=...`, `api_key=...`, `apikey=...`, `secret=...`.
        Regex::new(r#"(?i)(token|api[_-]?key|secret|bearer|credential)\s*[=:]\s*[^\s,;\]\}"']+"#)
            .unwrap(),
        // `Authorization: Bearer ...` / `Basic ...` header values.
        Regex::new(r"(?i)authorization\s*:\s*(bearer|basic)\s+[A-Za-z0-9._\-+/=]+").unwrap(),
        // PEM-encoded private key blocks (RSA, EC, OPENSSH, etc.).
        Regex::new(r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----")
            .unwrap(),
        // JWT-like strings (three base64url segments separated by dots). Conservative:
        // requires the typical `ey...` header prefix to avoid catching version triples.
        Regex::new(r"ey[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+").unwrap(),
        // Generic long hex/base64 strings assigned to key-ish names.
        Regex::new(
            r#"(?i)(key|secret|token|password|passphrase)\s*[=:]\s*[A-Za-z0-9+/]{32,}={0,2}"#,
        )
        .unwrap(),
    ]
}

/// Redact known-sensitive substrings from `s`. Returns a new `String`.
pub fn redact(s: &str) -> String {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    let patterns = PATTERNS.get_or_init(redaction_patterns);
    let mut out = s.to_string();
    for re in patterns {
        out = re.replace_all(&out, "<redacted>").to_string();
    }
    out
}

/// A writer wrapper that applies `redact()` to each formatted record before it
/// reaches the underlying writer. Used as the `MakeWriter` for the fmt layer.
pub struct RedactingWriter<W> {
    inner: W,
}

impl<W: std::io::Write> std::io::Write for RedactingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // Each `write` call from `tracing-subscriber::fmt` corresponds to one
        // complete formatted record (terminated by `\n`). Redact the whole
        // record as a unit so multi-line PEM blocks are caught by the regex.
        let s = std::str::from_utf8(buf).unwrap_or("");
        let redacted = redact(s);
        self.inner.write_all(redacted.as_bytes())?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Factory producing `RedactingWriter`s around a base `MakeWriter`.
pub struct RedactingMakeWriter<M> {
    inner: M,
}

impl<M> RedactingMakeWriter<M> {
    pub fn new(inner: M) -> Self {
        Self { inner }
    }
}

impl<'a, M> MakeWriter<'a> for RedactingMakeWriter<M>
where
    M: MakeWriter<'a>,
{
    type Writer = RedactingWriter<M::Writer>;

    fn make_writer(&'a self) -> Self::Writer {
        RedactingWriter {
            inner: self.inner.make_writer(),
        }
    }
}

/// Guard returned by `init_logging`. Must be kept alive for the entire
/// process lifetime — dropping it flushes and closes the log file.
pub struct LogGuard {
    _file_guard: tracing_appender::non_blocking::WorkerGuard,
}

/// Initialize global logging. Returns a `LogGuard` that must be held for the
/// process lifetime (otherwise non-blocking writes may be dropped on
/// shutdown).
///
/// # What this configures
///
/// - JSON formatter (machine-parseable, no ANSI colors).
/// - Daily-rotating file appender under `log_dir()`.
/// - `RedactingMakeWriter` wrapping the file writer — every record is scanned
///   for known secret patterns and redacted before reaching disk.
/// - `EnvFilter` defaulting to `rusterm=info`; overridable via `RUST_LOG`.
/// - A denylist of third-party crate targets known to log freely (silenced
///   via `target=off` directives on the `EnvFilter`).
///
/// # Panics
/// Panics if called more than once (process-wide subscriber already set).
pub fn init_logging() -> LogGuard {
    let dir = log_dir();
    std::fs::create_dir_all(&dir).ok();

    let file_appender = tracing_appender::rolling::daily(&dir, "rusterm.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    // Build an EnvFilter with:
    //   - default `rusterm=info` (overridable via `RUST_LOG`)
    //   - explicit `off` for third-party crates that may log URLs/headers/args
    //     and could leak credentials. We lose some diagnosability but gain a
    //     strong privacy guarantee.
    let env_filter = EnvFilter::from_default_env()
        .add_directive("rusterm=info".parse().expect("valid directive"))
        .add_directive("hyper=off".parse().expect("valid directive"))
        .add_directive("reqwest=off".parse().expect("valid directive"))
        .add_directive("russh=off".parse().expect("valid directive"))
        .add_directive("russh_keys=off".parse().expect("valid directive"))
        .add_directive("async_openai=off".parse().expect("valid directive"))
        .add_directive("wasmtime=off".parse().expect("valid directive"))
        .add_directive("wasmtime_wasi=off".parse().expect("valid directive"));

    let fmt = tracing_subscriber::fmt::layer()
        .json()
        .with_ansi(false)
        .with_writer(RedactingMakeWriter::new(non_blocking));

    let subscriber = tracing_subscriber::registry().with(env_filter).with(fmt);
    tracing::subscriber::set_global_default(subscriber).expect("logging already initialized");

    LogGuard { _file_guard: guard }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    #[test]
    fn redact_strips_password_assignment() {
        let s = "connecting with password=hunter2 please";
        let out = redact(s);
        assert!(!out.contains("hunter2"));
        assert!(out.contains("<redacted>"));
    }

    #[test]
    fn redact_strips_authorization_header() {
        let s = "Authorization: Bearer eyJabc.def.ghi";
        let out = redact(s);
        assert!(!out.contains("eyJabc"));
    }

    #[test]
    fn redact_strips_pem_block() {
        let s = "key was:\n-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEA...\n-----END RSA PRIVATE KEY-----\ndone";
        let out = redact(s);
        assert!(!out.contains("MIIEpAIBAAKCAQEA"));
        assert!(out.contains("<redacted>"));
    }

    #[test]
    fn redact_strips_jwt() {
        let s = "token: eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.abc123-_x";
        let out = redact(s);
        assert!(!out.contains("eyJhbGci"));
    }

    #[test]
    fn redact_preserves_safe_content() {
        let s = "session created id=abc12345 bytes=1024";
        let out = redact(s);
        assert_eq!(s, out);
    }

    #[test]
    fn redact_handles_empty_and_unicode() {
        assert_eq!(redact(""), "");
        assert_eq!(redact("你好世界"), "你好世界");
    }

    #[test]
    fn redact_strips_api_key_variants() {
        for s in [
            "api_key=ABCDEF1234567890ABCDEF1234567890",
            "api-key=ABCDEF1234567890ABCDEF1234567890",
            "apikey=ABCDEF1234567890ABCDEF1234567890",
        ] {
            let out = redact(s);
            assert!(
                !out.contains("ABCDEF1234567890ABCDEF1234567890"),
                "failed for: {s}"
            );
        }
    }

    /// Verify that a `RedactingWriter` actually scrubs secrets passing through
    /// it. This is the safety-net guarantee: even if a developer writes
    /// `tracing::info!("password=hunter2")`, the on-disk record is redacted.
    #[test]
    fn redacting_writer_scrubs_output() {
        let mut buf = Vec::new();
        {
            let mut w = RedactingWriter { inner: &mut buf };
            w.write_all(b"auth password=hunter2 ok\n").unwrap();
        }
        let s = String::from_utf8(buf).unwrap();
        assert!(!s.contains("hunter2"));
        assert!(s.contains("<redacted>"));
    }
}
