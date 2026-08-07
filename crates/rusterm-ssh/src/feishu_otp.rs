//! Feishu (Lark) *user-token* OTP relay used for JumpServer second-factor
//! prompts inside an interactive tty session.
//!
//! Unlike the bot-chat reader in [`crate::otp`] (which uses a
//! `tenant_access_token` to *read* a chat history), this module drives the
//! interactive **user OAuth** flow and *sends* a message — as the signed-in
//! user — to a designated ops bot (e.g. “智小安”). That bot is what issues the
//! one-time code, which RusTerm then reads back from the direct-message
//! conversation and auto-fills into the terminal.
//!
//! # Flow
//!
//! 1. [`build_authorize_url`] produces the Feishu QR / authorization URL with a
//!    PKCE challenge. The user scans it with the Feishu mobile app, which
//!    redirects to RusTerm's local loopback listener with a `code`.
//! 2. [`exchange_code`] trades that `code` (+ PKCE verifier) for a
//!    `user_access_token` and `refresh_token`.
//! 3. [`feishu_otp::ensure_fresh_token`] refreshes the token when stale, so
//!    the user only scans once until the refresh window lapses.
//! 4. [`FeishuOtpClient::request_otp`] sends a request **only to the configured bot**
//!    (hard-enforced) and polls the DM for the matching OTP reply, also
//!    parsing the bot-reported validity window (`有效期剩余：NN秒`) and
//!    re-requesting once when the code is about to lapse.
//!
//! # Guard rails (user requirement)
//!
//! * Messages may *only* be sent to the configured bot `open_id`. The
//!   `receive_id` is baked into [`FeishuOtpClient`] at construction and is not
//!   parameterised on the send path, so there is no way to address any other
//!   user, chat, or app.
//! * Tokens and the app secret are never logged.
//! * The OAuth listener binds `127.0.0.1` only and is loopback-scoped.

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use regex::Regex;
use rusterm_core::config::FeishuUserToken;
use serde::Deserialize;

/// Default Feishu open platform base (international users override to
/// `https://open.larksuite.com`).
pub const DEFAULT_FEISHU_BASE_URL: &str = "https://open.feishu.cn";

/// OAuth endpoints live on the accounts host, distinct from the open-apis host.
const FEISHU_ACCOUNTS_BASE: &str = "https://accounts.feishu.cn";

/// How long before expiry we proactively refresh the user token.
const TOKEN_REFRESH_SKEW_SECS: i64 = 300;

/// Default request timeout for the interactive OTP round-trip.
const OTP_FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Polling cadence while waiting for the bot to reply with the code.
const OTP_POLL_INTERVAL: Duration = Duration::from_millis(800);

/// How long to keep polling the DM for the OTP reply before giving up.
const OTP_REPLY_WINDOW: Duration = Duration::from_secs(25);

/// A code whose reported remaining validity is below this many seconds is
/// too risky to type (network + tty round-trip may outlive it); request a
/// fresh one instead (issue #130).
const MIN_OTP_VALID_SECS: u64 = 5;

/// Typed marker error: the persisted Feishu user session is unusable (both
/// tokens expired, or the refresh endpoint rejected the refresh token) and
/// only a fresh QR sign-in can recover. Callers detect it with
/// `err.downcast_ref::<ReauthRequired>()` — anyhow searches the whole
/// context chain — and pop the QR popup instead of surfacing a dead-end
/// failure (issue #130).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReauthRequired;

impl std::fmt::Display for ReauthRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Feishu user session expired; QR re-auth required")
    }
}

impl std::error::Error for ReauthRequired {}

// ── PKCE ──────────────────────────────────────────────────────────────

/// Generate a high-entropy PKCE `code_verifier` (43–128 url-safe chars).
pub fn generate_code_verifier() -> String {
    let mut buf = [0u8; 48];
    fill_random(&mut buf);
    // Base64-url without padding → 64 chars, within the 43..128 range.
    base64_url_no_pad(&buf)
}

/// Fill a byte buffer with cryptographically secure random bytes.
///
/// `getrandom` is already in the dependency tree (via `uuid`/`tokio`); using
/// it here avoids adding a heavyweight `rand` feature matrix for two call
/// sites that only need raw entropy.
fn fill_random(buf: &mut [u8]) {
    if getrandom::getrandom(buf).is_err() {
        // Deterministic-but-entropy-mixed fallback: XOR time + address-space
        // entropy. Only reachable on exotic targets where the OS RNG syscall
        // is unavailable; acceptable for a CSRF nonce / PKCE verifier.
        let nanos: u64 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as u64)
            .unwrap_or(0);
        let addr: u64 = buf.as_ptr() as u64;
        for (i, b) in buf.iter_mut().enumerate() {
            *b = (nanos
                .wrapping_mul(31)
                .wrapping_add(addr)
                .wrapping_add(i as u64 * 7)
                & 0xff) as u8;
        }
    }
}

/// Derive the S256 `code_challenge` from a verifier.
pub fn code_challenge_s256(verifier: &str) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(verifier.as_bytes());
    base64_url_no_pad(&digest)
}

/// Generate an opaque OAuth `state` nonce (CSRF protection).
pub fn generate_state() -> String {
    let mut buf = [0u8; 24];
    fill_random(&mut buf);
    base64_url_no_pad(&buf)
}

/// Base64-url-encode without padding. `Ssl`-free helper (reqwest/russh pulls
/// in `base64` already; we re-implement the url-safe alphabet to avoid adding
/// a feature flag).
fn base64_url_no_pad(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Build the authorization URL the user scans / opens to sign in with Feishu.
///
/// Returns the full URL (query encoded). The caller renders it as a QR code
/// and/or opens it in the system browser.
pub fn build_authorize_url(
    app_id: &str,
    redirect_uri: &str,
    state: &str,
    code_challenge: &str,
) -> String {
    format!(
        "{}/open-apis/authen/v1/authorize?app_id={}&redirect_uri={}&state={}&code_challenge={}&code_challenge_method=S256",
        FEISHU_ACCOUNTS_BASE,
        url_encode(app_id),
        url_encode(redirect_uri),
        url_encode(state),
        url_encode(code_challenge),
    )
}

/// Minimal percent-encoding for URL query values (unreserved per RFC 3986).
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

// ── Token model ────────────────────────────────────────────────────────

fn token_now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// `true` if the access token is still usable without a refresh.
pub fn token_access_valid(token: &FeishuUserToken) -> bool {
    token_now() < token.access_expires_at - TOKEN_REFRESH_SKEW_SECS
}

/// `true` if we can still refresh (refresh token not expired).
pub fn token_refresh_valid(token: &FeishuUserToken) -> bool {
    token_now() < token.refresh_expires_at
}

/// Ensure a usable access token, refreshing when stale. Returns `Ok(None)`
/// when neither access nor refresh is valid — the caller must then trigger a
/// fresh QR sign-in. On refresh, `token` is updated in place.
pub async fn ensure_fresh_token(
    token: &mut FeishuUserToken,
    base_url: &str,
    app_id: &str,
    app_secret: &str,
) -> Result<Option<String>> {
    if token_access_valid(token) {
        return Ok(Some(token.access_token.clone()));
    }
    if !token_refresh_valid(token) {
        tracing::info!("[OTP-FEISHU] both access and refresh tokens expired; re-auth required");
        return Ok(None);
    }
    refresh_user_token(base_url, app_id, app_secret, token)
        .await
        .map(Some)
}

#[derive(Debug, Deserialize)]
struct FeishuOAuthTokenResp {
    code: i64,
    #[serde(default)]
    error_description: String,
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    expires_in: i64,
    #[serde(default)]
    refresh_expires_in: i64,
}

/// Exchange an authorization `code` (from the OAuth callback) for tokens.
pub async fn exchange_code(
    app_id: &str,
    app_secret: &str,
    code: &str,
    redirect_uri: &str,
    code_verifier: &str,
) -> Result<FeishuUserToken> {
    let client = http_client(OTP_FETCH_TIMEOUT)?;
    let resp: FeishuOAuthTokenResp = client
        .post(format!(
            "{}/open-apis/authen/v2/oauth/token",
            FEISHU_ACCOUNTS_BASE
        ))
        .json(&serde_json::json!({
            "grant_type": "authorization_code",
            "client_id": app_id,
            "client_secret": app_secret,
            "code": code,
            "redirect_uri": redirect_uri,
            "code_verifier": code_verifier,
        }))
        .send()
        .await
        .context("Feishu token exchange request failed")?
        .json()
        .await
        .context("Feishu token exchange response decode failed")?;
    if resp.code != 0 {
        return Err(anyhow!(
            "Feishu token exchange error {}: {}",
            resp.code,
            resp.error_description
        ));
    }
    let now = token_now();
    Ok(FeishuUserToken {
        access_token: resp.access_token,
        refresh_token: resp.refresh_token,
        access_expires_at: now + resp.expires_in,
        refresh_expires_at: now + resp.refresh_expires_in.max(0),
        user_open_id: None,
    })
}

/// Refresh a stale access token using the stored refresh token. Updates the
/// token in place and returns the fresh access token.
async fn refresh_user_token(
    base_url: &str,
    app_id: &str,
    app_secret: &str,
    token: &mut FeishuUserToken,
) -> Result<String> {
    // The refresh endpoint is on the open-apis host, keyed by tenant app.
    let _ = base_url; // base_url reserved for Lark-variant override of open-apis
    let client = http_client(OTP_FETCH_TIMEOUT)?;
    let resp: FeishuOAuthTokenResp = client
        .post(format!(
            "{}/open-apis/authen/v2/oauth/token",
            FEISHU_ACCOUNTS_BASE
        ))
        .json(&serde_json::json!({
            "grant_type": "refresh_token",
            "client_id": app_id,
            "client_secret": app_secret,
            "refresh_token": token.refresh_token,
        }))
        .send()
        .await
        .context("Feishu token refresh request failed")?
        .json()
        .await
        .context("Feishu token refresh response decode failed")?;
    if resp.code != 0 {
        // A rejected refresh token (revoked, rotated away, app secret
        // changed…) is not transient — only a fresh QR sign-in recovers.
        // Tag the error so the UI can fall back to the QR popup.
        return Err(anyhow::Error::new(ReauthRequired).context(format!(
            "Feishu token refresh error {}: {}",
            resp.code, resp.error_description
        )));
    }
    let now = token_now();
    token.access_token = resp.access_token.clone();
    token.refresh_token = resp.refresh_token;
    token.access_expires_at = now + resp.expires_in;
    token.refresh_expires_at = now + resp.refresh_expires_in.max(0);
    tracing::info!("[OTP-FEISHU] user token refreshed");
    Ok(resp.access_token)
}

// ── OTP relay client ───────────────────────────────────────────────────

/// An OTP code extracted from the bot's reply, together with the validity
/// window the bot reported (issue #130: replies look like
/// `otp：313786，有效期剩余：36秒`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedOtp {
    pub code: String,
    /// Remaining validity in seconds, when the bot's reply stated one.
    pub valid_secs: Option<u64>,
}

/// Parse the remaining-validity window from a bot reply, e.g.
/// `有效期剩余：36秒` / `有效期剩余: 36 秒`. Returns `None` when the reply
/// carries no recognizable window (callers then assume it is fresh).
pub fn parse_validity_secs(text: &str) -> Option<u64> {
    static VALIDITY_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = VALIDITY_RE.get_or_init(|| {
        Regex::new(r"有效期(?:剩余)?\s*[：:]?\s*(\d+)\s*秒").expect("validity regex is valid")
    });
    re.captures(text)?.get(1)?.as_str().parse().ok()
}

/// Parse an OTP from a browser-rendered bot reply using the same extraction
/// rules as the OpenAPI message path. Invalid configured regexes fall back to
/// the conservative 4–8 digit pattern used by [`FeishuOtpClient`].
pub fn parse_otp_reply(text: &str, code_pattern: &str) -> Option<FetchedOtp> {
    let code_re = Regex::new(code_pattern)
        .unwrap_or_else(|_| Regex::new(r"\b\d{4,8}\b").expect("default OTP regex is valid"));
    let code = extract_first_match(text, &code_re)?;
    Some(FetchedOtp {
        code,
        valid_secs: parse_validity_secs(text),
    })
}

/// Client that relays a one-time code request to the designated ops bot via
/// the signed-in user's Feishu identity. The destination bot is fixed at
/// construction and cannot be changed on the send path.
#[derive(Debug, Clone)]
pub struct FeishuOtpClient {
    base_url: String,
    app_id: String,
    app_secret: String,
    /// The ONLY allowed recipient (`ou_...` open id of the ops bot).
    bot_open_id: String,
    /// Message text sent to trigger the bot (e.g. "动态口令").
    request_text: String,
    code_re: Regex,
}

impl FeishuOtpClient {
    pub fn new(
        base_url: String,
        app_id: String,
        app_secret: String,
        bot_open_id: String,
        request_text: String,
        code_pattern: &str,
    ) -> Self {
        let code_re = Regex::new(code_pattern)
            .unwrap_or_else(|_| Regex::new(r"\b\d{4,8}\b").expect("default OTP regex is valid"));
        Self {
            base_url,
            app_id,
            app_secret,
            bot_open_id,
            request_text,
            code_re,
        }
    }

    /// The allowlisted recipient. Only this open id may be messaged.
    pub fn bot_open_id(&self) -> &str {
        &self.bot_open_id
    }

    /// Send an OTP request to the bot and poll the DM for the matching code.
    /// `token` is refreshed in place when stale.
    ///
    /// When the bot reports a validity window (`有效期剩余：NN秒`) that is
    /// already nearly exhausted, one fresh request is made automatically so
    /// the caller never types a code that dies mid-flight (issue #130).
    pub async fn request_otp(&self, token: &mut FeishuUserToken) -> Result<Option<FetchedOtp>> {
        let first = self.request_otp_once(token).await?;
        match first {
            Some(otp) if otp.valid_secs.is_some_and(|s| s < MIN_OTP_VALID_SECS) => {
                tracing::info!(
                    "[OTP-FEISHU] code expires in {}s (< {}s); requesting a fresh one",
                    otp.valid_secs.unwrap_or(0),
                    MIN_OTP_VALID_SECS
                );
                self.request_otp_once(token).await
            }
            other => Ok(other),
        }
    }

    /// One request/poll cycle: message the bot, poll the DM for a reply.
    async fn request_otp_once(&self, token: &mut FeishuUserToken) -> Result<Option<FetchedOtp>> {
        let access = ensure_fresh_token(token, &self.base_url, &self.app_id, &self.app_secret)
            .await?
            .ok_or(anyhow::Error::new(ReauthRequired))?;

        tracing::info!("[OTP-FEISHU] requesting OTP from bot {}", self.bot_open_id);

        // Send the request message to the bot ONLY. The `receive_id` is the
        // configured bot open id — never parameterised — so no other target is
        // addressable through this code path. The send auto-creates the p2p
        // chat if needed and returns its id, which we then poll for the reply.
        let chat_id = self
            .send_message(&access, &self.bot_open_id, &self.request_text)
            .await
            .context("failed to send OTP request to bot")?;

        // Poll the DM for a reply matching the OTP pattern.
        self.poll_for_code(&access, &chat_id).await
    }

    /// Send a plain-text message to the configured bot. Hard-allowlist: the
    /// `receive_id` argument must equal `self.bot_open_id`, enforced here even
    /// if a future caller were to pass something else.
    async fn send_message(
        &self,
        access_token: &str,
        receive_id: &str,
        text: &str,
    ) -> Result<String> {
        if receive_id != self.bot_open_id {
            return Err(anyhow!(
                "refused to send message: receive_id is not the configured ops bot"
            ));
        }
        let url = format!(
            "{}/open-apis/im/v1/messages?receive_id_type=open_id",
            self.base_url
        );
        let body = serde_json::json!({
            "receive_id": receive_id,
            "msg_type": "text",
            "content": serde_json::json!({ "text": text }).to_string(),
        });
        #[derive(Deserialize)]
        struct SendResp {
            code: i64,
            #[serde(default)]
            msg: String,
            #[serde(default)]
            data: Option<SendData>,
        }
        #[derive(Deserialize)]
        struct SendData {
            #[serde(default)]
            chat_id: String,
        }
        let client = http_client(OTP_FETCH_TIMEOUT)?;
        let resp: SendResp = client
            .post(&url)
            .bearer_auth(access_token)
            .json(&body)
            .send()
            .await
            .context("Feishu send message request failed")?
            .json()
            .await
            .context("Feishu send message decode failed")?;
        if resp.code != 0 {
            return Err(anyhow!(
                "Feishu send message error {}: {}",
                resp.code,
                resp.msg
            ));
        }
        let data = resp
            .data
            .ok_or_else(|| anyhow!("Feishu send returned no data"))?;
        Ok(data.chat_id)
    }

    /// Poll the direct-message conversation for a message matching the OTP
    /// pattern, sent (by the bot) after `since` ms-epoch.
    async fn poll_for_code(&self, access_token: &str, chat_id: &str) -> Result<Option<FetchedOtp>> {
        let client = http_client(OTP_FETCH_TIMEOUT)?;
        let start = std::time::Instant::now();
        let since_ms = chrono::Utc::now().timestamp_millis();
        loop {
            if let Some(otp) = self
                .scan_recent_messages(&client, access_token, chat_id, since_ms)
                .await?
            {
                return Ok(Some(otp));
            }
            if start.elapsed() >= OTP_REPLY_WINDOW {
                tracing::warn!("[OTP-FEISHU] timed out waiting for bot OTP reply");
                return Ok(None);
            }
            tokio::time::sleep(OTP_POLL_INTERVAL).await;
        }
    }

    /// One scan pass over the recent messages in the bot DM.
    async fn scan_recent_messages(
        &self,
        client: &reqwest::Client,
        access_token: &str,
        chat_id: &str,
        since_ms: i64,
    ) -> Result<Option<FetchedOtp>> {
        if chat_id.is_empty() {
            return Ok(None);
        }
        let url = format!(
            "{}/open-apis/im/v1/messages?container_id_type=chat&container_id={}&page_size=20&sort_type=by_create_time_desc",
            self.base_url, chat_id,
        );
        #[derive(Deserialize)]
        struct ListResp {
            code: i64,
            #[serde(default)]
            data: Option<ListData>,
        }
        #[derive(Deserialize)]
        struct ListData {
            #[serde(default)]
            items: Vec<MsgItem>,
        }
        #[derive(Deserialize)]
        struct MsgItem {
            #[serde(default)]
            body: Option<MsgBody>,
            #[serde(default)]
            create_time: String,
        }
        #[derive(Deserialize)]
        struct MsgBody {
            content: String,
        }
        let resp: ListResp = client
            .get(&url)
            .bearer_auth(access_token)
            .send()
            .await
            .context("Feishu message list request failed")?
            .json()
            .await
            .context("Feishu message list decode failed")?;
        if resp.code != 0 {
            return Ok(None);
        }
        let items = resp.data.map(|d| d.items).unwrap_or_default();
        for item in items {
            let created = item.create_time.parse::<i64>().unwrap_or(0);
            if created < since_ms {
                // Descending order — older messages follow; stop.
                break;
            }
            let content = item.body.map(|b| b.content).unwrap_or_default();
            if let Some(code) = extract_first_match(&content, &self.code_re) {
                let valid_secs = parse_validity_secs(&content);
                tracing::info!(
                    "[OTP-FEISHU] matched OTP in bot reply (content len={}, valid_secs={:?})",
                    content.len(),
                    valid_secs
                );
                return Ok(Some(FetchedOtp { code, valid_secs }));
            }
        }
        Ok(None)
    }
}

/// Extract the first regex match (capture-group-1 if present, else whole).
fn extract_first_match(text: &str, re: &Regex) -> Option<String> {
    let caps = re.captures(text)?;
    Some(caps.get(1).or_else(|| caps.get(0))?.as_str().to_string())
}

fn http_client(timeout: Duration) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .use_rustls_tls()
        .tls_built_in_root_certs(true)
        .timeout(timeout)
        .build()
        .context("failed to build reqwest client for Feishu OTP")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_challenge_is_s256_base64url() {
        // RFC 7636 test vector.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let expected = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert_eq!(code_challenge_s256(verifier), expected);
    }

    #[test]
    fn verifier_is_url_safe_and_long_enough() {
        let v = generate_code_verifier();
        assert!(v.len() >= 43 && v.len() <= 128);
        assert!(
            v.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-._~".contains(c))
        );
    }

    #[test]
    fn url_encode_escapes_reserved() {
        assert_eq!(url_encode("a b+c&d=e"), "a%20b%2Bc%26d%3De");
        assert_eq!(url_encode("simple_value-1.0~x"), "simple_value-1.0~x");
    }

    #[test]
    fn authorize_url_pkce_shape() {
        let url = build_authorize_url(
            "cli_abc",
            "http://127.0.0.1:8877/oauth/feishu/callback",
            "state123",
            "challenge456",
        );
        assert!(url.starts_with(FEISHU_ACCOUNTS_BASE));
        assert!(url.contains("code_challenge=challenge456"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=state123"));
    }

    #[test]
    fn client_pins_bot_recipient() {
        let client = FeishuOtpClient::new(
            "https://open.feishu.cn".into(),
            "cli_x".into(),
            "secret".into(),
            "ou_bot".into(),
            "动态口令".into(),
            r"\d{6}",
        );
        assert_eq!(client.bot_open_id(), "ou_bot");
    }

    #[test]
    fn token_freshness_window() {
        let now = chrono::Utc::now().timestamp();
        let mut t = FeishuUserToken {
            access_token: "a".into(),
            refresh_token: "r".into(),
            access_expires_at: now + 3600,
            refresh_expires_at: now + 86400,
            user_open_id: None,
        };
        assert!(token_access_valid(&t));
        t.access_expires_at = now + 100; // within skew window
        assert!(!token_access_valid(&t));
        assert!(token_refresh_valid(&t));
    }

    #[test]
    fn extract_first_match_prefers_group() {
        let re = Regex::new(r"(\d{6})").unwrap();
        assert_eq!(
            extract_first_match("code 123456 ok", &re).as_deref(),
            Some("123456")
        );
    }

    #[test]
    fn parses_browser_reply_with_configured_code_pattern() {
        let parsed = parse_otp_reply("otp：313786，有效期剩余：36秒", r"otp[：:]\s*(\d{6})")
            .expect("reply should contain an OTP");
        assert_eq!(parsed.code, "313786");
        assert_eq!(parsed.valid_secs, Some(36));
        assert!(parse_otp_reply("动态口令", r"(\d{6})").is_none());
    }

    #[test]
    fn parses_validity_window_from_bot_reply() {
        // Exact format from issue #130.
        assert_eq!(
            parse_validity_secs("otp：313786，有效期剩余：36秒"),
            Some(36)
        );
        // ASCII colon + spaces variant.
        assert_eq!(parse_validity_secs("有效期剩余: 5 秒"), Some(5));
        // "剩余" omitted.
        assert_eq!(parse_validity_secs("有效期 90 秒"), Some(90));
        // JSON-wrapped text content, as delivered by the message list API.
        assert_eq!(
            parse_validity_secs(r#"{"text":"otp：313786，有效期剩余：36秒"}"#),
            Some(36)
        );
        // No window stated → None (treated as fresh).
        assert_eq!(parse_validity_secs("您的验证码是 123456"), None);
    }

    #[test]
    fn reauth_required_downcasts_through_context_chain() {
        // The UI decides "pop the QR" via downcast_ref, so the marker must
        // stay reachable even when wrapped by `.context(...)` layers.
        let plain = anyhow::Error::new(ReauthRequired);
        assert!(plain.downcast_ref::<ReauthRequired>().is_some());

        let wrapped = anyhow::Error::new(ReauthRequired)
            .context("Feishu token refresh error 20037: invalid refresh_token")
            .context("outer retry layer");
        assert!(wrapped.downcast_ref::<ReauthRequired>().is_some());

        let unrelated = anyhow!("network unreachable").context("send failed");
        assert!(unrelated.downcast_ref::<ReauthRequired>().is_none());
    }
}
