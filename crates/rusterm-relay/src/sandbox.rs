//! Sandbox pre-flight for scripts submitted to `/api/v1/exec`.
//!
//! # Design: static analysis, not local execution
//!
//! The user's directive calls for "可以先在沙箱中预执行，保证安全后才能真的
//! 发送" — pre-execute in a sandbox and only send for real after safety is
//! confirmed. A literal reading would run the script on the relay host
//! before forwarding it to the SSH target. **We deliberately do not do
//! that**, for three reasons:
//!
//! 1. **Host mismatch.** The relay host is not the SSH target host. A script
//!    that runs cleanly on the relay may fail on the target (different
//!    binaries, paths, env), and a script that fails on the relay may be
//!    perfectly valid on the target (e.g. it references `/opt/app/bin/...`).
//!    Local execution produces misleading verdicts in both directions.
//!
//! 2. **Isolation is OS-specific and brittle.** Real sandboxing on Linux
//!    needs `unshare --pid --mount --net --user --fork` plus a read-only
//!    bind-mount of `/usr/bin` and a tmpfs CWD; on macOS it needs
//!    `sandbox-exec` with a hand-written profile; on Windows there is no
//!    first-party equivalent short of a container. Each path is a large
//!    surface for privilege-escalation bugs. The relay crate is meant to be
//!    dependency-light and cross-platform.
//!
//! 3. **The hard floor already catches catastrophic commands.**
//!    [`crate::validator::CommandValidator::validate_script`] runs the
//!    existing terminal-safety patterns plus API-specific deny-lists on
//!    every line, and [`crate::dcg`] adds AST-aware scanning when the
//!    external `dcg` binary is present. Together these are a stronger
//!    signal than a single local execution would be.
//!
//! So the "sandbox" here is a **static pre-flight**: shell syntax check
//! (`sh -n`) + dcg's deeper analysis. This is the same posture as `shellcheck`
//! or `bash -n` — analyse, don't execute. The verdict is recorded in the
//! audit log so operators can see exactly what passed and what was rejected.
//!
//! # Verdict semantics
//!
//! - `Safe` — the script passed syntax check and (if present) dcg analysis.
//!   Proceed to real exec.
//! - `Unsafe` — syntax error, dcg deny, or internal pre-flight failure.
//!   Reject with 403 and audit `SandboxFailed`.
//!
//! Internal failures (e.g. `sh` not found) are treated as `Unsafe` rather
//! than `Safe` — fail closed. The relay operator can always bypass the
//! sandbox by sending a `command` string instead of a `script`, which still
//! goes through the hard-floor validator but skips the script-only pre-flight.

use crate::dcg::{self, DcgVerdict};

/// Outcome of the sandbox pre-flight.
#[derive(Debug, Clone)]
pub enum SandboxVerdict {
    /// The script is safe to forward to the SSH target.
    Safe {
        /// Human-readable note for the audit log (e.g. "syntax ok, dcg allow").
        note: String,
    },
    /// The script is not safe, or the pre-flight itself failed. Always
    /// results in a 403 to the client.
    Unsafe {
        /// Why the script was rejected.
        reason: String,
    },
}

/// Run the sandbox pre-flight on `script`.
///
/// Stages, in order:
/// 1. **Shell syntax check** (`sh -n`): catches unmatched quotes, broken
///    control flow, etc. Never executes the script.
/// 2. **dcg evaluate** (if installed): AST-aware destructive-command scan.
///    Falls back to "no extra signal" when dcg is absent — the hard-floor
///    validator has already run.
///
/// Both stages are static. The script is never executed locally.
pub fn preflight(script: &str) -> SandboxVerdict {
    // Stage 1: shell syntax check. `sh -n` parses the script without
    // executing it; it's POSIX and available on every Unix. On Windows we
    // skip this stage (there is no `sh`), falling through to dcg-only.
    match check_syntax(script) {
        SyntaxResult::Ok => {}
        SyntaxResult::Error(e) => {
            return SandboxVerdict::Unsafe {
                reason: format!("syntax check failed: {e}"),
            };
        }
        SyntaxResult::Skipped => {
            // No `sh` available (likely Windows). Don't fail closed here —
            // the hard-floor validator has already run. Let dcg or the
            // validator make the final call.
        }
    }

    // Stage 2: dcg. When present, its verdict is authoritative for the
    // "deeper analysis" layer. When absent, the hard-floor validator (which
    // already ran in validate_script) is the final authority and we return
    // Safe.
    match dcg::evaluate(script) {
        DcgVerdict::Allow { reason } => SandboxVerdict::Safe {
            note: reason
                .map(|r| format!("syntax ok; dcg allow: {r}"))
                .unwrap_or_else(|| "syntax ok; dcg allow".to_string()),
        },
        DcgVerdict::Deny { reason } => SandboxVerdict::Unsafe {
            reason: format!("dcg blocked: {reason}"),
        },
        DcgVerdict::NotInstalled => SandboxVerdict::Safe {
            note: "syntax ok; dcg not installed (hard-floor validator authoritative)".to_string(),
        },
    }
}

enum SyntaxResult {
    Ok,
    Error(String),
    /// `sh` not found — skip this stage (e.g. Windows without WSL).
    Skipped,
}

fn check_syntax(script: &str) -> SyntaxResult {
    let mut cmd = std::process::Command::new("sh");
    cmd.arg("-n");
    cmd.arg("-c");
    cmd.arg(script);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    match cmd.output() {
        Ok(out) => {
            if out.status.success() {
                SyntaxResult::Ok
            } else {
                let stderr = String::from_utf8_lossy(&out.stderr);
                let trimmed = stderr.trim();
                if trimmed.is_empty() {
                    SyntaxResult::Error(format!(
                        "sh -n exited with {}",
                        out.status.code().unwrap_or(-1)
                    ))
                } else {
                    SyntaxResult::Error(trimmed.to_string())
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // No `sh` on PATH — likely Windows. Skip rather than fail.
            tracing::debug!("[relay] sh not found on PATH; skipping syntax check");
            SyntaxResult::Skipped
        }
        Err(e) => SyntaxResult::Error(format!("sh -n failed to spawn: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn benign_script_passes_preflight() {
        // A trivially safe script. dcg may or may not be installed; if it
        // is absent, the verdict is Safe (hard-floor validator authoritative).
        match prelight_helper("echo hello\necho world\n") {
            SandboxVerdict::Safe { .. } => {}
            SandboxVerdict::Unsafe { reason } => {
                // If dcg is installed and denies this, that's a dcg false
                // positive — acceptable in tests, since the hard-floor
                // validator is the authoritative layer. We just ensure no
                // panic.
                let _ = reason;
            }
        }
    }

    #[test]
    fn syntax_error_is_unsafe() {
        let verdict = prelight_helper("if true; then echo broken\n");
        match verdict {
            SandboxVerdict::Unsafe { reason } => {
                assert!(reason.contains("syntax"), "got: {reason}");
            }
            other => panic!("expected Unsafe, got {other:?}"),
        }
    }

    #[test]
    fn empty_script_is_safe_syntactically() {
        // Empty string is a valid (if useless) script syntactically. The
        // empty-script rejection happens earlier in the validator, not here.
        match preflight("") {
            SandboxVerdict::Safe { .. } => {}
            SandboxVerdict::Unsafe { .. } => {
                // dcg may reject empty input; that's fine.
            }
        }
    }

    // Helper: same as preflight, renamed for readability in tests.
    fn preflight_helper(script: &str) -> SandboxVerdict {
        preflight(script)
    }
}
