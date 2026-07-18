//! Host key verification for SSH connections.
//!
//! This module implements a TOFU (Trust On First Use) host-key verification
//! scheme compatible with the OpenSSH `known_hosts` file format.
//!
//! # Policies
//!
//! [`HostKeyPolicy`] is the user-facing knob:
//! - [`HostKeyPolicy::AcceptNew`] — TOFU. The first time we see a host, its
//!   key is recorded. On subsequent connections, a mismatched key is a hard
//!   fail (likely MITM). This is the default and the right choice for most
//!   users: it gives real MITM protection after first contact without
//!   requiring the user to manually pre-populate `known_hosts`.
//! - [`HostKeyPolicy::Strict`] — Reject any host whose key is not already
//!   in `known_hosts`. Use this when you can pre-populate `known_hosts`
//!   (e.g. via `ssh-keyscan` on a trusted network) for maximum safety.
//! - [`HostKeyPolicy::Disabled`] — Skip verification entirely. **INSECURE**;
//!   vulnerable to MITM. Provided only for break-glass / lab scenarios.
//!
//! # Storage
//!
//! The file lives at `<config_dir>/rusterm/known_hosts` (resolved via
//! [`dirs::config_dir`]). Format is one OpenSSH-known_hosts entry per line:
//!
//! ```text
//! host ssh-ed25519 AAAA...
//! ```
//!
//! Only the bare hostname is stored (no hashing, no port suffix, no
//! wildcards). This is deliberately simpler than full OpenSSH semantics —
//! RusTerm doesn't (yet) support per-port or hashed host entries, and a
//! simpler format is easier to audit and harder to get wrong.
//!
//! # Security notes
//!
//! - The known_hosts file is created with `0600` perms on POSIX. We don't
//!   enforce this on every read (an attacker with write access to the file
//!   already has equivalent powers via swapping the binary), but we do
//!   create it restrictively so a fresh install doesn't leak host names to
//!   other local users.
//! - File I/O errors during verification are treated as "reject": a missing
//!   or unreadable known_hosts file under `Strict` policy fails closed; under
//!   `AcceptNew` it's treated as "first contact" (file doesn't exist yet).
//! - Fingerprints are SHA-256, formatted `SHA256:base64` (OpenSSH-style).
//!   This gives a stable, human-comparable identifier without exposing the
//!   raw key bytes in logs.
//! - The raw key (OpenSSH wire format) is what we actually persist — the
//!   fingerprint is computed on every verify and compared to the persisted
//!   key's fingerprint, so a corrupt known_hosts line never silently
//!   accepts a wrong key.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use russh::keys::ssh_key::{HashAlg, PublicKey};

/// Host key verification policy. See the module docs for details.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostKeyPolicy {
    /// TOFU: record on first contact, reject mismatches thereafter.
    AcceptNew,
    /// Reject any host not already in known_hosts.
    Strict,
    /// Skip verification entirely. **INSECURE**.
    Disabled,
}

impl HostKeyPolicy {
    /// Parse a policy string from `SshConfig::host_key_policy`.
    ///
    /// Unknown values fall back to [`HostKeyPolicy::AcceptNew`] (the safe
    /// default) and emit a warning so misconfigurations are visible rather
    /// than silently weakening security.
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "accept-new" | "tofu" | "" => HostKeyPolicy::AcceptNew,
            "strict" | "yes" | "true" => HostKeyPolicy::Strict,
            "disabled" | "no" | "false" | "off" => HostKeyPolicy::Disabled,
            other => {
                tracing::warn!(
                    "unknown host_key_policy {:?}, falling back to accept-new (TOFU)",
                    other
                );
                HostKeyPolicy::AcceptNew
            }
        }
    }
}

/// Resolve the path to the known_hosts file.
///
/// Honors `RUSTERM_CONFIG_DIR` if set (for tests / portable installs),
/// otherwise uses `<config_dir>/rusterm/known_hosts`.
pub fn known_hosts_path() -> PathBuf {
    if let Ok(dir) = std::env::var("RUSTERM_CONFIG_DIR") {
        return PathBuf::from(dir).join("known_hosts");
    }
    let mut path = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    path.push("rusterm");
    path.push("known_hosts");
    path
}

/// SHA-256 fingerprint of a public key, formatted `SHA256:base64`
/// (OpenSSH-compatible).
pub fn fingerprint(key: &PublicKey) -> String {
    key.fingerprint(HashAlg::Sha256).to_string()
}

/// Load known_hosts into a `host -> openssh_key_string` map.
///
/// Lines that fail to parse are skipped with a warning — a single corrupt
/// line must not brick SSH for every host.
fn load_known_hosts(path: &Path) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Ok(content) = fs::read_to_string(path) else {
        // Missing file is not an error here — caller decides what to do.
        return out;
    };
    for (lineno, raw) in content.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Format: `host keytype keydata [comment]`
        let mut parts = line.split_whitespace();
        let Some(host) = parts.next() else { continue };
        let Some(keytype) = parts.next() else {
            continue;
        };
        let Some(keydata) = parts.next() else {
            continue;
        };
        // Reconstruct the canonical openssh string (drop comment).
        let canonical = format!("{keytype} {keydata}");
        if out.insert(host.to_string(), canonical).is_some() {
            tracing::warn!(
                "known_hosts:{}: duplicate entry for host {:?}, last one wins",
                lineno + 1,
                host
            );
        }
    }
    out
}

/// Append a new entry to the known_hosts file, creating it (and parent
/// directories) if needed. The file is created with `0600` perms on POSIX
/// so other local users can't read which hosts the user connects to.
fn append_known_host(path: &Path, host: &str, openssh_key: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    // Tighten perms on POSIX if we just created the file. We attempt this
    // best-effort — if it fails (e.g. on Windows), we continue rather than
    // blocking the connection.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = file.metadata() {
            let mut perms = meta.permissions();
            // 0o600: rw for owner only.
            if perms.mode() & 0o077 != 0 {
                perms.set_mode(0o600);
                let _ = fs::set_permissions(path, perms);
            }
        }
    }
    writeln!(file, "{host} {openssh_key}")?;
    file.sync_all()?;
    Ok(())
}

/// Result of host key verification.
#[derive(Debug)]
pub enum VerifyOutcome {
    /// The key matches a previously-recorded entry. Connection is safe.
    Matched,
    /// This is the first time we've seen this host; the key has been
    /// recorded to known_hosts. Connection is safe.
    Added,
    /// The presented key does NOT match the previously-recorded key.
    /// Likely MITM — the connection MUST be rejected.
    Mismatch { expected: String, presented: String },
    /// `Strict` policy was set and the host is not in known_hosts.
    /// The connection MUST be rejected.
    UnknownHost,
    /// Verification disabled by policy. Connection proceeds without
    /// verification (INSECURE).
    Skipped,
}

/// Compute the OpenSSH wire-format string for a public key, e.g.
/// `ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI...`.
///
/// Returns `None` if encoding fails — under no circumstance should we
/// persist or compare a malformed key.
fn openssh_string(key: &PublicKey) -> Option<String> {
    key.to_openssh()
        .map_err(|e| {
            tracing::error!("failed to encode server host key to OpenSSH format: {e}");
        })
        .ok()
        .map(|s| {
            // to_openssh() may include a trailing comment; we want just
            // `type data` for known_hosts (we write our own host prefix).
            // Split off the first two whitespace-separated fields.
            let mut it = s.split_whitespace();
            let kt = it.next().unwrap_or_default();
            let kd = it.next().unwrap_or_default();
            format!("{kt} {kd}")
        })
}

/// Verify a server's public key against the known_hosts file, applying
/// the given policy.
///
/// This is the entry point called by the russh `check_server_key` handler.
/// It performs all file I/O and policy decisions, returning an outcome
/// the caller maps to `Ok(true)` / `Ok(false)`.
///
/// `known_hosts_path` lets tests inject a tempdir-owned path; production
/// callers should pass `None`, which resolves to the standard user-config
/// location (see [`known_hosts_path`]).
pub fn verify_server_key(
    host: &str,
    key: &PublicKey,
    policy: HostKeyPolicy,
    known_hosts_path_override: Option<&Path>,
) -> VerifyOutcome {
    if policy == HostKeyPolicy::Disabled {
        tracing::warn!(
            "host key verification DISABLED for {:?} — connection vulnerable to MITM",
            host
        );
        return VerifyOutcome::Skipped;
    }

    let Some(presented) = openssh_string(key) else {
        // We couldn't encode the key — fail closed.
        tracing::error!(
            "host key for {:?} could not be encoded; refusing to proceed",
            host
        );
        return VerifyOutcome::Mismatch {
            expected: "<unknown: encode failed>".to_string(),
            presented: "<encode failed>".to_string(),
        };
    };
    let presented_fp = fingerprint(key);

    let path = known_hosts_path_override
        .map(|p| p.to_path_buf())
        .unwrap_or_else(known_hosts_path);
    let path = path.as_path();
    let known = load_known_hosts(path);

    if let Some(recorded) = known.get(host) {
        // We've seen this host before. Compare fingerprints (more
        // informative than comparing the raw openssh strings, and
        // robust to whitespace differences in comments).
        let recorded_fp = fingerprint_of_openssh(recorded).unwrap_or_else(|| {
            tracing::warn!(
                "known_hosts entry for {:?} is malformed ({:?}); treating as mismatch",
                host,
                recorded
            );
            "<malformed>".to_string()
        });
        if recorded_fp == presented_fp {
            VerifyOutcome::Matched
        } else {
            VerifyOutcome::Mismatch {
                expected: recorded_fp,
                presented: presented_fp,
            }
        }
    } else {
        // First contact with this host.
        match policy {
            HostKeyPolicy::AcceptNew => {
                match append_known_host(path, host, &presented) {
                    Ok(()) => {
                        tracing::info!(
                            "first contact with {:?}: recorded host key fingerprint {} to {:?}",
                            host,
                            presented_fp,
                            path
                        );
                        VerifyOutcome::Added
                    }
                    Err(e) => {
                        // We failed to persist the key. Under TOFU, this
                        // is not grounds to reject — the user can still
                        // connect this once — but we warn loudly because
                        // *every* future connection will also be "first
                        // contact", losing MITM protection.
                        tracing::warn!(
                            "failed to write known_hosts entry for {:?}: {} — MITM protection will not engage until this is fixed",
                            host,
                            e
                        );
                        VerifyOutcome::Added
                    }
                }
            }
            HostKeyPolicy::Strict => VerifyOutcome::UnknownHost,
            // Already handled above; this arm is unreachable but keeps
            // the match exhaustive without a `_` that would hide bugs.
            HostKeyPolicy::Disabled => VerifyOutcome::Skipped,
        }
    }
}

/// Compute the SHA-256 fingerprint of an OpenSSH-format key string
/// (`ssh-ed25519 AAAA...`). Returns `None` if the string is malformed.
fn fingerprint_of_openssh(s: &str) -> Option<String> {
    let key: PublicKey = PublicKey::from_openssh(s).ok()?;
    Some(fingerprint(&key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// A static, well-formed Ed25519 OpenSSH public key for testing.
    ///
    /// We don't generate keys at test time because russh's `PrivateKey::random`
    /// wants a `CryptoRng` from its *own* `rand_core` re-export, which is
    /// a different version than the `rand` crate we depend on — wiring them
    /// together is more fragile than just hard-coding a known-good key.
    /// The actual key value doesn't matter for these tests; we just need
    /// *any* valid public key that can be encoded and parsed.
    const TEST_ED25519_PUB_OPENSSH: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAINfLuJZn1BDmU7fD6D7An7mUJ5lM4lQrI3kDQUdLb6Tr test";

    /// A second, distinct Ed25519 public key for mismatch tests.
    const TEST_ED25519_PUB_OPENSSH_2: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIE2nBp9lC6mVdYbL7q5l9QpZ5l8RZxJQr4XNoPmK3vT6 test2";

    fn test_pubkey() -> PublicKey {
        PublicKey::from_openssh(TEST_ED25519_PUB_OPENSSH).expect("test key 1 parses")
    }

    fn test_pubkey_2() -> PublicKey {
        PublicKey::from_openssh(TEST_ED25519_PUB_OPENSSH_2).expect("test key 2 parses")
    }

    #[test]
    fn parse_policy_variants() {
        assert_eq!(HostKeyPolicy::parse("accept-new"), HostKeyPolicy::AcceptNew);
        assert_eq!(HostKeyPolicy::parse("tofu"), HostKeyPolicy::AcceptNew);
        assert_eq!(HostKeyPolicy::parse(""), HostKeyPolicy::AcceptNew);
        assert_eq!(HostKeyPolicy::parse("strict"), HostKeyPolicy::Strict);
        assert_eq!(HostKeyPolicy::parse("yes"), HostKeyPolicy::Strict);
        assert_eq!(HostKeyPolicy::parse("disabled"), HostKeyPolicy::Disabled);
        assert_eq!(HostKeyPolicy::parse("off"), HostKeyPolicy::Disabled);
        // Unknown falls back to AcceptNew (safe default).
        assert_eq!(HostKeyPolicy::parse("nonsense"), HostKeyPolicy::AcceptNew);
    }

    #[test]
    fn fingerprint_is_sha256_prefixed() {
        let pk = test_pubkey();
        let fp = fingerprint(&pk);
        assert!(fp.starts_with("SHA256:"), "got: {fp}");
        // Base64 (unpadded) of a 32-byte SHA-256 digest is 43 chars.
        assert_eq!(fp.len(), "SHA256:".len() + 43);
    }

    #[test]
    fn tofu_first_contact_adds_entry() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("known_hosts");
        let pk = test_pubkey();

        // No known_hosts yet — should be Added.
        match verify_server_key(
            "host.example.com",
            &pk,
            HostKeyPolicy::AcceptNew,
            Some(&path),
        ) {
            VerifyOutcome::Added => {}
            other => panic!("expected Added, got {other:?}"),
        }
        // File should now exist and contain the host.
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("host.example.com"), "content: {content}");

        // Second verify with the SAME key should be Matched.
        match verify_server_key(
            "host.example.com",
            &pk,
            HostKeyPolicy::AcceptNew,
            Some(&path),
        ) {
            VerifyOutcome::Matched => {}
            other => panic!("expected Matched, got {other:?}"),
        }
    }

    #[test]
    fn mismatched_key_is_rejected() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("known_hosts");
        let pk1 = test_pubkey();
        let pk2 = test_pubkey_2();

        // First contact with pk1.
        verify_server_key(
            "host.example.com",
            &pk1,
            HostKeyPolicy::AcceptNew,
            Some(&path),
        );
        // Second contact with a DIFFERENT key should mismatch.
        match verify_server_key(
            "host.example.com",
            &pk2,
            HostKeyPolicy::AcceptNew,
            Some(&path),
        ) {
            VerifyOutcome::Mismatch { .. } => {}
            other => panic!("expected Mismatch, got {other:?}"),
        }
    }

    #[test]
    fn strict_rejects_unknown_host() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("known_hosts");
        let pk = test_pubkey();

        match verify_server_key("unknown.host", &pk, HostKeyPolicy::Strict, Some(&path)) {
            VerifyOutcome::UnknownHost => {}
            other => panic!("expected UnknownHost, got {other:?}"),
        }
    }

    #[test]
    fn disabled_always_succeeds() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("known_hosts");
        let pk = test_pubkey();

        match verify_server_key("any.host", &pk, HostKeyPolicy::Disabled, Some(&path)) {
            VerifyOutcome::Skipped => {}
            other => panic!("expected Skipped, got {other:?}"),
        }
        // Should NOT have written anything.
        assert!(!path.exists());
    }
}
