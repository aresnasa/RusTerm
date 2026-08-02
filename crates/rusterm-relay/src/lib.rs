//! `rusterm-relay` — an authenticated HTTP front-end that relays validated
//! commands to saved SSH hosts ("中转站").
//!
//! # Security model
//!
//! - **BasicAuth over multiple accounts** defined in `relay.json`. Passwords
//!   are stored only as Argon2id PHC strings.
//! - **Target restriction**: accounts may only execute against
//!   previously-saved SSH hosts — the request body can never supply host,
//!   port or credentials. This is the primary anti-abuse property.
//! - **Command validation** layers the interactive terminal's danger
//!   patterns (all hard-blocked here, there is no "confirm" step for an
//!   unattended API) with an API-specific deny-list and per-account regex
//!   allowlists + read-only flags. See [`validator`].
//! - **Rate limiting**: per-IP auth-failure throttle (429 after 5/min) and
//!   per-account exec budgets. See [`auth::RateLimiter`].
//! - **Audit**: every auth failure and every exec attempt is appended to
//!   `relay-audit.jsonl` in the app log directory. See [`audit`].
//! - Binds `127.0.0.1` by default; `0.0.0.0` requires explicit UI
//!   confirmation ([`config::RelayConfig::binds_publicly`]).
//!
//! # Endpoints
//!
//! - `GET  /api/v1/health`     — liveness, no auth
//! - `GET  /api/v1/hosts`      — hosts visible to the account
//! - `POST /api/v1/exec`       — `{host_id, command, elevated?, timeout_ms?}` → result
//! - `POST /api/v1/parse-curl` — parse a pasted curl command into JSON

pub mod audit;
pub mod auth;
pub mod config;
pub mod curl;
pub mod executor;
pub mod server;
pub mod validator;

pub use audit::{AuditAction, AuditEntry, AuditLog, AuditOutcome};
pub use auth::{RateLimiter, authenticate, parse_basic_auth};
pub use config::{DEFAULT_PORT, RelayAccount, RelayConfig, hash_password, verify_password};
pub use curl::{CurlParseError, ParsedCurl, parse_curl};
pub use executor::{ExecOutcome, ExecutorError, HostInfo, NullExecutor, RelayExecutor};
pub use server::{RelayHandle, run};
pub use validator::{CommandValidator, ValidationError, compile_allowlist};
