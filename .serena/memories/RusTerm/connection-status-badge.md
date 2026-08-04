# Tab/Pane Connection Status Badge — "已连接" Fallback (2026-08-04)

## Problem history

User reported (twice) that the active `bidbot-prod` tab showed NO status badge while
`jumpserver` tabs showed "✓ 成功". Session was demonstrably connected (terminal output
visible) — pure UI/state issue.

### First fix attempt (commit ec35d28 "fix app status") — insufficient

Added `AppState::note_connection_outcome(session_id, failure)` (state.rs ~L1461) called from
`start_ssh_connection`'s Connected/Failed transitions: upgrades `Idle`/`Disconnected(_)` badge
to `Success` on connect, resets to `Disconnected(reason)` on connect failure. This fixed the
*connect-time* badge but the user reported "还是没有显示链接成功".

### Real root cause (badge stuck at Idle after running a command)

Badge lifecycle (`SessionTab.last_command_status`, `#[serde(skip)]`):
1. Connect → `Success` (via note_connection_outcome / mark_login_script_success).
2. User runs any command → `dispatch_approved_command` (app.rs ~L2044) resets badge to
   `Idle` ("never show stale success while a command runs") and enqueues into
   `pending_exit_check`.
3. Resolution paths:
   - OSC 133;D exit code → `decide_command_status` → Success/Failed. Also calls
     `note_exit_code_evidence` which inserts into `state.exit_code_sessions` —
     **permanently disabling** the prompt-return fallback for that session.
   - No OSC → `resolve_pending_command_via_prompt` (app.rs ~L12335) fires only if
     `prompt_return_completion_target` (state.rs ~L1677): NOT in `exit_code_sessions`,
     pending queue non-empty, and `prompt_looks_like_shell(current_line)`.

**Failure mode**: session reaches jumpserver first (RusTerm injects shell integration via
`ssh_shell_integration_setup` after 1.2s quiet → outer shell emits OSC 133;D →
`exit_code_sessions` set forever). User then hops to inner host (bidbot-prod) whose shell
emits no OSC. Every subsequent command: badge → Idle, no OSC, fallback permanently
disabled → badge stuck at Idle = no badge at all on a healthy connected session.

The permanent exclusion is a deliberate race guard (late/split-chunk OSC marker vs prompt
fallback double-resolution) — do NOT weaken it.

## Fix (this pass): Idle + Connected → green "✓ 已连接" badge

Instead of touching the resolution pipeline, the badge component gained a connection-state
fallback: when `last_command_status == Idle` AND the session's
`session_connection_states` == `Connected`, render a green "✓ 已连接" badge
(tooltip "会话已连接（尚无命令执行结果）"). A connected session now ALWAYS shows green
feedback; real command results (Success/Failed/Disconnected) still win.

### Files changed

- `crates/rusterm-ui/src/components/command_status_badge.rs`:
  `command_status_presentation(status, connected: bool)`; `CommandStatusBadge` gained
  `#[props(default)] connected: bool`. Match order guarantees: `Idle if connected` →
  connected badge; `Idle` → None; Success/Failed/Disconnected unchanged (win over
  stale connected flag). 7 tests (3 new).
- `crates/rusterm-ui/src/i18n.rs`: new keys `cmd_status.connected` ("✓ Connected" /
  "✓ 已连接") + `cmd_status.connected_tip`.
- `crates/rusterm-ui/src/components/tab_bar.rs`: TabBar passes
  `connected: conn_state == Some(SessionConnectionState::Connected)` (conn_state already
  computed for the dot color).
- `crates/rusterm-ui/src/app.rs` `multi_pane_container`: computes `pane_connected` next to
  `(title, command_status)` (~L6420), threaded through the giant `pane_items` tuple
  (17→18 elements — **must update the explicit `Vec<(...)>` type annotation at ~L6268 AND
  the destructuring `for` pattern at ~L6657**), passed to the pane title bar's
  `CommandStatusBadge`.

### Badge display matrix (after fix)

| state | badge |
|---|---|
| Idle + Connected | ✓ 已连接 (green) |
| Idle + not connected (connecting/none) | none |
| Success | ✓ 成功 (green) |
| Failed(rc) | ✗ 失败 (exit rc) (red) |
| Disconnected(reason) | ⚠ 断开 (red), wins even if conn flag stale |

### Validation

- `cargo test -p rusterm-ui --lib` → 752 passed (was 749; +3 badge tests, 1 renamed).
- `cargo build -p rusterm-ui`, `cargo build -p rusterm-app` → clean.
- nightly rustfmt on the 4 touched files; `git diff --check HEAD` clean.

### Gotchas

- `pane_items` in `multi_pane_container` has an EXPLICIT tuple type annotation; adding a
  tuple element requires updating annotation + map return + for-loop destructure (3 places).
- `SessionConnectionState` is Copy; `conn_state` in tab_bar is `Option<SessionConnectionState>`
  via `.copied()` — safe to reuse after `connection_state_dot_color(conn_state)`.
- Deeper alternative NOT taken: making `exit_code_sessions` exclusion expire so the prompt
  fallback resumes on non-integrated inner hosts. Rejected: reintroduces the documented
  late-OSC race; the connected-badge fallback satisfies the user's actual expectation
  (persistent 链接成功 indicator) without touching resolution semantics.
