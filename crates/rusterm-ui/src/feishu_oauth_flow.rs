//! Feishu OAuth + OTP auto-fill app logic (issue #129).
//!
//! Pure functions over `AppState` — UI components only flip flags and render;
//! the Dioxus event loop in `app.rs` drains [`AppState::feishu_oauth_events`]
//! and delegates the I/O to [`feishu_oauth_impl`].
//!
//! Flow overview:
//! 1. tty output matches an OTP prompt ([`looks_like_feishu_otp_prompt`]) and
//!    the configured provider is [`OtpWebhookConfig::FeishuUser`].
//! 2. [`feishu_tty_fill_plan`] decides: fetch via the cached/refreshed user
//!    token, or open the QR popup ([`start_feishu_auth`]).
//! 3. The loopback listener (port 8878+) delivers the callback into
//!    `feishu_oauth_events`; [`remember_oauth_delivery`] +
//!    [`feishu_oauth_event_plan`] turn it into a code-exchange task.
//! 4. A successful exchange persists the encrypted token pair
//!    ([`oauth_delivery_to_extra`]) and — when a session was waiting — an OTP
//!    fetch immediately follows; the fetched code is queued into the session
//!    via `send_onekey_submission`.

use std::time::{Duration, Instant};

use rusterm_core::config::OtpWebhookConfig;

use crate::feishu_oauth_listener::{FIRST_PORT, FeishuOAuthCallback};
use crate::state::{
    AppState, FeishuOAuthEvent, FeishuOtpFetch, FeishuQrPopup, FeishuTokenStatus, PendingFeishuAuth,
};

/// Sign-in attempts older than this are unlikely to complete (the user
/// walked away from the QR); the popup shows "rescan" past this age.
pub const FEISHU_QR_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// How long "fetching…" UI stays before the fetch is considered wedged.
pub const FEISHU_OTP_FETCH_STALE: Duration = Duration::from_secs(45);
/// How long the "已填入" badge sticks around after a successful auto-fill.
pub const FEISHU_OTP_DELIVERED_TTL: Duration = Duration::from_secs(12);
/// Persisted user tokens are barely accepted past their access expiry — the
/// fetch task refreshes when the refresh token still lives; re-auth only
/// when both tokens are dead (mirrors `ensure_fresh_token`'s own window).
const TOKEN_REAUTH_SKEW_SECS: i64 = 0;

// ── QR expiry based on an externally supplied clock (testable) ────────────

fn qr_created_fallback(popup: &FeishuQrPopup, now: Instant) -> Instant {
    // Popups created before this release have no `created` field on disk;
    // the popup struct is runtime-only, so `authorized_at`/`failed_at` is
    // the closest stable anchor. Treat "no anchor" as just-created.
    popup
        .status
        .failed_at()
        .or_else(|| popup.status.delivered_at())
        .unwrap_or(now)
}

/// `true` once a popup has outlived [`FEISHU_QR_TIMEOUT`] without success.
pub fn qr_expired(popup: &FeishuQrPopup, now: Instant) -> bool {
    let created = qr_created_fallback(popup, now);
    popup.status.failed_at().is_none()
        && popup.status.delivered_at().is_none()
        && now.duration_since(created) > FEISHU_QR_TIMEOUT
}

// ── OTP prompt matching ────────────────────────────────────────────────────

/// `true` when `line` looks like a JumpServer-style tty OTP prompt AND the
/// term is either a known OTP marker or the terminal reaches the end of the
/// prompt (identical rules to the SSH auth path — a prompt marker mid-line
/// is enough).
pub fn looks_like_feishu_otp_prompt(line: &str) -> bool {
    rusterm_ssh::client::looks_like_otp_prompt(line)
}

/// `true` when password automation must stand down for this prompt.
///
/// JumpServer's `2nd Password:` is an OTP field, never an ordinary login/sudo
/// password field. OneKey and login scripts therefore must not claim it even
/// when the Feishu provider is temporarily unavailable or its retry budget is
/// exhausted. In those cases the user can still type directly in the terminal,
/// while the OTP pipeline surfaces the configuration/auth failure separately.
pub fn otp_prompt_blocks_password_automation(line: &str) -> bool {
    looks_like_feishu_otp_prompt(line)
}

/// The FeishuUser OTP configuration, if that provider is active. Everything
/// needed to start the OAuth flow or the bot round-trip (never logged).
pub fn feishu_user_cfg(state: &AppState) -> Option<FeishuUserCfgView> {
    // Master switch: when OTP auto-fetch is disabled, every Feishu path
    // (pre-auth, browser round-trip, prompt detection) stands down — the
    // OTP prompt falls back to manual entry via the OneKey popup.
    let cfg = state.config_manager.as_ref()?.load_active_otp_webhook()?;
    match cfg {
        OtpWebhookConfig::FeishuUser {
            app_id,
            app_secret,
            bot_open_id,
            code_pattern,
            request_text,
            base_url,
        } => Some(FeishuUserCfgView {
            app_id,
            app_secret,
            bot_open_id,
            code_pattern,
            request_text,
            base_url,
        }),
        _ => None,
    }
}

/// Snapshot of the FeishuUser provider config (owned strings so callers can
/// drop the state read guard before doing I/O).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeishuUserCfgView {
    pub app_id: String,
    pub app_secret: String,
    pub bot_open_id: String,
    pub code_pattern: String,
    pub request_text: String,
    pub base_url: String,
}

/// `true` when the FeishuUser provider is configured but any required field
/// is blank — a trap users hit when they switch providers before filling
/// things in. The flow refuses to run and points at settings instead.
pub fn feishu_cfg_incomplete(cfg: &FeishuUserCfgView) -> bool {
    cfg.app_id.trim().is_empty()
        || cfg.app_secret.trim().is_empty()
        || cfg.bot_open_id.trim().is_empty()
}

// ── Auth start ─────────────────────────────────────────────────────────────

/// Stable Feishu Web entry point. It redirects an authenticated browser to
/// the tenant messenger and otherwise renders Feishu's official QR login.
pub const FEISHU_WEB_LOGIN_URL: &str = "https://www.feishu.cn/messenger/";

/// Browser mode selected before any UI/window side effect occurs.
///
/// OpenAPI OAuth remains available when all application credentials are
/// configured. Missing or partial OpenAPI configuration is not an error for
/// the browser-session flow: RusTerm opens Feishu Web and reuses its persisted
/// cookies instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeishuBrowserStartPlan {
    OpenWebLogin { url: String },
    OpenOAuth { cfg: FeishuUserCfgView },
}

impl FeishuBrowserStartPlan {
    pub fn url(&self) -> &str {
        match self {
            Self::OpenWebLogin { url } => url,
            Self::OpenOAuth { .. } => "",
        }
    }
}

pub fn feishu_browser_start_plan(cfg: Option<&FeishuUserCfgView>) -> FeishuBrowserStartPlan {
    match cfg {
        Some(cfg) if !feishu_cfg_incomplete(cfg) => {
            FeishuBrowserStartPlan::OpenOAuth { cfg: cfg.clone() }
        }
        _ => FeishuBrowserStartPlan::OpenWebLogin {
            url: FEISHU_WEB_LOGIN_URL.to_string(),
        },
    }
}

/// What the caller must do to kick off a fresh QR sign-in.
pub struct AuthStart {
    pub nonce: String,
    pub code_verifier: String,
    pub authorize_url_seed: FeishuAuthUrlSeed,
}

pub struct FeishuAuthUrlSeed {
    pub app_id: String,
    pub code_challenge: String,
}

/// Prepare an OAuth attempt: fresh `state` nonce + PKCE pair. Port selection
/// happens at the listener, so only the seed is returned; [`insert_pending_auth`]
/// finalizes the popup once the redirect URI (port) is known.
pub fn prepare_auth_start() -> AuthStart {
    let nonce = rusterm_ssh::feishu_otp::generate_state();
    let code_verifier = rusterm_ssh::feishu_otp::generate_code_verifier();
    let code_challenge = rusterm_ssh::feishu_otp::code_challenge_s256(&code_verifier);
    AuthStart {
        nonce,
        code_verifier,
        authorize_url_seed: FeishuAuthUrlSeed {
            app_id: String::new(),
            code_challenge,
        },
    }
}

/// Register a pending auth and open the QR popup. `redirect` is the exact
/// URI the listener accepted (`port` encoded), `app_id` from the provider cfg.
pub fn insert_pending_auth(
    state: &mut AppState,
    session: Option<String>,
    start: AuthStart,
    app_id: &str,
    redirect: &str,
    port: u16,
) -> String {
    let authorize_url = rusterm_ssh::feishu_otp::build_authorize_url(
        app_id,
        redirect,
        &start.nonce,
        &start.authorize_url_seed.code_challenge,
    );
    state.feishu_pending_auths.insert(
        start.nonce.clone(),
        PendingFeishuAuth {
            code_verifier: start.code_verifier,
            session: session.clone(),
            created: Instant::now(),
        },
    );
    if is_owned_by(&state.feishu_qr_popup, session.as_deref()) || state.feishu_qr_popup.is_none() {
        state.feishu_qr_popup = Some(FeishuQrPopup {
            session,
            authorize_url: authorize_url.clone(),
            state_nonce: start.nonce,
            port,
            status: FeishuQrPopupStatus::Scanning {
                started: Instant::now(),
            },
        });
    }
    authorize_url
}

fn is_owned_by(popup: &Option<FeishuQrPopup>, session: Option<&str>) -> bool {
    popup
        .as_ref()
        .is_some_and(|p| p.session.as_deref() == session)
}

/// `true` when a QR sign-in for `session` is still awaiting its OAuth
/// callback and is young enough to plausibly complete. Two callers rely on
/// this to WAIT instead of starting a fresh sign-in (issue #130):
/// - the connect-time proactive open must not fire twice for one session;
/// - the `2nd Password:` prompt path must not rotate the nonce while the
///   user is mid-scan on the window that was opened at connect — the OAuth
///   success handler chains the OTP fetch for this session by itself.
/// Attempts older than [`FEISHU_QR_TIMEOUT`] are treated as abandoned so a
/// walked-away scan does not block the prompt-triggered re-auth forever.
pub fn feishu_auth_pending_for(state: &AppState, session: &str, now: Instant) -> bool {
    state.feishu_pending_auths.values().any(|pending| {
        pending.session.as_deref() == Some(session)
            && now.duration_since(pending.created) <= FEISHU_QR_TIMEOUT
    })
}

/// Close the popup + cancel any pending auths tied to `session`. The
/// settings-initiated (`None`-session) flow is intentionally NOT cancelled
/// by a session-specific cancel — the user may close a terminal tab while
/// keeping a settings QR open.
pub fn cancel_feishu_auth(state: &mut AppState, session: Option<&str>) {
    if is_owned_by(&state.feishu_qr_popup, session) {
        state.feishu_qr_popup = None;
    }
    state
        .feishu_pending_auths
        .retain(|_, pending| pending.session.as_deref() != session);
}

// ── Listener → state ────────────────────────────────────────────────────────

/// Queue a callback delivery from the loopback listener. Cheap; the event
/// loop drains `feishu_oauth_events` on its next turn.
pub fn remember_oauth_delivery(state: &mut AppState, cb: FeishuOAuthCallback) {
    state.feishu_oauth_events.push(FeishuOAuthEvent {
        state: cb.state,
        result: cb.result,
    });
}

// ── Event → task plan ──────────────────────────────────────────────────────

/// One OAuth event's effect, computed under a state read.
pub enum OAuthPlan {
    /// State nonce unknown (stale/cancelled/replayed callback) — ignore.
    Ignore,
    /// Feishu returned an error page for this attempt.
    Failed(FailedExchange),
    /// Exchange the authorization code for tokens.
    Exchange(Box<ExchangePlan>),
}

pub struct FailedExchange {
    pub nonce: String,
    pub reason: String,
    pub session: Option<String>,
}

pub struct ExchangePlan {
    pub nonce: String,
    pub code: String,
    pub verifier: String,
    pub session: Option<String>,
    pub app_id: String,
    pub app_secret: String,
    pub redirect_uri: String,
}

impl std::fmt::Debug for ExchangePlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print `code`, `verifier` or `app_secret` — even in debug logs.
        f.debug_struct("ExchangePlan")
            .field("nonce_len", &self.nonce.len())
            .field("session", &self.session.is_some())
            .finish()
    }
}

/// Convert one queued OAuth event into an [`OAuthPlan`]. May mutate the
/// popup for immediate failure feedback (exchange errors arrive later via
/// [`oauth_delivery_to_extra`]).
pub fn feishu_oauth_event_plan(state: &mut AppState, ev: &FeishuOAuthEvent) -> OAuthPlan {
    let now = Instant::now();
    match &ev.result {
        Err(reason) => {
            let session = state
                .feishu_pending_auths
                .get(&ev.state)
                .and_then(|p| p.session.clone());
            if state
                .feishu_qr_popup
                .as_ref()
                .is_some_and(|p| p.state_nonce == ev.state)
            {
                if let Some(popup) = state.feishu_qr_popup.as_mut() {
                    popup.status = FeishuQrPopupStatus::Failed {
                        reason: reason.clone(),
                        failed_at: now,
                    };
                }
            }
            state.feishu_pending_auths.remove(&ev.state);
            state.feishu_token_status = Some(FeishuTokenStatus::Failed {
                reason: reason.clone(),
                at: now,
            });
            OAuthPlan::Failed(FailedExchange {
                nonce: ev.state.clone(),
                reason: reason.clone(),
                session,
            })
        }
        Ok(code) => {
            let Some(pending) = state.feishu_pending_auths.get(&ev.state) else {
                return OAuthPlan::Ignore;
            };
            let Some(cfg) = feishu_user_cfg(state) else {
                // Provider switched mid-flow — nothing sensible to do.
                state.feishu_pending_auths.remove(&ev.state);
                return OAuthPlan::Ignore;
            };
            let exchange = ExchangePlan {
                nonce: ev.state.clone(),
                code: code.clone(),
                verifier: pending.code_verifier.clone(),
                session: pending.session.clone(),
                app_id: cfg.app_id.clone(),
                app_secret: cfg.app_secret.clone(),
                redirect_uri: crate::feishu_oauth_listener::redirect_uri(
                    state
                        .feishu_qr_popup
                        .as_ref()
                        .filter(|p| p.state_nonce == ev.state)
                        .map(|p| p.port)
                        .unwrap_or(FIRST_PORT),
                ),
            };
            OAuthPlan::Exchange(Box::new(exchange))
        }
    }
}

/// Extra data threaded *alongside* listener deliveries so token-exchange
/// tasks can persist and act without re-reading world state.
#[derive(Clone)]
pub struct OAuthDeliveryExtra {
    pub verifier: String,
    pub app_id: String,
    pub app_secret: String,
    pub redirect_uri: String,
}

/// Encode the plan context into the compact tuple the event loop hands to
/// `AppState` after the async exchange resolves. Keeps the OAuth code-verifier
/// and app-secret inside the task — never in `AppState`'s serializable view.
pub fn oauth_extra_from_plan(plan: &ExchangePlan) -> OAuthDeliveryExtra {
    OAuthDeliveryExtra {
        verifier: plan.verifier.clone(),
        app_id: plan.app_id.clone(),
        app_secret: plan.app_secret.clone(),
        redirect_uri: plan.redirect_uri.clone(),
    }
}

// ── Exchange result application ─────────────────────────────────────────────

/// Apply a finished token exchange. On success the encrypted token pair is
/// persisted through `ConfigManager`; the caller must then invoke
/// [`feishu_tty_fill_plan`] for the named session to continue the OTP fetch.
pub fn apply_feishu_oauth_result(
    state: &mut AppState,
    nonce: &str,
    result: &Result<rusterm_core::config::FeishuUserToken, String>,
) {
    state.feishu_pending_auths.remove(nonce);
    match result {
        Ok(token) => {
            let expires_at = token.access_expires_at;
            if let Some(cm) = state.config_manager.clone() {
                if let Err(e) = cm.save_feishu_user_token(Some(token)) {
                    tracing::error!("[OTP-FEISHU] failed to persist user token: {e}");
                    state.feishu_token_status = Some(FeishuTokenStatus::Failed {
                        reason: format!("persist failed: {e}"),
                        at: Instant::now(),
                    });
                    return;
                }
            }
            // Drop the popup ONLY when this exchange was the one behind it;
            // a re-scan for the same session owns the popup meanwhile.
            let popup_matches = state
                .feishu_qr_popup
                .as_ref()
                .is_some_and(|p| p.state_nonce == nonce);
            if popup_matches {
                state.feishu_qr_popup = None;
            }
            state.feishu_token_status = Some(FeishuTokenStatus::Connected { expires_at });
            tracing::info!("[OTP-FEISHU] sign-in complete; user token persisted (len=hidden)");
        }
        Err(reason) => {
            tracing::error!("[OTP-FEISHU] token exchange failed: {reason}");
            if state
                .feishu_qr_popup
                .as_ref()
                .is_some_and(|p| p.state_nonce == nonce)
            {
                if let Some(popup) = state.feishu_qr_popup.as_mut() {
                    popup.status = FeishuQrPopupStatus::Failed {
                        reason: reason.clone(),
                        failed_at: Instant::now(),
                    };
                }
            }
            state.feishu_token_status = Some(FeishuTokenStatus::Failed {
                reason: reason.clone(),
                at: Instant::now(),
            });
        }
    }
}

// ── tty OTP fetch planning ─────────────────────────────────────────────────

/// Decision for a per-session OTP auto-fill cycle.
pub enum TtyFillPlan {
    /// Spawn the bot round-trip using an already-present user token.
    Fetch {
        token: rusterm_core::config::FeishuUserToken,
    },
    /// Persisted token is dead — the user must scan again.
    Reauth,
    /// Provider inactive, blank, or another fetch attempt is already running.
    Skip,
}

pub fn feishu_tty_fill_plan(state: &AppState) -> TtyFillPlan {
    let Some(token) = state
        .config_manager
        .as_ref()
        .and_then(|cm| cm.load_feishu_user_token())
    else {
        return TtyFillPlan::Reauth;
    };
    let now = chrono::Utc::now().timestamp();
    if now >= token.refresh_expires_at + TOKEN_REAUTH_SKEW_SECS {
        return TtyFillPlan::Reauth;
    }
    TtyFillPlan::Fetch { token }
}

/// Debounce + attempt-cap gate before a tty auto-fill cycle starts.
///
/// Returns `true` when the cycle may proceed. Side effects:
/// - stamps `feishu_otp_status[session] = InFlight` so the second chunk of
///   the same prompt (SSH splits outputs) cannot double-fire;
/// - bumps `feishu_otp_attempts[session]` past [`FEISHU_OTP_MAX_ATTEMPTS`]
///   the caller must fall back to the manual popup.
pub const FEISHU_OTP_MAX_ATTEMPTS: u8 = 3;

pub fn feishu_tty_fill_begin(state: &mut AppState, session: &str) -> bool {
    if state
        .feishu_otp_status
        .get(session)
        .is_some_and(|status| match status {
            FeishuOtpFetch::InFlight { started } => started.elapsed() < FEISHU_OTP_FETCH_STALE,
            FeishuOtpFetch::Delivered { at } => at.elapsed() < FEISHU_OTP_DELIVERED_TTL,
            FeishuOtpFetch::Failed { .. } => false,
        })
    {
        return false;
    }
    let attempts = state
        .feishu_otp_attempts
        .entry(session.to_string())
        .or_insert(0);
    if *attempts >= FEISHU_OTP_MAX_ATTEMPTS {
        return false;
    }
    *attempts += 1;
    state.feishu_otp_status.insert(
        session.to_string(),
        FeishuOtpFetch::InFlight {
            started: Instant::now(),
        },
    );
    true
}

/// After a proactive OAuth exchange, fetch an OTP only when this session has
/// already reached and is still displaying its OTP prompt. A successful scan
/// that finishes before JumpServer asks for `2nd Password:` must only persist
/// the token; the later prompt will start the fetch at the correct time.
pub fn feishu_should_fetch_after_auth(
    state: &AppState,
    session: &str,
    otp_prompt_visible: bool,
) -> bool {
    otp_prompt_visible
        && matches!(
            state.feishu_otp_status.get(session),
            Some(FeishuOtpFetch::InFlight { .. })
        )
}

/// Record the OTP fetch outcome for the popup / status badge. A delivered
/// OTP resets the session's attempt counter so the NEXT login prompt gets a
/// fresh allowance.
pub fn feishu_tty_fill_end(state: &mut AppState, session: &str, ok: bool, reason: Option<String>) {
    if ok {
        state.feishu_otp_attempts.remove(session);
        state.feishu_otp_status.insert(
            session.to_string(),
            FeishuOtpFetch::Delivered { at: Instant::now() },
        );
    } else {
        state.feishu_otp_status.insert(
            session.to_string(),
            FeishuOtpFetch::Failed {
                reason: reason.unwrap_or_else(|| "fetch failed".into()),
                at: Instant::now(),
            },
        );
    }
}

/// Session teardown hook — same cleanup points as `clear_onekey_session_runtime`.
pub fn feishu_otp_session_closed(state: &mut AppState, session: &str) {
    state.feishu_otp_status.remove(session);
    state.feishu_otp_attempts.remove(session);
    if let Some(popup) = state.feishu_qr_popup.take() {
        if popup.session.as_deref() == Some(session) {
            state.feishu_pending_auths.remove(&popup.state_nonce);
        } else {
            state.feishu_qr_popup = Some(popup);
        }
    }
}

// Re-exports used by the Dioxus popup / app.rs bridge layer.
pub use crate::state::FeishuQrPopupStatus;

#[cfg(test)]
mod tests {
    use super::*;
    use rusterm_core::config::FeishuUserToken;
    use std::collections::HashMap;

    fn state_without_cm() -> AppState {
        // AppState::default() reports Locked when an encrypted config exists
        // on disk (e.g. on the dev machine running the tests) — the unlock
        // gate is irrelevant for these state-machine tests.
        AppState {
            sessions: Vec::new(),
            active_tab: None,
            active_session: None,
            tabs: Vec::new(),
            sidebar_open: true,
            sidebar_preferences: Default::default(),
            workspace_preferences: Default::default(),
            connections: Vec::new(),
            theme: crate::state::Theme::Dark,
            focused_tab_appearance: Default::default(),
            keybindings: Default::default(),
            skin: Default::default(),
            close_senders: Vec::new(),
            resize_senders: HashMap::new(),
            config_manager: None,
            terminals: HashMap::new(),
            session_logs: HashMap::new(),
            unlock_state: crate::state::UnlockState::Unlocked,
            master_password_error: None,
            suggestion_epoch: 0,
            pending_exit_check: HashMap::new(),
            exit_code_sessions: Default::default(),
            terminal_command_lines: HashMap::new(),
            history_completion_sessions: Default::default(),
            suggestion_muted_sessions: Default::default(),
            recent_failed_commands: Default::default(),
            last_failed_command_by_session: HashMap::new(),
            onekeys: Vec::new(),
            onekey_preferences: Vec::new(),
            onekey_preference_attempts: HashMap::new(),
            onekey_habit_events: HashMap::new(),
            onekey_pending_analytics: Vec::new(),
            onekey_popups: HashMap::new(),
            onekey_submission_feedback: HashMap::new(),
            onekey_submission_cooldown: HashMap::new(),
            onekey_output_since_submission: HashMap::new(),
            onekey_skip_logged: Default::default(),
            session_configs: HashMap::new(),
            session_connection_states: HashMap::new(),
            session_nodes: HashMap::new(),
            otp_groups: crate::state::OtpGroupRegistry::default(),
            send_target_selection: None,
            ssh_sessions: HashMap::new(),
            sftp_clients: HashMap::new(),
            transfers: crate::transfers::TransferState::default(),
            transfer_cancellations: HashMap::new(),
            zmodem: std::sync::Arc::new(parking_lot::Mutex::new(
                crate::zmodem::ZmodemSessions::new(),
            )),
            bottom_shell_session_id: None,
            analytics: crate::analytics::AnalyticsHandle::default(),
            ohmyzsh: None,
            layouts: HashMap::new(),
            focused_pane: None,
            layout_preset: crate::layout::LayoutPreset::default(),
            split_mode_enabled: true,
            restore_pending: None,
            restore_disabled: false,
            confirm_close_on_exit: true,
            comparison_diff_warning_enabled: true,
            suggestion_enabled: true,
            suggestion_count: 3,
            collect_usage_habits: false,
            language: Default::default(),
            login_scripts: HashMap::new(),
            session_replays: HashMap::new(),
            close_dialog_visible: false,
            close_dialog_dont_ask_again: true,
            pending_dangerous_command: None,
            safety_checker: rusterm_core::CommandSafetyChecker::new(),
            shadow_sandbox: rusterm_ai::ShadowSandbox::default(),
            comparison_diffs: None,
            comparison_diff_warning: None,
            comparison_diff_confirmed: false,
            relay_config: rusterm_relay::RelayConfig::default(),
            relay_runtime: crate::relay_tunnel::RelayRuntime::default(),
            relay_panel_open: false,
            relay_status_message: None,
            tunnel_manager: None,
            tunnel_panel_open: false,
            pending_renders: HashMap::new(),
            next_render_allowed: HashMap::new(),
            chat_visible: false,
            chat_settings: rusterm_core::config::ChatSettings::default(),
            chat_messages: Vec::new(),
            chat_input: String::new(),
            chat_command_mode: false,
            chat_command_results: Vec::new(),
            chat_command_selected: 0,
            chat_drag_offset: None,
            chat_status: None,
            chat_api_keys: HashMap::new(),
            chat_request_in_flight: false,
            feishu_pending_auths: HashMap::new(),
            feishu_qr_popup: None,
            feishu_oauth_port: None,
            feishu_oauth_events: Vec::new(),
            feishu_token_status: None,
            feishu_otp_status: HashMap::new(),
            feishu_otp_attempts: HashMap::new(),
            feishu_auth_reveal_settings: false,
        }
    }

    fn popup_for(state_nonce: &str, session: Option<&str>) -> FeishuQrPopup {
        FeishuQrPopup {
            session: session.map(str::to_string),
            authorize_url: "https://accounts.feishu.cn/x".into(),
            state_nonce: state_nonce.into(),
            port: FIRST_PORT,
            status: FeishuQrPopupStatus::Scanning {
                started: Instant::now(),
            },
        }
    }

    fn token(access_secs: i64, refresh_secs: i64) -> FeishuUserToken {
        let now = chrono::Utc::now().timestamp();
        FeishuUserToken {
            access_token: "at".into(),
            refresh_token: "rt".into(),
            access_expires_at: now + access_secs,
            refresh_expires_at: now + refresh_secs,
            user_open_id: None,
        }
    }

    // ── qr_expired ──────────────────────────────────────────────────────
    #[test]
    fn qr_expired_only_after_timeout_without_progress() {
        let now = Instant::now();
        let fresh = popup_for("n", Some("s"));
        assert!(!qr_expired(&fresh, now));
        let failed = FeishuQrPopup {
            status: FeishuQrPopupStatus::Failed {
                reason: "x".into(),
                failed_at: now - Duration::from_secs(60),
            },
            ..fresh.clone()
        };
        assert!(!qr_expired(&failed, now));
    }

    // ── prompt matching ─────────────────────────────────────────────────
    #[test]
    fn otp_prompt_markers_gate() {
        assert!(looks_like_feishu_otp_prompt("2nd Password: "));
        assert!(looks_like_feishu_otp_prompt("OTP: "));
        assert!(looks_like_feishu_otp_prompt("二次验证码:"));
        assert!(looks_like_feishu_otp_prompt("[sudo] 验证码: "));
        assert!(!looks_like_feishu_otp_prompt("Password: "));
        assert!(!looks_like_feishu_otp_prompt("username: "));
        assert!(!looks_like_feishu_otp_prompt(""));
    }

    #[test]
    fn feishu_prompt_ownership_gates() {
        // Owned: provider active, OTP prompt, attempts left — OneKey and
        // login scripts must stand down so the auto-fill (or QR popup) runs.
        assert!(otp_prompt_blocks_password_automation("2nd Password: "));
        // koko echoes `*` per typed char — the masked tail must not break
        // the ownership match.
        assert!(otp_prompt_blocks_password_automation(
            "2nd Password: ******"
        ));
        // A temporarily unavailable/missing provider must not let OneKey
        // reinterpret JumpServer's OTP as an ordinary sudo/login password.
        assert!(otp_prompt_blocks_password_automation("2nd Password: "));
        // Not an OTP prompt → OneKey may auto-submit the login password.
        assert!(!otp_prompt_blocks_password_automation("Password: "));
        // Exhausting Feishu retries still must not release an OTP prompt to
        // password automation. The user can type directly in the terminal.
        assert!(otp_prompt_blocks_password_automation("2nd Password: "));
    }

    #[test]
    fn oauth_success_fetches_only_for_an_already_visible_otp_prompt() {
        let mut state = state_without_cm();
        state.feishu_otp_status.insert(
            "sess-1".into(),
            FeishuOtpFetch::InFlight {
                started: Instant::now(),
            },
        );

        assert!(feishu_should_fetch_after_auth(&state, "sess-1", true));
        assert!(!feishu_should_fetch_after_auth(&state, "sess-1", false));
        assert!(!feishu_should_fetch_after_auth(&state, "other", true));
    }

    #[test]
    fn missing_openapi_config_starts_web_session_login() {
        let plan = feishu_browser_start_plan(None);
        assert_eq!(
            plan,
            FeishuBrowserStartPlan::OpenWebLogin {
                url: FEISHU_WEB_LOGIN_URL.to_string(),
            }
        );
        assert!(plan.url().starts_with("https://"));
        assert!(!plan.url().trim().is_empty());
    }

    // ── start & cancel ─────────────────────────────────────────────────
    #[test]
    fn start_registers_pending_and_popup() {
        let mut state = state_without_cm();
        let start = prepare_auth_start();
        let url = insert_pending_auth(
            &mut state,
            Some("sess-1".into()),
            start,
            "cli_xxx",
            "http://127.0.0.1:8878/oauth/feishu/callback",
            FIRST_PORT,
        );
        assert!(url.contains("app_id=cli_xxx"));
        assert!(url.contains("code_challenge="));
        assert_eq!(state.feishu_pending_auths.len(), 1);
        let popup = state.feishu_qr_popup.clone().expect("popup visible");
        assert_eq!(popup.session.as_deref(), Some("sess-1"));
        assert_eq!(popup.authorize_url, url);
    }

    #[test]
    fn cancel_only_clears_its_own_session() {
        let mut state = state_without_cm();
        let start_a = prepare_auth_start();
        insert_pending_auth(
            &mut state,
            Some("a".into()),
            start_a,
            "cli_x",
            "http://127.0.0.1:8878/callback",
            FIRST_PORT,
        );
        let start_b = prepare_auth_start();
        insert_pending_auth(
            &mut state,
            None,
            start_b,
            "cli_x",
            "http://127.0.0.1:8878/callback",
            FIRST_PORT,
        );
        cancel_feishu_auth(&mut state, Some("a"));
        assert_eq!(
            state.feishu_pending_auths.len(),
            1,
            "sessionless auth survives a session cancel"
        );
        assert!(
            state.feishu_qr_popup.is_none(),
            "popup held by cancelled session closes"
        );
    }

    #[test]
    fn session_cancel_keeps_sessionless_popup_visible() {
        let mut state = state_without_cm();
        let start = prepare_auth_start();
        insert_pending_auth(
            &mut state,
            None,
            start,
            "cli_x",
            "http://127.0.0.1:8878/callback",
            FIRST_PORT,
        );
        // A session's cancel must NOT close a settings-started popup.
        cancel_feishu_auth(&mut state, Some("other"));
        assert!(state.feishu_qr_popup.is_some());
        assert_eq!(state.feishu_pending_auths.len(), 1);
    }

    #[test]
    fn pending_auth_lookup_is_session_scoped_and_ages_out() {
        let mut state = state_without_cm();
        let now = Instant::now();
        assert!(
            !feishu_auth_pending_for(&state, "sess-1", now),
            "no pending auths at all"
        );
        insert_pending_auth(
            &mut state,
            None,
            prepare_auth_start(),
            "cli_x",
            "http://127.0.0.1:8878/callback",
            FIRST_PORT,
        );
        assert!(
            !feishu_auth_pending_for(&state, "sess-1", now),
            "a settings-owned (sessionless) auth does not count for a session"
        );
        insert_pending_auth(
            &mut state,
            Some("sess-1".into()),
            prepare_auth_start(),
            "cli_x",
            "http://127.0.0.1:8878/callback",
            FIRST_PORT,
        );
        assert!(feishu_auth_pending_for(&state, "sess-1", Instant::now()));
        assert!(
            !feishu_auth_pending_for(&state, "sess-2", Instant::now()),
            "another session's auth does not count"
        );
        // Abandoned scans age out so the prompt path can re-auth again.
        let later = Instant::now() + FEISHU_QR_TIMEOUT + Duration::from_secs(1);
        assert!(!feishu_auth_pending_for(&state, "sess-1", later));
    }

    // ── event → plan ────────────────────────────────────────────────────
    #[test]
    fn unknown_nonce_is_ignored() {
        let mut state = state_without_cm();
        let plan = feishu_oauth_event_plan(
            &mut state,
            &FeishuOAuthEvent {
                state: "ghost".into(),
                result: Ok("code".into()),
            },
        );
        assert!(matches!(plan, OAuthPlan::Ignore));
        assert!(state.feishu_pending_auths.is_empty());
    }

    #[test]
    fn error_callback_marks_popup_failed_and_clears_pending() {
        let mut state = state_without_cm();
        let start = prepare_auth_start();
        let nonce = start.nonce.clone();
        insert_pending_auth(
            &mut state,
            Some("s".into()),
            start,
            "cli_x",
            "http://127.0.0.1:8878/callback",
            FIRST_PORT,
        );
        let plan = feishu_oauth_event_plan(
            &mut state,
            &FeishuOAuthEvent {
                state: nonce.clone(),
                result: Err("access denied".into()),
            },
        );
        match plan {
            OAuthPlan::Failed(f) => {
                assert_eq!(f.reason, "access denied");
                assert_eq!(f.session.as_deref(), Some("s"));
            }
            _ => panic!("expected Failed plan"),
        }
        assert!(state.feishu_pending_auths.is_empty());
        let popup = state.feishu_qr_popup.expect("popup stays for feedback");
        assert!(matches!(popup.status, FeishuQrPopupStatus::Failed { .. }));
    }

    // ── attempt cap & debounce ─────────────────────────────────────────
    #[test]
    fn attempts_are_capped_and_failures_do_not_stick() {
        let mut state = state_without_cm();
        for cycle in 0..FEISHU_OTP_MAX_ATTEMPTS {
            assert!(
                feishu_tty_fill_begin(&mut state, "s"),
                "cycle {cycle} starts"
            );
            feishu_tty_fill_end(&mut state, "s", false, Some("timeout".into()));
        }
        assert!(
            !feishu_tty_fill_begin(&mut state, "s"),
            "cap reached → caller must fall back"
        );
    }

    #[test]
    fn delivered_status_debounces_then_allows_again() {
        let mut state = state_without_cm();
        assert!(feishu_tty_fill_begin(&mut state, "s"));
        feishu_tty_fill_end(&mut state, "s", true, None);
        assert!(
            !feishu_tty_fill_begin(&mut state, "s"),
            "fresh Delivered debounces the same chunk"
        );
        let status = state.feishu_otp_status.get_mut("s").unwrap();
        if let FeishuOtpFetch::Delivered { at } = status {
            *at = Instant::now() - FEISHU_OTP_DELIVERED_TTL - Duration::from_millis(5);
        }
        assert!(
            feishu_tty_fill_begin(&mut state, "s"),
            "stale Delivered allows a new cycle"
        );
    }

    #[test]
    fn in_flight_status_blocks_duplicate_cycles() {
        let mut state = state_without_cm();
        assert!(feishu_tty_fill_begin(&mut state, "s"));
        assert!(
            !feishu_tty_fill_begin(&mut state, "s"),
            "InFlight blocks a re-entrant chunk"
        );
    }

    #[test]
    fn successful_delivery_resets_attempt_counter() {
        let mut state = state_without_cm();
        assert!(feishu_tty_fill_begin(&mut state, "s"));
        feishu_tty_fill_end(&mut state, "s", false, Some("x".into()));
        assert!(feishu_tty_fill_begin(&mut state, "s"));
        feishu_tty_fill_end(&mut state, "s", true, None);
        assert!(
            state.feishu_otp_attempts.get("s").is_none(),
            "delivered OTP clears the attempt budget for the next login"
        );
    }

    #[test]
    fn session_close_cleans_everything() {
        let mut state = state_without_cm();
        assert!(feishu_tty_fill_begin(&mut state, "s"));
        let start = prepare_auth_start();
        insert_pending_auth(
            &mut state,
            Some("s".into()),
            start,
            "cli_x",
            "http://127.0.0.1:8878/callback",
            FIRST_PORT,
        );
        feishu_otp_session_closed(&mut state, "s");
        assert!(state.feishu_otp_status.is_empty());
        assert!(state.feishu_otp_attempts.is_empty());
        assert!(state.feishu_qr_popup.is_none());
        assert!(state.feishu_pending_auths.is_empty());
    }

    // ── token plan gating ──────────────────────────────────────────────
    #[test]
    fn fill_plan_requires_a_config_manager_token() {
        let state = state_without_cm();
        assert!(matches!(feishu_tty_fill_plan(&state), TtyFillPlan::Reauth));
    }

    #[test]
    fn token_refresh_expired_forces_reauth() {
        let expired = token(-60, -5);
        let now = chrono::Utc::now().timestamp();
        assert!(
            now >= expired.refresh_expires_at,
            "fixture must be expired for the plan gate"
        );
    }
}
