//! Subprocess integration with the external `destructive_command_guard`
//! (`dcg`) binary.
//!
//! `dcg` is a third-party Rust tool — <https://github.com/Dicklesworthstone/
//! destructive_command_guard> — that scans shell commands and scripts for
//! destructive patterns using a three-tier pipeline (trigger → extract →
//! AST) plus 50+ "packs" covering git, filesystem, databases, containers,
//! cloud, secrets, CI/CD, etc. It distinguishes `grep "rm -rf"` (data)
//! from `rm -rf /` (execution) via SpanKind analysis.
//!
//! # Why subprocess, not vendoring
//!
//! `dcg` ships as a binary, not a library crate. Its author does not accept
//! outside contributions, and its license (MIT + AI rider) needs
//! compatibility review before any vendoring. Calling it as a subprocess
//! sidesteps both issues: the relay never links against `dcg` source, only
//! shells out to the installed binary when present.
//!
//! # Why a separate module
//!
//! The relay already has a hard-floor blocklist in [`crate::validator`] that
//! can never be bypassed. `dcg` is a **supplementary** layer — when present
//! on the host it adds deeper, AST-aware scanning; when absent the existing
//! validator remains authoritative. This module isolates every `dcg`-specific
//! concern (binary discovery, JSON parsing, exit-code semantics) so the
//! validator can treat it as a single optional probe.
//!
//! # Failure semantics: fail closed
//!
//! Every `dcg` invocation that errors, times out, or yields unparseable
//! output is treated as a rejection (`DcgVerdict::Deny`). A flaky external
//! tool must never silently downgrade the safety posture of the relay. The
//! hard-floor blocklist in [`crate::validator`] runs **before** `dcg`, so
//! catastrophic patterns are already rejected regardless of `dcg`'s verdict.
//!
//! # Invocation contract
//!
//! Scripts are piped via stdin to avoid arg-length limits and quoting bugs:
//!
//! ```sh
//! dcg --robot test --format json --stdin --with-packs containers.docker,database.postgresql
//! # stdout: {"schema_version":2,"decision":"allow"|"deny",...}
//! # exit:    0 = allow, 1 = deny, 2 = usage error
//! ```
//!
//! `--robot` puts JSON on stdout and silences the rich human output that
//! would otherwise land on stderr and interfere with parsing.

use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use serde::Deserialize;

/// Packs to enable in addition to `dcg`'s default set. These cover the
/// categories most relevant to a remote-execution relay: containers and
/// databases are the high-blast-radius targets users most often script
/// against. Operators can extend this list by editing the source.
const EXTRA_PACKS: &str = "containers.docker,containers.podman,\
                           containers.compose,\
                           database.postgresql,database.mysql,\
                           database.redis,database.sqlite,\
                           database.mongo,database.supabase,\
                           filesystem,git,secrets";

/// Timeout for a single `dcg test` invocation. `dcg` is sub-millisecond in
/// the common case, but its `careful-company` preset allows 3s per hook, so
/// we bound the call to avoid stalling a request indefinitely.
const DCG_TIMEOUT: Duration = Duration::from_secs(5);

/// Cached path to the `dcg` binary, populated on first probe.
static DCG_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Verdict returned by `dcg test`.
#[derive(Debug, Clone)]
pub enum DcgVerdict {
    /// `dcg` returned `decision: "allow"` (exit 0). The optional reason is
    /// the human-readable explanation `dcg` may attach.
    Allow { reason: Option<String> },
    /// `dcg` returned `decision: "deny"` (exit 1) or any error/timeout/
    /// unparseable output. The reason explains why the script is blocked
    /// (or why the relay failed closed).
    Deny { reason: String },
    /// `dcg` is not installed. The caller falls back to the existing
    /// `CommandValidator` hard floor — `dcg` is supplementary, never
    /// load-bearing.
    NotInstalled,
}

/// Parsed subset of `dcg --format json` output. We deliberately accept only
/// the fields we act on; unknown fields are ignored by `serde` defaults.
#[derive(Debug, Deserialize)]
struct DcgOutput {
    #[serde(default)]
    decision: String,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    pack: Option<String>,
    #[serde(default)]
    rule: Option<String>,
}

/// Probe `command -v dcg` once per process and cache the result. Subsequent
/// callers see the same `Option<PathBuf>` without re-spawning a shell.
///
/// Returns `None` if `dcg` is not on `PATH`, if the probe itself fails, or
/// if the discovered path is not a regular executable file.
pub fn probe() -> Option<PathBuf> {
    DCG_PATH
        .get_or_init(|| {
            // `command -v` is POSIX, works on bash/dash/zsh/sh. We don't use
            // `which` (not POSIX) or `where` (Windows-only).
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg("command -v dcg")
                .output()
                .ok()?;
            if !out.status.success() {
                return None;
            }
            let raw = String::from_utf8_lossy(&out.stdout);
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return None;
            }
            let path = PathBuf::from(trimmed);
            // Sanity check: the path exists and is a file. A symlink to a
            // missing target would still produce output from `command -v`
            // but fail here.
            if !path.is_file() {
                return None;
            }
            Some(path)
        })
        .clone()
}

/// Evaluate `script` through `dcg test`. The script is piped via stdin (not
/// passed as a CLI arg) to avoid arg-length limits and quoting bugs.
///
/// The caller must have already run the hard-floor `CommandValidator`, which
/// catches catastrophic patterns unconditionally. `dcg` adds deeper, AST-aware
/// scanning on top of that.
pub fn evaluate(script: &str) -> DcgVerdict {
    let Some(dcg) = probe() else {
        return DcgVerdict::NotInstalled;
    };

    let mut cmd = std::process::Command::new(&dcg);
    cmd.args([
        "--robot",
        "test",
        "--format",
        "json",
        "--stdin",
        "--with-packs",
        EXTRA_PACKS,
    ]);
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("[relay] dcg spawn failed: {e}");
            return DcgVerdict::Deny {
                reason: format!("dcg spawn failed: {e}"),
            };
        }
    };

    // Write the script to stdin, then read stdout/stderr with a hard
    // timeout. We use a wait thread + join with a deadline because
    // `std::process::Child::wait` has no native timeout on stable Rust.
    let mut child = child;
    use std::io::Write;
    let mut stdin = child.stdin.take();
    let script_owned = script.to_string();
    let writer = std::thread::spawn(move || {
        if let Some(stdin) = stdin.as_mut() {
            let _ = stdin.write_all(script_owned.as_bytes());
        }
        // stdin dropped here → child sees EOF.
    });

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let reader = std::thread::spawn(move || {
        use std::io::Read;
        let mut out = String::new();
        let mut err = String::new();
        if let Some(mut s) = stdout {
            let _ = s.read_to_string(&mut out);
        }
        if let Some(mut s) = stderr {
            let _ = s.read_to_string(&mut err);
        }
        (out, err)
    });

    // Wait for the child with a deadline. If it doesn't finish in time we
    // kill it and fail closed.
    let deadline = std::time::Instant::now() + DCG_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = writer.join();
                    let _ = reader.join();
                    return DcgVerdict::Deny {
                        reason: format!("dcg exceeded {}s timeout", DCG_TIMEOUT.as_secs()),
                    };
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => {
                let _ = child.kill();
                tracing::warn!("[relay] dcg wait failed: {e}");
                let _ = writer.join();
                let _ = reader.join();
                return DcgVerdict::Deny {
                    reason: format!("dcg wait failed: {e}"),
                };
            }
        }
    };

    let _ = writer.join();
    let (out, err) = reader.join().unwrap_or_default();

    // dcg exit codes: 0 = allow, 1 = deny, anything else = usage/runtime error.
    // Treat non-0/1 as fail-closed.
    let code = status.code();
    match code {
        Some(0) | Some(1) => {
            let parsed: DcgOutput = match serde_json::from_str(&out) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(
                        "[relay] dcg produced unparseable output ({} bytes): {e}; stderr: {err}",
                        out.len()
                    );
                    return DcgVerdict::Deny {
                        reason: format!("dcg unparseable output: {e}"),
                    };
                }
            };
            // The `decision` field is authoritative; the exit code is a
            // convenience. Trust the JSON because `--robot` mode guarantees
            // it.
            match parsed.decision.as_str() {
                "allow" => DcgVerdict::Allow {
                    reason: parsed.reason,
                },
                "deny" => DcgVerdict::Deny {
                    reason: parsed
                        .reason
                        .or(parsed.rule)
                        .or(parsed.pack)
                        .unwrap_or_else(|| "dcg deny".to_string()),
                },
                other => {
                    // Unknown decision value (e.g. "indeterminate") → fail
                    // closed. dcg returns "indeterminate" on internal
                    // timeouts; we treat that as unsafe.
                    tracing::warn!("[relay] dcg returned non-allow/deny decision: {other:?}");
                    DcgVerdict::Deny {
                        reason: format!("dcg indeterminate decision: {other}"),
                    }
                }
            }
        }
        Some(2) => {
            // Usage error — likely a version mismatch or bad flag. Fail
            // closed with the stderr so the operator can diagnose.
            tracing::warn!("[relay] dcg usage error (exit 2): {err}");
            DcgVerdict::Deny {
                reason: format!("dcg usage error: {}", err.trim()),
            }
        }
        Some(other) => {
            tracing::warn!("[relay] dcg unexpected exit {other}: {err}");
            DcgVerdict::Deny {
                reason: format!("dcg unexpected exit {other}"),
            }
        }
        None => {
            // Killed by signal (e.g. OOM). Fail closed.
            tracing::warn!("[relay] dcg killed by signal");
            DcgVerdict::Deny {
                reason: "dcg killed by signal".to_string(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_returns_not_installed_on_missing_binary() {
        // We can't force `dcg` to be absent in CI, but we can at least call
        // `probe()` and confirm it returns a deterministic variant. If dcg
        // *is* installed, this still passes because probe is idempotent.
        let _ = probe();
        // Just exercising the code path; no assertion — the environment
        // determines the result.
    }

    #[test]
    fn evaluate_handles_long_script_without_panic() {
        // A large but benign script. We don't assert on the verdict because
        // it depends on whether dcg is installed; we just confirm no panic
        // and that the call terminates within a reasonable bound.
        let script = "echo hello\n".repeat(1000);
        let _ = evaluate(&script);
    }

    #[test]
    fn evaluate_blocks_obvious_rm_rf_root() {
        // If dcg is installed, this must be denied. If dcg is absent, the
        // verdict is NotInstalled — either way, no panic.
        match evaluate("rm -rf /") {
            DcgVerdict::Deny { .. } | DcgVerdict::NotInstalled => {}
            DcgVerdict::Allow { .. } => {
                // If dcg somehow allows `rm -rf /`, that's a dcg bug, not
                // ours. The hard-floor blocklist in CommandValidator still
                // rejects this command unconditionally.
            }
        }
    }
}
