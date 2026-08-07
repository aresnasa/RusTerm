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

## Session 5: browser window still not opening — unsaved provider draft (commit e38e99c)
User screenshot: reconnect → `2nd Password:` → OneKey popup ("sudo 密码") instead of embedded
browser. Gate analysis: OneKey winning ⇒ `feishu_owns_prompt` false ⇒ `feishu_user_cfg` None
(prompt text matches "2nd password" marker; attempts reset was already wired).
**Root cause:** the settings 扫码登录/授权 button only set `FEISHU_AUTH_REQUESTED`; the poll loop's
`start_feishu_auth_session` reads the PERSISTED config (`load_otp_webhook`), but the FeishuUser
draft is only persisted when the dialog's 保存 button fires `on_save_otp_webhook`. Unsaved draft ⇒
button silently no-ops (warn only) AND the tty gates see provider inactive ⇒ OneKey popup wins.
**Fix:**
- `render_otp_webhook_settings` now takes `on_save: EventHandler<Option<OtpWebhookConfig>>`; the
  auth button calls `on_save.call(setting())` (synchronous persist) BEFORE raising
  `FEISHU_AUTH_REQUESTED` (150ms poll ⇒ persist wins the race).
- `start_feishu_auth_session` provider-missing branch now shows the `FeishuQrPopup` Failed state
  (`feishu.qr_status_cfg_missing_fields`) instead of silently returning.
Expected flow per user directive: settings button → embedded browser visits open.feishu.cn,
creates session + chat permission → token persisted → later OTP prompts plan Fetch directly.
Note: origin/main caught up (earlier 3 commits pushed); e38e99c is the only unpushed commit.

## Session 6: proactive sign-in at connect (commit ff363a7)
User directive: the Feishu window must pop up BEFORE the JumpServer terminal login reaches
`2nd Password:` — scan first, then auto-fill the converted OTP at the prompt.
**Implemented:**
- New `feishu_auth_pending_for(state, session, now)` in feishu_oauth_flow.rs: true when a
  pending auth for that session exists and is younger than `FEISHU_QR_TIMEOUT` (abandoned scans
  age out so the prompt path can re-auth). Test: `pending_auth_lookup_is_session_scoped_and_ages_out`.
- New `maybe_preauth_feishu_at_connect(state, session_id)` in app.rs (below
  `start_feishu_auth_session`): gates = provider active → cfg complete (incomplete = warn+skip,
  no error popup on every connect) → no pending auth for session → `feishu_tty_fill_plan` is
  `Reauth`. Then `start_feishu_auth_session(Some(session))` opens the embedded window at connect.
  Valid token (plan Fetch) ⇒ nothing at connect; prompt-triggered fetch fills as before.
- Hooked into BOTH SSH connect paths (component scope, so `feishu_browser` works):
  `open_connection` SSH arm (after `start_ssh_connection`) and `reconnect_session` SSH arm
  (uses `&wd_tab_id` since `tab_id` moves into the connect fn). Non-SSH kinds not hooked.
- Anti-double-open: `trigger_feishu_otp_fetch`'s `Reauth` arm now returns early (logged
  "OTP prompt while sign-in pending — waiting for scan") when `feishu_auth_pending_for` is true —
  never rotates the nonce mid-scan; the OAuth-success handler chains `trigger_feishu_otp_fetch`
  for the session itself. The prompt's `feishu_tty_fill_begin` InFlight marker goes stale after
  45s which is fine (re-begin is attempt-capped; no begin gate on the OAuth-success chain).
`cargo check --workspace` ✅, `cargo test -p rusterm-ui` 815 pass ✅. Unpushed commits on main:
e38e99c + ff363a7 (ff363a7 also swept in previously-staged .claude/Claude.md + this memory file).

## Session 8: local Chrome/Edge replaces Wry/Obscura (uncommitted)
User explicitly rejected Obscura and embedded Wry. `feishu_browser.rs` was replaced with an external Chromium launcher/controller:
- Browser priority: Google Chrome, then Microsoft Edge; actionable failure when neither exists. Optional `RUSTERM_CHROME_PATH` / `RUSTERM_EDGE_PATH` overrides.
- Dedicated persistent profiles under the platform data directory: `rusterm/feishu-browser/chrome` and `.../edge`; never uses the user's default browser profile.
- Launch flags include `--remote-debugging-port=0`, `--remote-allow-origins=*`, no-first-run/default-browser checks, and `--new-window`.
- Reads `DevToolsActivePort`, polls loopback CDP `/json/list`, and marks the web session logged in only for an official tenant `/next/messenger` URL. Official HTTPS host validation prevents arbitrary URLs.
- Closing the RusTerm popup stops monitoring but deliberately does not kill Chrome/Edge, preserving the session.
- Removed the tao custom event handler, Wry implementation, debug smoke hook, and generic macOS `open` fallback (which could choose Safari). Popup copy/actions now explicitly say Chrome/Edge.
- Existing `2nd Password:` OneKey/login-script ownership guards and OAuth prompt timing were preserved.
Validation: Chrome and Edge executables both detected on this Mac; 7 browser unit tests passed; full `cargo test -p rusterm-ui` passed (825 + 2 doctests); `cargo check --workspace`, fmt check, diff check, and diagnostics passed. Live QR scan/CDP session reuse still needs user interaction and was not run. No commit/push.

## Still unverified end-to-end (needs live JumpServer + Feishu)
QR scan → exchange → 动态口令 → bot reply parse → tty fill. Next user retest should capture
`RUST_LOG=info` logs (~/Library/Application Support/rusterm/logs/) and grep `[OTP-FEISHU]`,
`[REPLAY]`, `[LOGIN-SCRIPT]`, `[ONEKEY-SKIP]` (new reason: `feishu_otp_prompt`).

## Session 9: v0.11 requirement already satisfied by staged tree (verified)
User restated the v0.11 goal: keep Chrome/Edge background-resident after QR sign-in; every
new/cloned session reuses the Feishu web login and just sends 2fa/动态口令 to 智小安 via CDP.
Audit found ALL of it already implemented in the staged tree (Sessions 7+8):
- Browser never killed: `close_feishu_login_window`/`hide_feishu_login_window` only stop monitoring
  + flip ACTIVE; `close` does NOT kill the process.
- Duplicate/clone (`open_connection` → `clone_session_into_pane`) routes through the same
  `start_ssh_connection` → `maybe_preauth_feishu_at_connect`; browser-flow `start` gate is
  `!is_feishu_browser_active() && !is_feishu_web_session_logged_in()` ⇒ no reopen once logged in.
- `maybe_trigger_feishu_otp` (browser flow, LOGGED_IN=true) → `StartFetch` → `trigger_feishu_otp_fetch`
  → `feishu_browser::request_feishu_otp` (智小安, `default_feishu_otp_request_text` = "动态口令",
  user-editable in settings 取码消息文本 — "2fa" also bot-supported) → CDP automate → `OtpReply` → parse
  (`parse_otp_reply`, pattern `\b\d{4,8}\b`) → `queue_feishu_otp_if_prompt_visible` → tty send.
- Resilience: `working_devtools_port` re-scans `DevToolsActivePort`; `OtpFailed` with
  `cdp_unavailable` clears LOGGED_IN → next prompt re-opens browser.
Only edit made: i18n label `settings.otp_feishu_request_text` now reads "(e.g. 动态口令 or 2fa)"
so users know either keyword works. Validation: `cargo check --workspace` clean;
`cargo test -p rusterm-ui` 835 passed, 0 failed. Staged (uncommitted) state is the deliverable;
live QR→OTP E2E still needs user retest with real Feishu/JumpServer.

## Session 7: real SSH entry bypass + OTP ownership invariant (uncommitted)
User retest showed no Feishu window and OneKey `sudo 密码` at `2nd Password:`.
- Confirmed `cargo run` resolves to the only workspace binary, `rusterm-app` → `rusterm`; stale/wrong binary was not the cause.
- Found a third SSH start path in `ConnectionDialog::on_create` calling `start_ssh_connection` directly, bypassing the proactive Feishu hook. Moved `maybe_preauth_feishu_at_connect` into the top of the single `start_ssh_connection` function, before its async SSH spawn, and removed duplicated caller hooks. Saved/open, reconnect/restore, and newly-created SSH sessions now all share the same preauth entry.
- Replaced provider-dependent `feishu_owns_prompt` with `otp_prompt_blocks_password_automation`: recognized OTP prompts always block OneKey/login scripts, even if Feishu config is unavailable or retries are exhausted. Direct terminal typing remains the manual fallback. Missing provider at an OTP prompt now invokes `start_feishu_auth_session`, which either opens auth after a re-check or shows the explicit missing-config popup; no sudo popup.
- Fixed proactive OAuth ordering: OAuth success only fetches/sends OTP when the session has an InFlight OTP cycle AND the current rendered terminal line still matches an OTP prompt. A fast QR scan before `2nd Password:` now persists the token and waits for the later prompt instead of typing OTP into an earlier login/menu stage.
- Added regressions: `jumpserver_otp_prompt_never_creates_a_password_popup`, `oauth_success_fetches_only_for_an_already_visible_otp_prompt`, and strengthened prompt gate tests.
Validation: `cargo test -p rusterm-ui` 817 tests + 2 doctests passed; `cargo check --workspace` passed; rust-analyzer diagnostics clean. Live Feishu/JumpServer E2E still requires user credentials and was not run. No commit/push made.

## Session 10: live CDP HTTP bug + stuck popup fixes (uncommitted)
User ran a build from HEAD at 03:07/04:00 CST and hit: browser spawned OK, but popup showed
"浏览器已启动，但无法连接本地 CDP 调试端口" then stayed stuck on "正在向智小安获取临时密码…".

Root causes found + fixed:
1. cdp_http_request (crates/rusterm-ui/src/feishu_browser.rs) read the DevTools HTTP response
   with `stream.take(MAX).read_to_end()`. Chrome's DevTools server is HTTP/1.1 keep-alive: it
   sets Content-Length and does NOT close the connection → read_to_end blocked until the 2 s
   read timeout → every `working_devtools_port` / `fetch_cdp_targets` / `activate_cdp_target`
   failed at runtime. curl worked because it reads exactly Content-Length bytes. Fixed by
   `read_http_response` + `parse_http_head`: read headers to `\r\n\r\n`, honour Content-Length,
   bounded fallback only when no length; dropped the bogus `Content-Length: 0` on GET. Added
   `--remote-allow-origins=*` to the launch plan. Tests assert the new reader completes
   without ever polling past the declared body.
2. Popup lifecycle had no failure/timeout arm for the OTP fetch phase: OtpFailed/OtpReply-error
   only updated `feishu_otp_status[session]` but left `FeishuQrPopupStatus::Scanning`, so the
   banner kept saying "正在向智小安获取临时密码…". Fixed in app.rs: OtpFailed and OtpReply
   parse-failure now mark the session popup Failed; successful queue marks it Delivered; a
   FEISHU_OTP_CYCLE_WATCHDOG (180 s) fails any cycle whose browser thread never reported back;
   new i18n `feishu.qr_otp_timeout` ("获取临时密码超时，请重试").
3. OtpSendReady approval was a single-shot strict current-line check — if the rendered `2nd
   Password:` line beat the drain tick, approval denied within 3 s and 智小安 never got the
   message (user's exact complaint). Now a grace loop retries the check for 2.4 s
   (FEISHU_OTP_SEND_APPROVAL_GRACE) before denying, well within the backend's 3 s window.

Validation: `cargo fmt -p rusterm-ui`; `cargo test -p rusterm-ui` 841 passed/0 failed;
`cargo build -p rusterm-ui` clean; clippy shows no NEW warnings in the touched lines
(pre-existing collapsible-if warnings elsewhere, ignored).

Still pending: real retest on the user's Mac against the live Chrome instance (DevTools port
alive) + real Feishu/JumpServer to confirm: scan → auto 智小安 message → OTP auto-filled,
and that the popup never sticks on the fetching banner when automation fails.

## Session 11 (2026-08-07 late)

User retest: CDP works, browser logs in, but bot never searched. Read logs and live CDP.

Findings from live CDP (python websocket-client to port 60226):
- s1-s2: messenger page has ZERO `<input>` in steady state; only `.appNavbar-search-input` DIV opens
   the ⌘+K palette. Hidden decoys exist: `ud__select__selector__search__input` (negative-Y,
   inside folded alert cards) does match `input[class*=search]`.
- s5: palette editor = `div[contenteditable="true"].zone-container:not(.innerdocbody)`, ancestor
   id `search_bar_editor` exists (s6 chain). Bot cards: `.bot-result-card`, name in
   `.bot-chatter-info-name`, robot label `.bot-tag`="机器人". Both 智小安 and OTP-智小安 appear.
- s7-s8: composer `innerdocbody` `innerText` tail = `…。​\n​\n​` (ZWSP+newlines); JS `.trim()`
   does NOT strip ZWSP → old strict `editor_text != request_text` always failed
   (`发送框内容校验失败` at 05:24:13). Confirmed len 24 vs expected 19.
- s9: `.chatWindow_chatName` = "智小安" ✓, `.messageItem-wrapper[data-id]` count 11 ✓.
- s11: `is_self` via `wrapper.querySelector('.message-self')` works (own message has that class).

Root causes fixed in commit `bd64bdc` (fix 飞书扫码：机器人搜索与消息发送; on
`fix/feishu-cdp-http`, pushed to origin):
- New `type_into_editor`: 6 retries alternating execCommand('insertText') (DOM-driven,
  self-verifying) and CDP `Input.insertText` (with focus priming), each preceded by
  re-clear; read-back strips U+200B then trims before equality.
- `search_editor_finder`/`composer_finder`: layered selectors with dialog-exclusion fallback.
- Bot candidates: primary `.bot-chatter-info-name` within `.bot-result-card` (robot label
  checked); fallback finds bare 「机器人」 badge in span/div/p/li and walks up to the small
  card, EXCLUDING anything under history/recent containers (stale chips must not match).

Codegraph caveat: codegraph index only walks committed HEAD, so it showed hundreds-of-lines
STALE automate_feishu_otp. The real working tree had a newer staged (never committed) rewrite
from earlier session (already includes `type_into_editor`, candidates fallback, etc.) which is
what user-log analysis relied on. Always `git status -s` + `git diff --cached` first.

Branch state at session end: `fix/feishu-cdp-http` @ bd64bdc (pushed matches origin).
User still needs to restart RusTerm (running binary was built at b6df836) and retest.
