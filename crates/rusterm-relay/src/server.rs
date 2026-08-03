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

use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use crate::audit::{AuditAction, AuditEntry, AuditLog, AuditOutcome, now_iso};
use crate::auth::{RateLimiter, authenticate, parse_basic_auth};
use crate::command_guard::BlocklistConfig;
use crate::config::RelayConfig;
#[cfg(test)]
use crate::executor::NullExecutor;
use crate::executor::{ExecOutcome, ExecutorError, HostInfo, RelayExecutor};
use crate::history::{
    HistoryCursor, HistoryQuery, RelayHistoryRecord, RelayHistoryStore, new_record_id,
};
#[cfg(test)]
use crate::history::{NullHistoryStore, RecordingHistoryStore};
use crate::validator::{CommandValidator, ValidationError, compile_allowlist};

/// Shared per-process state handed to every handler.
#[derive(Clone)]
struct AppState {
    config: Arc<parking_lot::RwLock<RelayConfig>>,
    executor: Arc<dyn RelayExecutor>,
    validator: Arc<CommandValidator>,
    limiter: RateLimiter,
    audit: Arc<AuditLog>,
    history: Arc<dyn RelayHistoryStore>,
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
///
/// The dangerous-command blocklist is loaded from `relay-blocklist.json`
/// (if present) and merged into the validator. The hardcoded catastrophic
/// patterns always apply regardless of this file — it can only *add*
/// restrictions.
pub async fn run(
    config: RelayConfig,
    executor: Arc<dyn RelayExecutor>,
    history: Arc<dyn RelayHistoryStore>,
) -> anyhow::Result<RelayHandle> {
    let bind = SocketAddr::new(config.bind_addr, config.port);
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .map_err(|e| anyhow::anyhow!("cannot bind relay at {bind}: {e} (port already in use?)"))?;
    let bound_addr = listener.local_addr()?;

    // Load the user/skill blocklist. Missing file = empty (first launch).
    // Invalid regexes are logged but don't abort startup — one bad pattern
    // shouldn't take down the API.
    let blocklist = BlocklistConfig::load().unwrap_or_else(|e| {
        tracing::warn!("[relay] failed to load blocklist config: {e}; using built-ins only");
        BlocklistConfig::default()
    });
    let loaded = blocklist.compile();
    for err in &loaded.errors {
        tracing::warn!(
            "[relay] blocklist pattern from {} failed to compile: {} (regex: {:?})",
            err.source,
            err.error,
            err.regex
        );
    }
    if !loaded.patterns.is_empty() {
        tracing::info!(
            "[relay] loaded {} user/skill blocklist patterns",
            loaded.patterns.len()
        );
    }
    let validator = Arc::new(CommandValidator::new().with_blocklist(loaded));

    let state = AppState {
        config: Arc::new(parking_lot::RwLock::new(config)),
        executor,
        validator,
        limiter: RateLimiter::new(),
        audit: Arc::new(AuditLog::new()),
        history,
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
        .route("/api/v1/history", get(list_history))
        .route("/api/v1/history/{id}", delete(delete_history))
        .route("/api/v1/parse-curl", post(parse_curl_handler))
        // Short-form endpoint: POST /r/{host_id} with plain-text body.
        // Returns plain-text stdout. Designed for one-liner curl usage:
        //   curl -s -u user:pass http://localhost:8877/r/jumpserver -d 'uname -a'
        .route("/r/{host_id}", post(exec_shortform))
        .with_state(state)
}

// ── responses ────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<&'a str>,
}

fn error_response(status: StatusCode, msg: &str) -> Response {
    error_response_with_detail(status, msg, None, None)
}

fn error_response_with_code<'a>(
    status: StatusCode,
    msg: &'a str,
    code: Option<&'a str>,
) -> Response {
    error_response_with_detail(status, msg, code, None)
}

fn error_response_with_detail<'a>(
    status: StatusCode,
    msg: &'a str,
    code: Option<&'a str>,
    detail: Option<&'a str>,
) -> Response {
    (
        status,
        Json(ErrorBody {
            error: msg,
            code,
            detail,
        }),
    )
        .into_response()
}

/// Return only structured bastion-state diagnostics to API clients. Arbitrary
/// SSH errors and raw PTY tails may contain internal paths, banners or echoed
/// input, so they remain in local audit logs instead of crossing the API.
fn safe_executor_detail(error: &ExecutorError) -> Option<String> {
    let ExecutorError::Exec(detail) = error else {
        return None;
    };
    if !detail.contains("[bastion-pre-command]") {
        return None;
    }
    let without_output = detail.split("; last_output=").next().unwrap_or(detail);
    Some(without_output.chars().take(1_000).collect())
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Basic realm=\"rusterm-relay\"")],
        Json(ErrorBody {
            error: "authentication required",
            code: None,
            detail: None,
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
    truncated: bool,
    duration_ms: u64,
}

#[derive(Clone, Copy)]
enum ExecResponseFormat {
    Json,
    PlainText,
}

#[derive(Debug, Default, Deserialize)]
struct ShortExecParams {
    elevated: Option<bool>,
    timeout_ms: Option<u64>,
}

async fn exec(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<ExecRequest>,
) -> Response {
    execute_request(state, addr, headers, body, ExecResponseFormat::Json).await
}

async fn exec_shortform(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Path(host_id): Path<String>,
    Query(params): Query<ShortExecParams>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let request = ExecRequest {
        host_id,
        command: Some(body),
        script: None,
        script_base64: None,
        elevated: params.elevated.unwrap_or(false),
        timeout_ms: params.timeout_ms,
    };
    execute_request(state, addr, headers, request, ExecResponseFormat::PlainText).await
}

async fn execute_request(
    state: AppState,
    addr: SocketAddr,
    headers: HeaderMap,
    body: ExecRequest,
    response_format: ExecResponseFormat,
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
            truncated,
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
            record_history(
                &state,
                &account.username,
                &host.id,
                &payload,
                body.elevated,
                exit_code.map(|c| c as i32),
                Some(duration_ms),
                timed_out,
            );
            match response_format {
                ExecResponseFormat::Json => Json(ExecResponse {
                    exit_code,
                    stdout,
                    stderr,
                    timed_out,
                    truncated,
                    duration_ms,
                })
                .into_response(),
                ExecResponseFormat::PlainText => {
                    let mut response = stdout.into_response();
                    let headers = response.headers_mut();
                    headers.insert(
                        header::CONTENT_TYPE,
                        header::HeaderValue::from_static("text/plain; charset=utf-8"),
                    );
                    headers.insert(
                        "x-rusterm-exit-code",
                        header::HeaderValue::from_str(
                            &exit_code.map_or_else(|| "unknown".to_string(), |code| code.to_string()),
                        )
                        .expect("exit code is a valid header value"),
                    );
                    headers.insert(
                        "x-rusterm-duration-ms",
                        header::HeaderValue::from_str(&duration_ms.to_string())
                            .expect("duration is a valid header value"),
                    );
                    headers.insert(
                        "x-rusterm-timed-out",
                        header::HeaderValue::from_static(if timed_out { "true" } else { "false" }),
                    );
                    headers.insert(
                        "x-rusterm-truncated",
                        header::HeaderValue::from_static(if truncated { "true" } else { "false" }),
                    );
                    response
                }
            }
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
                // The command was dispatched but couldn't run (needed sudo,
                // none cached). Record it as a non-success so the user can
                // see and retry — but with no exit_code, since nothing ran.
                record_history(
                    &state,
                    &account.username,
                    &host.id,
                    &payload,
                    body.elevated,
                    None,
                    Some(started.elapsed().as_millis() as u64),
                    false,
                );
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
            let detail = safe_executor_detail(&err);
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
            // Reached the executor but failed (connect/exec error). Record
            // with no exit_code so the failure is visible and retryable.
            record_history(
                &state,
                &account.username,
                &host.id,
                &payload,
                body.elevated,
                None,
                Some(started.elapsed().as_millis() as u64),
                false,
            );
            error_response_with_detail(status, msg, None, detail.as_deref())
        }
    }
}

/// Whether the request carried a `script` or `script_base64` field (even if
/// invalid). Used to pick the right audit action for resolve errors.
fn is_script_field_set(body: &ExecRequest) -> bool {
    body.script.is_some() || body.script_base64.is_some()
}

/// Best-effort history recording. A storage failure must never break command
/// execution, so errors are logged but not propagated. Runs the DB call on a
/// detached task so the response isn't delayed by the write — history is a
/// side effect of execution, not part of its result.
#[allow(clippy::too_many_arguments)]
fn record_history(
    state: &AppState,
    account: &str,
    host_id: &str,
    command: &str,
    elevated: bool,
    exit_code: Option<i32>,
    duration_ms: Option<u64>,
    timed_out: bool,
) {
    let record = RelayHistoryRecord {
        id: new_record_id(),
        account: account.to_string(),
        host_id: host_id.to_string(),
        command: command.to_string(),
        elevated,
        exit_code,
        duration_ms,
        timed_out,
        created_at: now_iso(),
        success: exit_code == Some(0),
    };
    let store = state.history.clone();
    tokio::spawn(async move {
        if let Err(e) = store.record(record).await {
            tracing::warn!("[relay] failed to record command history: {e:#}");
        }
    });
}

// ── history endpoints ─────────────────────────────────────────────────────

/// `GET /api/v1/history` — list previously-executed commands, newest first.
///
/// Query params (all optional):
/// - `account` — scope to one API user
/// - `host_id` — scope to one target host
/// - `query`   — case-insensitive substring match on `command`
/// - `limit`   — max entries (default 50, clamped to `[1, 500]`)
/// - `cursor`  — opaque pagination cursor from a prior `next_cursor`
///
/// A non-admin account is always scoped to its own history regardless of the
/// `account` query param (enforced below). Admins (empty `allowed_hosts`) may
/// query any account.
async fn list_history(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(params): Query<HistoryListParams>,
) -> Response {
    let account = match require_account(&state, &headers, addr.ip()) {
        Ok(a) => a,
        Err(r) => return r,
    };

    // A non-admin account can only see its own history. Admins (whose
    // `allowed_hosts` is empty, meaning "all hosts") may pass any account.
    let is_admin = account.allowed_hosts.is_empty();
    let scoped_account = if is_admin {
        params.account
    } else {
        Some(account.username.clone())
    };

    let query = HistoryQuery {
        account: scoped_account,
        host_id: params.host_id,
        query: params.query,
        limit: params.limit,
    };
    let before = params.cursor.as_ref();
    match state.history.list(&query, before).await {
        Ok(page) => Json(page).into_response(),
        Err(e) => {
            tracing::warn!("[relay] history list failed: {e:#}");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "history query failed")
        }
    }
}

/// Query params for [`list_history`]. `cursor` is the JSON-encoded
/// `HistoryCursor` returned as `next_cursor`.
#[derive(Debug, Deserialize)]
struct HistoryListParams {
    #[serde(default)]
    account: Option<String>,
    #[serde(default)]
    host_id: Option<String>,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    cursor: Option<HistoryCursor>,
}

/// `DELETE /api/v1/history/{id}` — remove one history entry. Only the entry's
/// owner (or an admin) may delete it.
async fn delete_history(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let account = match require_account(&state, &headers, addr.ip()) {
        Ok(a) => a,
        Err(r) => return r,
    };
    let is_admin = account.allowed_hosts.is_empty();

    // For a non-admin, first fetch the page containing the entry to verify
    // ownership. We don't have a single-fetch endpoint, so scope the listing
    // to the caller's account and search for the id. This is cheap because
    // history pages are small and the caller is the owner of most rows.
    if !is_admin {
        let query = HistoryQuery {
            account: Some(account.username.clone()),
            host_id: None,
            query: Some(id.clone()),
            limit: Some(500),
        };
        let owned = state.history.list(&query, None).await;
        let owns = owned
            .map(|p| p.entries.iter().any(|e| e.id == id))
            .unwrap_or(false);
        if !owns {
            return error_response(StatusCode::NOT_FOUND, "history entry not found");
        }
    }

    match state.history.delete(&id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => error_response(StatusCode::NOT_FOUND, "history entry not found"),
        Err(e) => {
            tracing::warn!("[relay] history delete failed: {e:#}");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "history delete failed")
        }
    }
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
        fail_exec: Option<String>,
        outcome: Option<ExecOutcome>,
        commands: std::sync::Mutex<Vec<String>>,
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
            command: &str,
            elevated: bool,
            _timeout: Duration,
        ) -> Result<ExecOutcome, ExecutorError> {
            self.elevated
                .store(elevated, std::sync::atomic::Ordering::SeqCst);
            self.commands.lock().unwrap().push(command.to_string());
            if elevated && self.fail_elevation {
                return Err(ExecutorError::ElevationRequired(
                    "No reusable sudo authorization is available".to_string(),
                ));
            }
            if let Some(detail) = &self.fail_exec {
                return Err(ExecutorError::Exec(detail.clone()));
            }
            Ok(self.outcome.clone().unwrap_or(ExecOutcome {
                exit_code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
                timed_out: false,
                truncated: false,
                duration_ms: 1,
            }))
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
        let handle = run(test_config(), executor, Arc::new(NullHistoryStore))
            .await
            .unwrap();
        let url = format!("{}/api/v1/health", handle.url());
        let body: serde_json::Value = reqwest_get(&url, None).await;
        assert_eq!(body["status"], "ok");
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn hosts_requires_auth() {
        let executor: Arc<dyn RelayExecutor> = Arc::new(NullExecutor);
        let handle = run(test_config(), executor, Arc::new(NullHistoryStore))
            .await
            .unwrap();
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
        let handle = run(test_config(), executor, Arc::new(NullHistoryStore))
            .await
            .unwrap();
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
        let handle = run(test_config(), executor.clone(), Arc::new(NullHistoryStore))
            .await
            .unwrap();
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
        let handle = run(test_config(), executor, Arc::new(NullHistoryStore))
            .await
            .unwrap();
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
    async fn exec_failure_returns_actionable_detail() {
        let executor = Arc::new(RecordingExecutor {
            fail_exec: Some(
                "[bastion-pre-command] navigation step 4 stuck waiting for target asset; \
                 state=asset-category menu; last_output=secret-value"
                    .to_string(),
            ),
            ..Default::default()
        });
        let handle = run(test_config(), executor, Arc::new(NullHistoryStore))
            .await
            .unwrap();
        let (status, body) = post_json(
            &format!("{}/api/v1/exec", handle.url()),
            Some(("ops", "pw")),
            &serde_json::json!({
                "host_id": "host-1",
                "command": "uptime",
            }),
        )
        .await;
        assert_eq!(status, 502);
        assert_eq!(body["error"], "remote exec failed");
        let detail = body["detail"].as_str().unwrap();
        assert!(detail.contains("navigation step 4"));
        assert!(!detail.contains("secret-value"));
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn exec_defaults_to_non_elevated_for_legacy_clients() {
        let executor = Arc::new(RecordingExecutor::default());
        let handle = run(test_config(), executor.clone(), Arc::new(NullHistoryStore))
            .await
            .unwrap();
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
    async fn shortform_reuses_auth_validation_and_returns_plain_output_metadata() {
        let executor = Arc::new(RecordingExecutor {
            outcome: Some(ExecOutcome {
                exit_code: Some(17),
                stdout: "shadow-output\n".to_string(),
                stderr: "ignored-for-plain-response".to_string(),
                timed_out: false,
                truncated: false,
                duration_ms: 23,
            }),
            ..Default::default()
        });
        let handle = run(test_config(), executor.clone(), Arc::new(NullHistoryStore))
            .await
            .unwrap();
        let url = format!("{}/r/prod?timeout_ms=500", handle.url());

        let (status, _, _) = raw_text_request(&url, None, "uptime").await;
        assert_eq!(status, 401);

        let (status, _, _) = raw_text_request(&url, Some(("ops", "pw")), "rm -rf /").await;
        assert_eq!(status, 403);
        assert!(executor.commands.lock().unwrap().is_empty());

        let (status, headers, body) =
            raw_text_request(&url, Some(("ops", "pw")), "printf shadow-output").await;
        assert_eq!(status, 200);
        assert_eq!(body, "shadow-output\n");
        assert_eq!(headers.get("content-type").unwrap(), "text/plain; charset=utf-8");
        assert_eq!(headers.get("x-rusterm-exit-code").unwrap(), "17");
        assert_eq!(headers.get("x-rusterm-duration-ms").unwrap(), "23");
        assert_eq!(headers.get("x-rusterm-timed-out").unwrap(), "false");
        assert_eq!(headers.get("x-rusterm-truncated").unwrap(), "false");
        assert_eq!(
            executor.commands.lock().unwrap().as_slice(),
            ["printf shadow-output"]
        );

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn parse_curl_roundtrip() {
        let executor: Arc<dyn RelayExecutor> = Arc::new(NullExecutor);
        let handle = run(test_config(), executor, Arc::new(NullHistoryStore))
            .await
            .unwrap();
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
        // Include the query string (e.g. `?query=docker`) so filtered GETs
        // reach the handler instead of being silently dropped.
        let path = if let Some(q) = parsed.query() {
            format!("{}?{q}", parsed.path())
        } else {
            parsed.path().to_string()
        };
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

    async fn raw_text_request(
        url: &str,
        auth: Option<(&str, &str)>,
        body: &str,
    ) -> (u16, std::collections::HashMap<String, String>, String) {
        use base64::Engine;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let parsed = url::Url::parse(url).unwrap();
        let host = parsed.host_str().unwrap().to_string();
        let port = parsed.port().unwrap();
        let path = if let Some(query) = parsed.query() {
            format!("{}?{query}", parsed.path())
        } else {
            parsed.path().to_string()
        };
        let mut stream = tokio::net::TcpStream::connect((host.as_str(), port))
            .await
            .unwrap();
        let mut request = format!(
            "POST {path} HTTP/1.1\r\nHost: {host}:{port}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n",
            body.len()
        );
        if let Some((username, password)) = auth {
            let credentials = base64::engine::general_purpose::STANDARD
                .encode(format!("{username}:{password}"));
            request.push_str(&format!("Authorization: Basic {credentials}\r\n"));
        }
        request.push_str("Connection: close\r\n\r\n");
        request.push_str(body);
        stream.write_all(request.as_bytes()).await.unwrap();

        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        let text = String::from_utf8(response).unwrap();
        let (head, body) = text.split_once("\r\n\r\n").unwrap();
        let status = head
            .split_whitespace()
            .nth(1)
            .and_then(|value| value.parse().ok())
            .unwrap();
        let headers = head
            .lines()
            .skip(1)
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_string()))
            .collect();
        (status, headers, body.to_string())
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

    // ── Script endpoint (issue 73) ────────────────────────────────────────

    #[tokio::test]
    async fn missing_payload_returns_400() {
        let executor: Arc<dyn RelayExecutor> = Arc::new(NullExecutor);
        let handle = run(test_config(), executor, Arc::new(NullHistoryStore))
            .await
            .unwrap();
        let (status, body) = post_json(
            &format!("{}/api/v1/exec", handle.url()),
            Some(("ops", "pw")),
            &serde_json::json!({"host_id": "host-1"}),
        )
        .await;
        assert_eq!(status, 400);
        assert_eq!(body["code"], "missing_payload");
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn multiple_payloads_returns_400() {
        let executor: Arc<dyn RelayExecutor> = Arc::new(NullExecutor);
        let handle = run(test_config(), executor, Arc::new(NullHistoryStore))
            .await
            .unwrap();
        let (status, body) = post_json(
            &format!("{}/api/v1/exec", handle.url()),
            Some(("ops", "pw")),
            &serde_json::json!({
                "host_id": "host-1",
                "command": "uptime",
                "script": "echo hi",
            }),
        )
        .await;
        assert_eq!(status, 400);
        assert_eq!(body["code"], "multiple_payloads");
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn base64_invalid_returns_400() {
        let executor: Arc<dyn RelayExecutor> = Arc::new(NullExecutor);
        let handle = run(test_config(), executor, Arc::new(NullHistoryStore))
            .await
            .unwrap();
        let (status, body) = post_json(
            &format!("{}/api/v1/exec", handle.url()),
            Some(("ops", "pw")),
            &serde_json::json!({
                "host_id": "host-1",
                "script_base64": "not base64!!!",
            }),
        )
        .await;
        assert_eq!(status, 400);
        assert_eq!(body["code"], "base64_invalid");
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn script_with_dangerous_line_returns_403() {
        let executor: Arc<dyn RelayExecutor> = Arc::new(NullExecutor);
        let handle = run(test_config(), executor, Arc::new(NullHistoryStore))
            .await
            .unwrap();
        // Line 2 is `rm -rf /` — the hard floor must reject the whole script.
        let (status, _) = post_json(
            &format!("{}/api/v1/exec", handle.url()),
            Some(("ops", "pw")),
            &serde_json::json!({
                "host_id": "host-1",
                "script": "echo before\nrm -rf /\necho after",
            }),
        )
        .await;
        assert_eq!(status, 403);
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn script_with_syntax_error_returns_403_sandbox_failed() {
        let executor = Arc::new(RecordingExecutor::default());
        let handle = run(test_config(), executor, Arc::new(NullHistoryStore))
            .await
            .unwrap();
        // `if true; then echo broken` (no `fi`) — passes the hard floor
        // (each line is individually benign) but fails `sh -n` syntax check
        // in the sandbox pre-flight.
        let (status, body) = post_json(
            &format!("{}/api/v1/exec", handle.url()),
            Some(("ops", "pw")),
            &serde_json::json!({
                "host_id": "host-1",
                "script": "if true; then echo broken",
            }),
        )
        .await;
        assert_eq!(status, 403);
        // The syntax error is caught by the sandbox, not the validator —
        // so the code should be `sandbox_failed`.
        assert_eq!(body["code"], "sandbox_failed");
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn base64_encoded_benign_script_runs() {
        use base64::Engine;
        let executor = Arc::new(RecordingExecutor::default());
        let handle = run(test_config(), executor, Arc::new(NullHistoryStore))
            .await
            .unwrap();
        let script = "#!/bin/sh\nset -e\necho hello\nuptime\n";
        let encoded = base64::engine::general_purpose::STANDARD.encode(script);
        let (status, body) = post_json(
            &format!("{}/api/v1/exec", handle.url()),
            Some(("ops", "pw")),
            &serde_json::json!({
                "host_id": "host-1",
                "script_base64": encoded,
            }),
        )
        .await;
        // If dcg is installed and denies a benign script, this becomes 403 —
        // acceptable in CI, since dcg's verdict is environment-dependent.
        // We assert it's either 200 (happy path) or 403 with dcg/sandbox code.
        assert!(
            status == 200 || (status == 403 && body["code"].as_str().is_some()),
            "unexpected status {status}, body: {body}"
        );
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn command_still_works_backward_compatible() {
        // The legacy `command` field must keep working exactly as before.
        let executor = Arc::new(RecordingExecutor::default());
        let handle = run(test_config(), executor, Arc::new(NullHistoryStore))
            .await
            .unwrap();
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
        handle.shutdown().await;
    }

    // ── history persistence ─────────────────────────────────────────────────
    //
    // `record_history` writes via a detached `tokio::spawn`, so the POST
    // returns before the row is guaranteed to be persisted. These tests give
    // the spawned task a brief window before asserting.

    /// Wait until `pred` returns true, polling every 5ms up to ~1s. Used to
    /// observe the async history write without a fixed sleep.
    async fn wait_until<F: Fn() -> bool>(pred: F) {
        for _ in 0..200 {
            if pred() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        // Final check panics with a useful message if still false.
        assert!(pred(), "condition never became true within ~1s");
    }

    #[tokio::test]
    async fn exec_records_history_and_list_returns_it() {
        let executor = Arc::new(RecordingExecutor::default());
        let history = Arc::new(RecordingHistoryStore::new());
        let handle = run(
            test_config(),
            executor.clone(),
            history.clone() as Arc<dyn RelayHistoryStore>,
        )
        .await
        .unwrap();
        let base = handle.url();

        let status = post_exec(&base, "ops", "pw", "host-1", "uptime", None).await;
        assert_eq!(status, 200);

        // The recording happens on a detached task; wait for it.
        let history_clone = history.clone();
        wait_until(move || history_clone.len() == 1).await;

        // GET /history should return the recorded command, newest first.
        let (status, body) =
            reqwest_get_status(&format!("{base}/api/v1/history"), Some(("ops", "pw"))).await;
        assert_eq!(status, 200);
        let entries = body["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["command"], "uptime");
        assert_eq!(entries[0]["host_id"], "host-1");
        assert_eq!(entries[0]["account"], "ops");
        assert_eq!(entries[0]["exit_code"], 0);
        assert_eq!(entries[0]["success"], true);
        assert_eq!(entries[0]["elevated"], false);
        let id = entries[0]["id"].as_str().unwrap().to_string();

        // DELETE /history/{id} removes it.
        let (status, _) = raw_request(
            &format!("{base}/api/v1/history/{id}"),
            "DELETE",
            Some(("ops", "pw")),
            None,
        )
        .await;
        assert_eq!(status, 204);

        // Subsequent GET shows an empty list.
        let (status, body) =
            reqwest_get_status(&format!("{base}/api/v1/history"), Some(("ops", "pw"))).await;
        assert_eq!(status, 200);
        assert!(body["entries"].as_array().unwrap().is_empty());

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn history_query_filters_by_command_substring() {
        let executor = Arc::new(RecordingExecutor::default());
        let history = Arc::new(RecordingHistoryStore::new());
        let handle = run(
            test_config(),
            executor.clone(),
            history.clone() as Arc<dyn RelayHistoryStore>,
        )
        .await
        .unwrap();
        let base = handle.url();

        post_exec(&base, "ops", "pw", "host-1", "docker ps", None).await;
        post_exec(&base, "ops", "pw", "host-1", "uptime", None).await;

        let history_clone = history.clone();
        wait_until(move || history_clone.len() == 2).await;

        // Filter by substring "docker".
        let (status, body) = reqwest_get_status(
            &format!("{base}/api/v1/history?query=docker"),
            Some(("ops", "pw")),
        )
        .await;
        assert_eq!(status, 200);
        let entries = body["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["command"], "docker ps");

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn history_requires_auth() {
        let executor: Arc<dyn RelayExecutor> = Arc::new(NullExecutor);
        let handle = run(test_config(), executor, Arc::new(NullHistoryStore))
            .await
            .unwrap();
        let (status, _) =
            reqwest_get_status(&format!("{}/api/v1/history", handle.url()), None).await;
        assert_eq!(status, 401);
        handle.shutdown().await;
    }
}
