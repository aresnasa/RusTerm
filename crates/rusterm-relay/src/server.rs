//! The axum HTTP front-end. Routes:
//!
//! - `GET  /api/v1/health`            — unauthenticated liveness probe
//! - `GET  /api/v1/hosts`             — list saved hosts the account may see
//! - `POST /api/v1/exec`              — validate + execute one command
//! - `POST /api/v1/parse-curl`        — turn a pasted curl command into JSON
//!
//! All routes except `/health` require HTTP Basic auth (Argon2-verified) and
//! are throttled by the shared [`RateLimiter`].

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use crate::audit::{AuditAction, AuditEntry, AuditLog, AuditOutcome, now_iso};
use crate::auth::{RateLimiter, authenticate, parse_basic_auth};
use crate::config::RelayConfig;
#[cfg(test)]
use crate::executor::NullExecutor;
use crate::executor::{ExecOutcome, ExecutorError, HostInfo, RelayExecutor};
use crate::validator::{CommandValidator, ValidationError, compile_allowlist};

/// Shared per-process state handed to every handler.
#[derive(Clone)]
struct AppState {
    config: Arc<parking_lot::RwLock<RelayConfig>>,
    executor: Arc<dyn RelayExecutor>,
    validator: Arc<CommandValidator>,
    limiter: RateLimiter,
    audit: Arc<AuditLog>,
}

/// Handle returned by [`run`]. Dropping it does NOT stop the server — call
/// [`RelayHandle::shutdown`] for a graceful stop.
pub struct RelayHandle {
    pub bound_addr: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl std::fmt::Debug for RelayHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayHandle")
            .field("bound_addr", &self.bound_addr)
            .finish_non_exhaustive()
    }
}

impl RelayHandle {
    pub fn url(&self) -> String {
        format!("http://{}", self.bound_addr)
    }

    /// Gracefully stop the server and wait for the task to exit.
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        let _ = (&mut self.task).await;
    }
}

/// Start the relay. Binds first (so port conflicts are reported before any
/// state changes), then spawns the serving task.
pub async fn run(
    config: RelayConfig,
    executor: Arc<dyn RelayExecutor>,
) -> anyhow::Result<RelayHandle> {
    let bind = SocketAddr::new(config.bind_addr, config.port);
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .map_err(|e| anyhow::anyhow!("cannot bind relay at {bind}: {e} (port already in use?)"))?;
    let bound_addr = listener.local_addr()?;

    let state = AppState {
        config: Arc::new(parking_lot::RwLock::new(config)),
        executor,
        validator: Arc::new(CommandValidator::new()),
        limiter: RateLimiter::new(),
        audit: Arc::new(AuditLog::new()),
    };

    let app = router(state);
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    let task = tokio::spawn(async move {
        let server = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.await;
        });
        if let Err(e) = server.await {
            tracing::error!("[relay] server error: {e}");
        }
    });

    tracing::info!("[relay] listening on http://{bound_addr}");
    Ok(RelayHandle {
        bound_addr,
        shutdown: Some(shutdown_tx),
        task,
    })
}

fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/v1/health", get(health))
        .route("/api/v1/hosts", get(list_hosts))
        .route("/api/v1/exec", post(exec))
        .route("/api/v1/parse-curl", post(parse_curl_handler))
        .with_state(state)
}

// ── responses ────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<&'a str>,
}

fn error_response(status: StatusCode, msg: &str) -> Response {
    error_response_with_code(status, msg, None)
}

fn error_response_with_code<'a>(
    status: StatusCode,
    msg: &'a str,
    code: Option<&'a str>,
) -> Response {
    (status, Json(ErrorBody { error: msg, code })).into_response()
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Basic realm=\"rusterm-relay\"")],
        Json(ErrorBody {
            error: "authentication required",
            code: None,
        }),
    )
        .into_response()
}

fn too_many_requests() -> Response {
    error_response(StatusCode::TOO_MANY_REQUESTS, "rate limit exceeded")
}

// ── auth plumbing ────────────────────────────────────────────────────────

/// Extract credentials from headers, verify them, enforce the auth-failure
/// throttle. Returns the account on success.
fn require_account(
    state: &AppState,
    headers: &HeaderMap,
    client_ip: IpAddr,
) -> Result<crate::config::RelayAccount, Response> {
    if state.limiter.is_auth_throttled(client_ip) {
        state.audit.log(AuditEntry {
            ts: now_iso(),
            account: String::new(),
            client_ip: client_ip.to_string(),
            action: AuditAction::AuthFailure,
            host_id: None,
            command: None,
            outcome: AuditOutcome::rejected("throttled"),
        });
        return Err(too_many_requests());
    }

    let header = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    let creds = header.and_then(parse_basic_auth);

    let (username, password) = match creds {
        Some(c) => c,
        None => return Err(unauthorized()),
    };

    let config = state.config.read();
    match authenticate(&config, &username, &password) {
        Some(account) => Ok(account),
        None => {
            let throttled = state.limiter.record_auth_failure(client_ip);
            state.audit.log(AuditEntry {
                ts: now_iso(),
                account: username,
                client_ip: client_ip.to_string(),
                action: AuditAction::AuthFailure,
                host_id: None,
                command: None,
                outcome: AuditOutcome::rejected(if throttled {
                    "bad credentials (now throttled)"
                } else {
                    "bad credentials"
                }),
            });
            Err(unauthorized())
        }
    }
}

/// Whether the account may *see* and *target* this host.
fn host_allowed(account: &crate::config::RelayAccount, host: &HostInfo) -> bool {
    account.allowed_hosts.is_empty()
        || account
            .allowed_hosts
            .iter()
            .any(|h| h == &host.id || h == &host.name)
}

// ── handlers ─────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct HealthBody<'a> {
    status: &'a str,
    version: &'a str,
}

async fn health() -> Json<HealthBody<'static>> {
    Json(HealthBody {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
    })
}

async fn list_hosts(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    let account = match require_account(&state, &headers, addr.ip()) {
        Ok(a) => a,
        Err(r) => return r,
    };
    let hosts: Vec<HostInfo> = state
        .executor
        .list_hosts()
        .await
        .into_iter()
        .filter(|h| host_allowed(&account, h))
        .collect();
    Json(hosts).into_response()
}

#[derive(Debug, Deserialize)]
struct ExecRequest {
    host_id: String,
    /// Single-line command. Mutually exclusive with `script` and
    /// `script_base64` — exactly one of the three must be present.
    #[serde(default)]
    command: Option<String>,
    /// Multi-line script, validated by
    /// [`CommandValidator::validate_script`] and pre-flighted by
    /// [`crate::sandbox::preflight`] before reaching the executor.
    #[serde(default)]
    script: Option<String>,
    /// Base64-encoded script. Decoded to UTF-8 before validation. Accepts
    /// standard and URL-safe alphabets, with or without padding.
    #[serde(default)]
    script_base64: Option<String>,
    /// Execute through sudo. False by default for backward compatibility and
    /// to avoid silently elevating every API command.
    #[serde(default)]
    elevated: bool,
    /// Per-request deadline in milliseconds. Capped by the relay's
    /// `request_timeout_ms`.
    timeout_ms: Option<u64>,
}

impl ExecRequest {
    /// Resolve the request into exactly one command/script string.
    ///
    /// Returns:
    /// - `Ok((payload, is_script))` — `payload` is the command or decoded
    ///   script to forward; `is_script` flags whether the script pipeline
    ///   (validate_script + sandbox) should run.
    /// - `Err(ResolveError)` — the client sent zero or more than one of
    ///   `command`/`script`/`script_base64`, or the base64 was invalid.
    fn resolve_payload(&self) -> Result<(String, bool), ResolveError> {
        let present = [
            self.command.is_some(),
            self.script.is_some(),
            self.script_base64.is_some(),
        ];
        let count = present.iter().filter(|&&b| b).count();
        if count == 0 {
            return Err(ResolveError::MissingPayload);
        }
        if count > 1 {
            return Err(ResolveError::MultiplePayloads);
        }
        if let Some(cmd) = &self.command {
            return Ok((cmd.clone(), false));
        }
        if let Some(script) = &self.script {
            return Ok((script.clone(), true));
        }
        // script_base64 is the only remaining option.
        let encoded = self.script_base64.as_ref().expect("checked count above");
        let decoded =
            crate::validator::decode_script_base64(encoded).map_err(ResolveError::Base64Invalid)?;
        Ok((decoded, true))
    }
}

/// Error resolving an [`ExecRequest`] into a single payload. Mapped to HTTP
/// 400 with a structured `code` field so clients can distinguish the cases.
enum ResolveError {
    MissingPayload,
    MultiplePayloads,
    Base64Invalid(crate::validator::ScriptError),
}

impl ResolveError {
    fn code(&self) -> &'static str {
        match self {
            ResolveError::MissingPayload => "missing_payload",
            ResolveError::MultiplePayloads => "multiple_payloads",
            ResolveError::Base64Invalid(_) => "base64_invalid",
        }
    }
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolveError::MissingPayload => {
                write!(
                    f,
                    "request must include one of command, script, script_base64"
                )
            }
            ResolveError::MultiplePayloads => {
                write!(
                    f,
                    "command, script, and script_base64 are mutually exclusive"
                )
            }
            ResolveError::Base64Invalid(e) => write!(f, "{e}"),
        }
    }
}

#[derive(Serialize)]
struct ExecResponse {
    exit_code: Option<u32>,
    stdout: String,
    stderr: String,
    timed_out: bool,
    duration_ms: u64,
}

async fn exec(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<ExecRequest>,
) -> Response {
    let account = match require_account(&state, &headers, addr.ip()) {
        Ok(a) => a,
        Err(r) => return r,
    };

    // Resolve command XOR script XOR script_base64 into a single payload.
    // Errors here are 400 — the request was malformed, not unauthorized.
    let (payload, is_script) = match body.resolve_payload() {
        Ok(v) => v,
        Err(e) => {
            state.audit.log(AuditEntry {
                ts: now_iso(),
                account: account.username.clone(),
                client_ip: addr.ip().to_string(),
                action: if is_script_field_set(&body) {
                    AuditAction::ScriptRejected
                } else {
                    AuditAction::ExecRejected
                },
                host_id: Some(body.host_id.clone()),
                command: None,
                outcome: AuditOutcome::rejected(e.to_string()),
            });
            return error_response_with_code(
                StatusCode::BAD_REQUEST,
                &e.to_string(),
                Some(e.code()),
            );
        }
    };

    let config = state.config.read().clone();
    if !state
        .limiter
        .allow_exec(&account.username, config.per_account_rate_limit)
    {
        state.audit.log(AuditEntry {
            ts: now_iso(),
            account: account.username.clone(),
            client_ip: addr.ip().to_string(),
            action: if is_script {
                AuditAction::ScriptRejected
            } else {
                AuditAction::ExecRejected
            },
            host_id: Some(body.host_id.clone()),
            command: Some(payload.clone()),
            outcome: AuditOutcome::rejected("account rate limit exceeded"),
        });
        return too_many_requests();
    }

    // Validate: safety checker + API deny-list + readonly + allowlist.
    // For scripts, use the richer validate_script pipeline (per-line hard
    // floor + script injection patterns + dcg). For commands, use the
    // single-command validator (backward compatible).
    let allowlist = match compile_allowlist(&account.allowed_commands) {
        Ok(v) => v,
        Err(errors) => {
            tracing::warn!(
                "[relay] account {} has invalid allowlist patterns: {:?}",
                account.username,
                errors
            );
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "account allowlist has invalid regex — fix relay.json",
            );
        }
    };
    let validation = if is_script {
        state
            .validator
            .validate_script(&payload, &allowlist, account.readonly)
            .map_err(|e| {
                // Map ScriptError to a ValidationError-shaped string for
                // the audit log. We keep the variant for the HTTP code.
                match e {
                    crate::validator::ScriptError::DcgBlocked(reason) => {
                        ValidationError::Dangerous(format!("dcg blocked: {reason}"))
                    }
                    other => ValidationError::Dangerous(other.to_string()),
                }
            })
    } else {
        state
            .validator
            .validate(&payload, &allowlist, account.readonly)
    };
    if let Err(validation) = validation {
        let is_dcg_block = validation.to_string().contains("dcg blocked");
        state.audit.log(AuditEntry {
            ts: now_iso(),
            account: account.username.clone(),
            client_ip: addr.ip().to_string(),
            action: if is_dcg_block {
                AuditAction::DcgBlocked
            } else if is_script {
                AuditAction::ScriptRejected
            } else {
                AuditAction::ExecRejected
            },
            host_id: Some(body.host_id.clone()),
            command: Some(payload.clone()),
            outcome: AuditOutcome::rejected(validation.to_string()),
        });
        return error_response(StatusCode::FORBIDDEN, &validation.to_string());
    }

    // Script-only: sandbox pre-flight (syntax check + dcg). The hard floor
    // has already run in validate_script; this stage adds the syntax check
    // and a second dcg pass over the whole script (dcg's AST analysis can
    // catch multi-line constructs the per-line pass doesn't see).
    if is_script {
        match crate::sandbox::preflight(&payload) {
            crate::sandbox::SandboxVerdict::Safe { note } => {
                tracing::debug!("[relay] script sandbox preflight ok: {note}");
            }
            crate::sandbox::SandboxVerdict::Unsafe { reason } => {
                state.audit.log(AuditEntry {
                    ts: now_iso(),
                    account: account.username.clone(),
                    client_ip: addr.ip().to_string(),
                    action: AuditAction::SandboxFailed,
                    host_id: Some(body.host_id.clone()),
                    command: Some(payload.clone()),
                    outcome: AuditOutcome::rejected(reason.clone()),
                });
                return error_response_with_code(
                    StatusCode::FORBIDDEN,
                    &reason,
                    Some("sandbox_failed"),
                );
            }
        }
    }

    // Host authorization happens against the executor's host list.
    let hosts = state.executor.list_hosts().await;
    let host = match hosts
        .iter()
        .find(|h| h.id == body.host_id || h.name == body.host_id)
    {
        Some(h) if host_allowed(&account, h) => h.clone(),
        Some(_) => {
            state.audit.log(AuditEntry {
                ts: now_iso(),
                account: account.username.clone(),
                client_ip: addr.ip().to_string(),
                action: if is_script {
                    AuditAction::ScriptRejected
                } else {
                    AuditAction::ExecRejected
                },
                host_id: Some(body.host_id.clone()),
                command: Some(payload.clone()),
                outcome: AuditOutcome::rejected("host not allowed for account"),
            });
            return error_response(StatusCode::FORBIDDEN, "host not allowed for this account");
        }
        None => {
            return error_response(StatusCode::NOT_FOUND, "unknown host_id");
        }
    };

    let timeout = Duration::from_millis(
        body.timeout_ms
            .unwrap_or(config.request_timeout_ms)
            .min(config.request_timeout_ms),
    );

    state.audit.log(AuditEntry {
        ts: now_iso(),
        account: account.username.clone(),
        client_ip: addr.ip().to_string(),
        action: if is_script {
            AuditAction::ScriptAccepted
        } else {
            AuditAction::ExecAccepted
        },
        host_id: Some(host.id.clone()),
        command: Some(payload.clone()),
        outcome: AuditOutcome {
            success: true,
            exit_code: None,
            reason: None,
            duration_ms: None,
        },
    });

    let started = Instant::now();
    match state
        .executor
        .exec(&host.id, &payload, body.elevated, timeout)
        .await
    {
        Ok(ExecOutcome {
            exit_code,
            stdout,
            stderr,
            timed_out,
            duration_ms,
        }) => {
            state.audit.log(AuditEntry {
                ts: now_iso(),
                account: account.username.clone(),
                client_ip: addr.ip().to_string(),
                action: if is_script {
                    AuditAction::ScriptAccepted
                } else {
                    AuditAction::ExecAccepted
                },
                host_id: Some(host.id.clone()),
                command: Some(payload.clone()),
                outcome: AuditOutcome::ok(exit_code, duration_ms),
            });
            Json(ExecResponse {
                exit_code,
                stdout,
                stderr,
                timed_out,
                duration_ms,
            })
            .into_response()
        }
        Err(err) => {
            if let ExecutorError::ElevationRequired(message) = &err {
                state.audit.log(AuditEntry {
                    ts: now_iso(),
                    account: account.username.clone(),
                    client_ip: addr.ip().to_string(),
                    action: AuditAction::ExecFailed,
                    host_id: Some(host.id.clone()),
                    command: Some(payload.clone()),
                    outcome: AuditOutcome {
                        success: false,
                        exit_code: None,
                        reason: Some("elevation required".to_string()),
                        duration_ms: Some(started.elapsed().as_millis() as u64),
                    },
                });
                return error_response_with_code(
                    StatusCode::FORBIDDEN,
                    message,
                    Some("elevation_required"),
                );
            }
            let (status, msg) = match &err {
                ExecutorError::UnknownHost(_) => (StatusCode::NOT_FOUND, "unknown host_id"),
                ExecutorError::Connect(_) => (StatusCode::BAD_GATEWAY, "SSH connect failed"),
                ExecutorError::Exec(_) => (StatusCode::BAD_GATEWAY, "remote exec failed"),
                ExecutorError::ElevationRequired(_) => unreachable!(),
            };
            state.audit.log(AuditEntry {
                ts: now_iso(),
                account: account.username.clone(),
                client_ip: addr.ip().to_string(),
                action: AuditAction::ExecFailed,
                host_id: Some(host.id.clone()),
                command: Some(payload.clone()),
                outcome: AuditOutcome {
                    success: false,
                    exit_code: None,
                    reason: Some(err.to_string()),
                    duration_ms: Some(started.elapsed().as_millis() as u64),
                },
            });
            error_response(status, msg)
        }
    }
}

/// Whether the request carried a `script` or `script_base64` field (even if
/// invalid). Used to pick the right audit action for resolve errors.
fn is_script_field_set(body: &ExecRequest) -> bool {
    body.script.is_some() || body.script_base64.is_some()
}

#[derive(Debug, Deserialize)]
struct ParseCurlRequest {
    curl: String,
}

async fn parse_curl_handler(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<ParseCurlRequest>,
) -> Response {
    let account = match require_account(&state, &headers, addr.ip()) {
        Ok(a) => a,
        Err(r) => return r,
    };
    match crate::curl::parse_curl(&body.curl) {
        Ok(parsed) => {
            state.audit.log(AuditEntry {
                ts: now_iso(),
                account: account.username,
                client_ip: addr.ip().to_string(),
                action: AuditAction::ParseCurl,
                host_id: None,
                command: None,
                outcome: AuditOutcome {
                    success: true,
                    exit_code: None,
                    reason: None,
                    duration_ms: None,
                },
            });
            Json(parsed).into_response()
        }
        Err(e) => error_response(StatusCode::BAD_REQUEST, &e.to_string()),
    }
}

// ── tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{RelayAccount, hash_password};

    #[derive(Debug, Default)]
    struct RecordingExecutor {
        elevated: std::sync::atomic::AtomicBool,
        fail_elevation: bool,
    }

    #[async_trait::async_trait]
    impl RelayExecutor for RecordingExecutor {
        async fn list_hosts(&self) -> Vec<HostInfo> {
            vec![HostInfo {
                id: "host-1".to_string(),
                name: "prod".to_string(),
                host: "127.0.0.1".to_string(),
                port: 22,
                username: "ops".to_string(),
            }]
        }

        async fn exec(
            &self,
            _host_id: &str,
            _command: &str,
            elevated: bool,
            _timeout: Duration,
        ) -> Result<ExecOutcome, ExecutorError> {
            self.elevated
                .store(elevated, std::sync::atomic::Ordering::SeqCst);
            if elevated && self.fail_elevation {
                return Err(ExecutorError::ElevationRequired(
                    "No reusable sudo authorization is available".to_string(),
                ));
            }
            Ok(ExecOutcome {
                exit_code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
                timed_out: false,
                duration_ms: 1,
            })
        }
    }

    fn test_config() -> RelayConfig {
        let mut cfg = RelayConfig::default();
        // Port 0 → kernel assigns a free port; tests can run in parallel.
        cfg.port = 0;
        cfg.accounts.push(RelayAccount {
            username: "ops".into(),
            password_hash: hash_password("pw").unwrap(),
            ..Default::default()
        });
        cfg
    }

    #[tokio::test]
    async fn health_needs_no_auth() {
        let executor: Arc<dyn RelayExecutor> = Arc::new(NullExecutor);
        let handle = run(test_config(), executor).await.unwrap();
        let url = format!("{}/api/v1/health", handle.url());
        let body: serde_json::Value = reqwest_get(&url, None).await;
        assert_eq!(body["status"], "ok");
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn hosts_requires_auth() {
        let executor: Arc<dyn RelayExecutor> = Arc::new(NullExecutor);
        let handle = run(test_config(), executor).await.unwrap();
        let url = format!("{}/api/v1/hosts", handle.url());
        let (status, _) = reqwest_get_status(&url, None).await;
        assert_eq!(status, 401);
        let (status, _) = reqwest_get_status(&url, Some(("ops", "wrong"))).await;
        assert_eq!(status, 401);
        let (status, body) = reqwest_get_status(&url, Some(("ops", "pw"))).await;
        assert_eq!(status, 200);
        assert!(body.is_array());
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn exec_rejects_dangerous_and_unknown_hosts() {
        let executor: Arc<dyn RelayExecutor> = Arc::new(NullExecutor);
        let handle = run(test_config(), executor).await.unwrap();
        let base = handle.url();

        // Dangerous command → 403 even before host lookup.
        let status = post_exec(&base, "ops", "pw", "h1", "rm -rf /", None).await;
        assert_eq!(status, 403);
        // Unknown host → the NullExecutor has no hosts.
        let status = post_exec(&base, "ops", "pw", "ghost", "uptime", None).await;
        assert_eq!(status, 404);
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn exec_forwards_explicit_elevation_request() {
        let executor = Arc::new(RecordingExecutor::default());
        let handle = run(test_config(), executor.clone()).await.unwrap();
        let (status, _) = post_json(
            &format!("{}/api/v1/exec", handle.url()),
            Some(("ops", "pw")),
            &serde_json::json!({
                "host_id": "host-1",
                "command": "docker ps",
                "elevated": true,
            }),
        )
        .await;
        assert_eq!(status, 200);
        assert!(executor.elevated.load(std::sync::atomic::Ordering::SeqCst));
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn elevation_failure_returns_structured_error() {
        let executor = Arc::new(RecordingExecutor {
            fail_elevation: true,
            ..Default::default()
        });
        let handle = run(test_config(), executor).await.unwrap();
        let (status, body) = post_json(
            &format!("{}/api/v1/exec", handle.url()),
            Some(("ops", "pw")),
            &serde_json::json!({
                "host_id": "host-1",
                "command": "docker ps",
                "elevated": true,
            }),
        )
        .await;
        assert_eq!(status, 403);
        assert_eq!(body["code"], "elevation_required");
        assert_eq!(body["error"], "No reusable sudo authorization is available");
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn exec_defaults_to_non_elevated_for_legacy_clients() {
        let executor = Arc::new(RecordingExecutor::default());
        let handle = run(test_config(), executor.clone()).await.unwrap();
        let (status, _) = post_json(
            &format!("{}/api/v1/exec", handle.url()),
            Some(("ops", "pw")),
            &serde_json::json!({
                "host_id": "host-1",
                "command": "uptime",
            }),
        )
        .await;
        assert_eq!(status, 200);
        assert!(!executor.elevated.load(std::sync::atomic::Ordering::SeqCst));
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn parse_curl_roundtrip() {
        let executor: Arc<dyn RelayExecutor> = Arc::new(NullExecutor);
        let handle = run(test_config(), executor).await.unwrap();
        let url = format!("{}/api/v1/parse-curl", handle.url());
        let (status, body) = post_json(
            &url,
            Some(("ops", "pw")),
            &serde_json::json!({"curl": "curl -X POST https://x -d a=1"}),
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(body["method"], "POST");
        assert_eq!(body["url"], "https://x");
        assert_eq!(body["body"], "a=1");
        handle.shutdown().await;
    }

    // Minimal blocking-free HTTP helpers implemented over raw TCP so tests
    // don't need reqwest in dev-dependencies.
    async fn raw_request(
        url: &str,
        method: &str,
        auth: Option<(&str, &str)>,
        body: Option<String>,
    ) -> (u16, serde_json::Value) {
        use base64::Engine;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let parsed = url::Url::parse(url).unwrap();
        let host = parsed.host_str().unwrap().to_string();
        let port = parsed.port().unwrap();
        let path = parsed.path();
        let mut stream = tokio::net::TcpStream::connect((host.as_str(), port))
            .await
            .unwrap();
        let mut request = format!("{method} {path} HTTP/1.1\r\nHost: {host}:{port}\r\n");
        if let Some((u, p)) = auth {
            let creds = base64::engine::general_purpose::STANDARD.encode(format!("{u}:{p}"));
            request.push_str(&format!("Authorization: Basic {creds}\r\n"));
        }
        if let Some(body) = &body {
            request.push_str("Content-Type: application/json\r\n");
            request.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        request.push_str("Connection: close\r\n\r\n");
        if let Some(body) = &body {
            request.push_str(body);
        }
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        let text = String::from_utf8_lossy(&response);
        let status: u16 = text
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap();
        let json_start = text.find("\r\n\r\n").map(|i| i + 4).unwrap();
        let body_text = &text[json_start..];
        let json = serde_json::from_str(body_text.trim_end_matches('\0')).unwrap_or_default();
        (status, json)
    }

    async fn reqwest_get(url: &str, auth: Option<(&str, &str)>) -> serde_json::Value {
        reqwest_get_status(url, auth).await.1
    }

    async fn reqwest_get_status(url: &str, auth: Option<(&str, &str)>) -> (u16, serde_json::Value) {
        raw_request(url, "GET", auth, None).await
    }

    async fn post_exec(
        base: &str,
        user: &str,
        pass: &str,
        host: &str,
        command: &str,
        timeout_ms: Option<u64>,
    ) -> u16 {
        let (status, _) = raw_request(
            &format!("{base}/api/v1/exec"),
            "POST",
            Some((user, pass)),
            Some(
                serde_json::json!({
                    "host_id": host,
                    "command": command,
                    "timeout_ms": timeout_ms,
                })
                .to_string(),
            ),
        )
        .await;
        status
    }

    async fn post_json(
        url: &str,
        auth: Option<(&str, &str)>,
        body: &serde_json::Value,
    ) -> (u16, serde_json::Value) {
        raw_request(url, "POST", auth, Some(body.to_string())).await
    }
}
