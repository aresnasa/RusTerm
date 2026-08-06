# Issue #130 — Feishu QR → OTP → JumpServer auto-fill (builds on #129, see `mem:otp-webhook-settings-129`)

## Bug 1 (fixed earlier): ~420 masked chars = OSC 133 shell-integration script
`shell_integration_blocked_by_prompt(line)` gate in app.rs; injector re-checks prompt line after
each quiet window. Tests: `credential_prompts_block_shell_integration_injection` etc.

## Bug 2 (fixed earlier): reauth fallback + refresh-token rotation + OAuth listener port
- `ReauthRequired` typed marker (rusterm-ssh/feishu_otp.rs); `trigger_feishu_otp_fetch` downcasts,
  clears token, opens QR via `start_feishu_auth_session`.
- Rotated refresh token persisted after `request_otp`.
- `AppState.feishu_oauth_port` plumbed into `insert_pending_auth(port)`.

## Bug 3 (THIS session): reconnect showed ~36 masked chars in `2nd Password:` and no QR
Root causes and fixes (all in rusterm-ui):

1. **Feishu attempt cap never reset across connections** — `feishu_otp_attempts`/`feishu_otp_status`
   survived disconnect+reconnect; after 3 failures `feishu_tty_fill_begin` returned false forever →
   QR/fetch never started again on that tab. `feishu_otp_session_closed` existed but was ONLY called
   from `close_session` (state.rs) — never on disconnect/reconnect.
   **Fix:** wired `feishu_otp_session_closed(&mut s, id)` into: `reconnect_session` cleanup block,
   SSH + shell `SessionEvent::Disconnected` handlers, `disconnect_session_state`, and after
   `clear_onekey_session_runtime` at serial+telnet disconnect sites.

2. **OneKey auto-submit + login scripts owned the OTP prompt** — a broad `password` expect matches
   `2nd Password:` and auto-types the stored login password (or a bastion selection).
   **Fix:** new pure gate `feishu_owns_prompt(provider_active, line, attempts)` in
   feishu_oauth_flow.rs (true when FeishuUser active + `looks_like_feishu_otp_prompt` + attempts <
   FEISHU_OTP_MAX_ATTEMPTS; releases the prompt to OneKey as manual fallback when exhausted).
   Wired into:
   - `onekey_popup_for_output` → returns `Err(ONEKEY_SKIP_FEISHU_OTP)` (line check first so the
     config read only happens on OTP prompts);
   - `drive_login_script` → holds the script (early return, logged `[LOGIN-SCRIPT] … paused at OTP
     prompt`) while feishu owns the tail line.
   Test: `feishu_prompt_ownership_gates` in feishu_oauth_flow.rs.

3. **Replay first-op race** — first replay op had no echo baseline; a quiescence-only wait can
   elapse in the connected→first-remote-byte gap (koko queries core API before printing MFA
   prompt), so the first op (asset id, ~36 chars — matches user's masked injection) was typed
   blindly into the just-arriving `2nd Password:` before the credential guard saw it.
   **Fix:** step 3.5 in `schedule_replay_after_reconnect`: bounded wait
   (`REPLAY_FIRST_OUTPUT_GRACE_SECS = 5`) for ANY output after the replay notice before op 1;
   silent remotes proceed after grace (per-op quiescence + credential guard still apply).
   The credential guard itself covers the prompt: `credential_kind("2nd Password: ******")` is
   Some (asserted in `credential_prompt_resolution_is_detected_via_credential_kind`).

4. **Gate logging** — `maybe_trigger_feishu_otp` restructured: prompt check FIRST (avoids per-chunk
   config disk read via `feishu_user_cfg` → `load_otp_webhook` → `read_persisted`), then logs
   `[OTP-FEISHU] session=… OTP prompt detected but FeishuUser provider inactive` or
   `… detected begin={} attempts_before={} status_before={in_flight|delivered|failed|none}`.
   `FeishuOtpFetch` imported into app.rs use crate::state::{…}.

## Session 4 (v0.2 iteration): embedded browser replaces self-rendered QR
User reported (image_12.png) OneKey popup at `2nd Password:` + "还是没法正确的生成二维码"; cited
obscura (github.com/h4ckf0r0day/obscura) as the pattern → integrate a minimal embedded browser.

**Root cause of the QR approach being fundamentally broken:** rendering the authorize URL as a QR
and scanning it moves BOTH login and OAuth redirect onto the phone — the phone has no
`127.0.0.1:8878` listener, so the desktop never receives the code.

**Fix — embedded wry browser window (new module `crates/rusterm-ui/src/feishu_browser.rs`):**
- `open_feishu_login_window(url)`: `dioxus::desktop::window().new_window(VirtualDom::new(FeishuBrowserLoading), cfg)`
  (dioxus-desktop 0.7 `PendingDesktopContext::try_resolve().await` → `ctx.webview.load_url(&url)`)
  navigates a second WebView window to the Feishu authorize page. Feishu renders its OFFICIAL QR
  there; phone scan authorizes the desktop webview → redirect reaches the loopback listener →
  existing exchange pipeline unchanged. WKWebView persistent cookies ⇒ next sign-in auto-completes
  without re-scan (satisfies "reuse valid session").
  Window: 480x700, always-on-top, `WindowCloseBehaviour::WindowCloses` (child must really close;
  main window uses WindowHides). Handle kept in thread-local `Weak<DesktopService>`; reopen
  re-focuses + `load_url`s the existing window.
- `close_feishu_login_window()`: called from `handle_feishu_oauth_event` as soon as a recognized
  callback drains (Exchange or Failed plan; Ignore returns early), and from popup `on_close`.
- `start_feishu_auth_session` now captures `insert_pending_auth`'s returned authorize_url and calls
  `open_feishu_login_window`.
- `FeishuQrPopupView` (components/feishu_qr_popup.rs): qrcode SVG block REMOVED → instruction panel
  (📱 + `feishu.qr_embedded_hint`); new `on_embedded` handler/button (primary) reopens the window;
  browser-open + rescan kept. `qrcode` dep removed from rusterm-ui/Cargo.toml (workspace entry stays).
- i18n: new keys `feishu.qr_embedded_hint`, `feishu.qr_open_embedded`, `feishu.browser_title`,
  `feishu.browser_loading`; subtitle/help/rescan texts reworded (重新授权).
- All feishu_browser fns require Dioxus runtime scope (all call sites are component handlers/futures).

## Verified
`cargo check --workspace` clean; `cargo test -p rusterm-ui` 814 pass; clippy warnings only (pre-existing).
Committed on main (3 commits ahead of origin, NOT pushed): 9bf697b (QR OAuth + OTP autofill),
3f4a549 (app.rs), a4515e2 (embedded browser replaces self-rendered QR, #130).

## Still unverified end-to-end (needs live JumpServer + Feishu)
QR scan → exchange → 动态口令 → bot reply parse → tty fill. Next user retest should capture
`RUST_LOG=info` logs (~/Library/Application Support/rusterm/logs/) and grep `[OTP-FEISHU]`,
`[REPLAY]`, `[LOGIN-SCRIPT]`, `[ONEKEY-SKIP]` (new reason: `feishu_otp_prompt`).
