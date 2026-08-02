//! HTTP Basic authentication + abuse throttling for the relay.
//!
//! Two layers:
//!
//! 1. [`parse_basic_auth`] / [`authenticate`] — verify `Authorization:
//!    Basic ...` against the Argon2id hashes in [`RelayConfig`]. Unknown
//!    users and bad passwords both return `None`, and a dummy verify is run
//!    for unknown users to even out timing.
//! 2. [`RateLimiter`] — in-memory sliding-window counters: failed login
//!    attempts per client IP (slows credential stuffing), plus a per-account
//!    exec allowance (stops a stolen credential from hammering hosts).

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use base64::Engine;
use parking_lot::Mutex;

use crate::config::{RelayAccount, RelayConfig, verify_password};

/// Parse an `Authorization: Basic <b64>` header value into
/// `(username, password)`. Returns `None` for anything malformed.
pub fn parse_basic_auth(header_value: &str) -> Option<(String, String)> {
    let encoded = header_value.strip_prefix("Basic ")?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .ok()?;
    let text = String::from_utf8(decoded).ok()?;
    let (user, pass) = text.split_once(':')?;
    if user.is_empty() {
        return None;
    }
    Some((user.to_string(), pass.to_string()))
}

/// A valid credential pair resolved to the owning account.
pub struct AuthenticatedAccount {
    pub username: String,
}

/// Verify credentials against the configured accounts. Both "unknown user"
/// and "wrong password" return `None`; we also run a dummy Argon2 verify
/// for unknown users so response timing doesn't reveal account existence.
pub fn authenticate(config: &RelayConfig, username: &str, password: &str) -> Option<RelayAccount> {
    match config.find_account(username) {
        Some(account) if verify_password(&account.password_hash, password) => Some(account.clone()),
        Some(_) => None,
        None => {
            // Dummy verify to blunt user-enumeration timing attacks.
            verify_password(dummy_hash(), password);
            None
        }
    }
}

/// A stable Argon2id hash generated once per process. Used only as a
/// timing-equalization verify target for unknown usernames; the plaintext
/// is irrelevant.
fn dummy_hash() -> &'static str {
    static HASH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    HASH.get_or_init(|| {
        crate::config::hash_password("rusterm-relay-dummy-password")
            .unwrap_or_else(|_| "$argon2id$v=19$m=19456,t=2,p=1$invalid$invalid".to_string())
    })
}

// ── Rate limiting ────────────────────────────────────────────────────────

/// Sliding window length for both limiters.
const WINDOW: Duration = Duration::from_secs(60);
/// Failed-auth budget per IP per window. Exceeding it returns 429 for the
/// rest of the window.
const AUTH_FAILURE_BUDGET: u32 = 5;

#[derive(Debug, Default)]
struct WindowCounter {
    /// Window start.
    start: Option<Instant>,
    count: u32,
}

impl WindowCounter {
    /// Record one event and return the count *within the current window*.
    fn record(&mut self) -> u32 {
        let now = Instant::now();
        match self.start {
            Some(start) if now.duration_since(start) < WINDOW => {
                self.count += 1;
            }
            _ => {
                self.start = Some(now);
                self.count = 1;
            }
        }
        self.count
    }
}

/// Thread-safe in-memory rate limiter. Cheap to clone (all state is behind
/// one shared mutex-protected map).
#[derive(Clone, Default)]
pub struct RateLimiter {
    auth_failures: std::sync::Arc<Mutex<HashMap<IpAddr, WindowCounter>>>,
    exec_counts: std::sync::Arc<Mutex<HashMap<String, WindowCounter>>>,
}

impl std::fmt::Debug for RateLimiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RateLimiter").finish_non_exhaustive()
    }
}

impl RateLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Call on every failed authentication. Returns `true` when the client
    /// IP has exhausted its failure budget and should be throttled (429).
    pub fn record_auth_failure(&self, ip: IpAddr) -> bool {
        let count = self.auth_failures.lock().entry(ip).or_default().record();
        count > AUTH_FAILURE_BUDGET
    }

    /// Check whether an IP is currently over the failure budget without
    /// recording another failure (used to short-circuit before hashing).
    pub fn is_auth_throttled(&self, ip: IpAddr) -> bool {
        let map = self.auth_failures.lock();
        match map.get(&ip) {
            Some(counter) => counter
                .start
                .is_some_and(|s| s.elapsed() < WINDOW && counter.count > AUTH_FAILURE_BUDGET),
            None => false,
        }
    }

    /// Record an exec request for `account`. Returns `true` if allowed.
    pub fn allow_exec(&self, account: &str, per_minute: u32) -> bool {
        let count = self
            .exec_counts
            .lock()
            .entry(account.to_string())
            .or_default()
            .record();
        count <= per_minute
    }
}

// ── tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::hash_password;

    fn config_with_user(user: &str, pass: &str) -> RelayConfig {
        let mut cfg = RelayConfig::default();
        cfg.accounts.push(RelayAccount {
            username: user.to_string(),
            password_hash: hash_password(pass).unwrap(),
            ..Default::default()
        });
        cfg
    }

    #[test]
    fn parse_basic_auth_ok() {
        let userpass = base64::engine::general_purpose::STANDARD.encode("alice:wonder");
        let header = format!("Basic {userpass}");
        assert_eq!(
            parse_basic_auth(&header),
            Some(("alice".to_string(), "wonder".to_string()))
        );
    }

    #[test]
    fn parse_basic_auth_password_may_contain_colons() {
        let userpass = base64::engine::general_purpose::STANDARD.encode("a:p:q:");
        let header = format!("Basic {userpass}");
        assert_eq!(
            parse_basic_auth(&header),
            Some(("a".to_string(), "p:q:".to_string()))
        );
    }

    #[test]
    fn parse_basic_auth_rejects_malformed() {
        assert!(parse_basic_auth("Bearer xyz").is_none());
        assert!(parse_basic_auth("Basic !!!not-base64!!!").is_none());
        assert!(parse_basic_auth("Basic").is_none());
        let no_colon = base64::engine::general_purpose::STANDARD.encode("nocolon");
        assert!(parse_basic_auth(&format!("Basic {no_colon}")).is_none());
        let empty_user = base64::engine::general_purpose::STANDARD.encode(":pass");
        assert!(parse_basic_auth(&format!("Basic {empty_user}")).is_none());
    }

    #[test]
    fn authenticate_happy_and_negative_paths() {
        let cfg = config_with_user("ops", "s3cret");
        assert!(authenticate(&cfg, "ops", "s3cret").is_some());
        assert!(authenticate(&cfg, "ops", "wrong").is_none());
        assert!(authenticate(&cfg, "ghost", "s3cret").is_none());
        assert!(authenticate(&cfg, "", "").is_none());
    }

    #[test]
    fn auth_throttle_after_budget_exhausted() {
        let limiter = RateLimiter::new();
        let ip: IpAddr = "10.0.0.9".parse().unwrap();
        assert!(!limiter.is_auth_throttled(ip));
        for _ in 0..AUTH_FAILURE_BUDGET {
            assert!(!limiter.record_auth_failure(ip));
        }
        // Sixth failure flips into throttled state.
        assert!(limiter.record_auth_failure(ip));
        assert!(limiter.is_auth_throttled(ip));
        // A different IP is unaffected.
        let other: IpAddr = "10.0.0.10".parse().unwrap();
        assert!(!limiter.is_auth_throttled(other));
    }

    #[test]
    fn exec_budget_respected() {
        let limiter = RateLimiter::new();
        for _ in 0..3 {
            assert!(limiter.allow_exec("ops", 3));
        }
        assert!(!limiter.allow_exec("ops", 3));
        // Other accounts unaffected.
        assert!(limiter.allow_exec("ci", 3));
    }
}
