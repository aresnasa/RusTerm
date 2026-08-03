# RusTerm — Architecture & Key Decisions

## Crates

- `rusterm-core` — terminal emulator core (parser, renderer, state machine)
- `rusterm-db` — SQLite primary store (OLTP). `Database` wraps `tokio-rusqlite`.
  - `store.rs`: `save_history`, `save_history_batch`, `search_history` (HAVING clause filters failed commands), `mark_command_failed` (durable failure marker), `known_failed_commands`, `delete_history_by_command` (deprecated — use `mark_command_failed`), `delete_history_by_hostname`, `all_history` (NEW — full table scan for analytics mirror).
  - `history.rs`: `HistoryEntry` struct (id, command, session_id, cwd, hostname, exit_code, duration_ms, created_at).
  - 16+ tests in `store.rs` covering HAVING clause behavior, failure markers, reimport scenarios.
- `rusterm-history` — local-only history providers (bash/zsh/fish/atuin).
  - `HistoryMatch` struct: `#[non_exhaustive]` with `command, cwd, hostname, timestamp, score, exit_code` (exit_code added 2026-07-18 for atuin propagation).
  - `HybridHistoryProvider::search()` merges all sources, dedups by command, preserves `exit_code` from atuin.
  - `AtuinDbProvider` reads `exit_code` column from atuin's `history` table (MAX aggregation biases toward "don't suggest").
  - bash/zsh/fish flat files have NO exit code → `None` → filtered by `known_failed_commands` at import time.
- `rusterm-analytics` (NEW 2026-07-18) — DuckDB-backed OLAP layer.
  - `AnalyticsDB` wraps `duckdb::Connection` in `Mutex` (Connection is Send but not Sync).
  - `AnalyticsCommand` struct: command, hostname, exit_code, created_at (subset of HistoryEntry).
  - Methods: `open`, `open_in_memory`, `record_command`, `total_commands`, `classify`, `success_rate_by_prefix`, `usage_patterns_by_time_of_day`, `behavior_summary`, `clear`.
  - `mirror::mirror_from_sqlite(&analytics, &sqlite_db)` — bulk copy from SQLite → DuckDB.
  - `classify::classify_command()` — prefix-matching into `CommandCategory` enum (Git, Docker, Kubernetes, Rust, NodeJs, Python, Go, Build, FileOps, TextProcessing, Networking, Process, Editor, Navigation, Other). Strips `sudo`/`time`/`nohup` and path prefixes.
  - 14 tests, all passing.
  - **DuckDB gotcha**: `EXTRACT(HOUR FROM TIMESTAMPTZ)` returns LOCAL hour, not UTC. Use `strftime(ts, '%H')` on a `TIMESTAMPTZ` to get UTC hour.
- `rusterm-ui` — Dioxus desktop UI.
  - `AppState` (Serialize/Deserialize) in `state.rs`: sessions, active_session, sidebar_open, connections, theme, close_senders, resize_senders, config_manager, terminals, session_logs, unlock_state, master_password_error, suggestion_epoch, pending_exit_check, recent_failed_commands (NEW), onekeys, onekey_popups, session_configs, disconnected_sessions, analytics (NEW — `AnalyticsHandle`).
  - `SessionTab`: id, name, kind, render_output, version, suggestion, suggestions, suggestion_selected, suggestion_visible, command_history, hostname.
  - `app.rs` (~2900 lines): `App()` function, `start_ssh_connection`, `start_shell_connection`, `open_local_terminal`, `reconnect_session`.
  - `analytics.rs` (NEW): `AnalyticsHandle` with feature-gated real/stub impls.
  - `components/suggestion_popup.rs`: `SuggestionPopup` component (props: suggestions, selected_index, on_select, on_dismiss, on_delete [NEW]).
  - `components/terminal_view.rs`: `TerminalView` component. Suggestion keyboard handler handles ArrowUp/Down/Tab/Escape/Shift+Delete [NEW]/Enter.
- `rusterm-app` — binary crate.
- `rusterm-ssh`, `rusterm-crypto`, `rusterm-ai`, `rusterm-plugins`, `rusterm-proto` — supporting.

## Feature Flags

- `rusterm-ui/analytics` (NEW) — enables DuckDB analytics. Default OFF. Adds ~50MB to binary (bundled libduckdb C++). When off, `AnalyticsHandle` is a no-op stub.

## Critical Patterns

- **Borrow checker in `app.rs`**: `let mut s = state.write();` then `state.clone()` fails (E0502). MUST `drop(s);` before `state.clone()`. `state_for_mark` must be `let mut` (Dioxus `Signal::write` takes `&mut self`).
- **Failed-command filter**: three layers:
  1. `recent_failed_commands: HashSet<String>` in AppState — immediate UI guard during async DB write
  2. `mark_command_failed(&cmd, rc)` in DB — durable failure marker (DELETEs prior rows, inserts single row with non-zero exit_code)
  3. `HAVING` clause in `search_history` — `SUM(exit_code = 0) > 0 OR SUM(exit_code IS NOT NULL) = 0` keeps commands with at least one success OR all-NULL (unknown)
- **Atuin exit_code propagation**: `AtuinDbProvider::search` reads `MAX(exit_code)` so any failed execution marks the command as failed. `HistoryMatch.exit_code` flows through `hybrid.rs` merge → DB import → `HAVING` filter.
- **Startup import filter**: `_history_import` use_future in `app.rs` fetches `known_failed_commands()` BEFORE building entries and filters them out, preventing re-introduction of typos as NULL-exit-code rows on every launch.
- **Shell integration (OSC 133;D)**: injected inline for zsh (`precmd_functions`) and bash (`PROMPT_COMMAND`) at L1125/L1700. Fish/nu/pwsh NOT supported — failed commands in those shells won't be detected at runtime.
- **User delete feature (Shift+Delete)**: handler in `app.rs` `on_suggestion_delete` removes from `command_history`, `suggestions`, inserts into `recent_failed_commands`, spawns `mark_command_failed(&cmd, 1)`, refreshes inline ghost text, increments `suggestion_epoch`. Uses `mark_command_failed` (NOT `delete_history_by_command`) — deletion would let next import re-introduce as NULL.

## Tests

- 325 tests pass (workspace, no features) as of 2026-07-19.
- Breakdown: 142 in rusterm-ui (was 136 — added 6 tests for Task 19 drag-tab-to-split feature in state.rs), 53 layout tests, 95 in rusterm-core, 25 in rusterm-db, 19 in rusterm-ai, 16 in rusterm-crypto, 14 in rusterm-analytics, rest in other crates.
- Total workspace test count is now 384 (was 383 after the state-level Task 19 fix; +1 for `drop_background_tab_creates_multi_pane_layout_from_single` which pins the render-path switch contract).
- 2 tests intentionally ignored in rusterm-ssh (live SSH tests, gated by `RUSTERM_LIVE_SSH_TEST=1`).
- 325 tests also pass with `--features rusterm-ui/analytics`.
- Note: the uncommitted `scan_cwd`/`session_state`/`command_safety` work in rusterm-core added ~56 tests and a `cwd: Option<PathBuf>` field to `Terminal` (mirrored as `cwd: Option<String>` on `SessionTab`). All `SessionTab` initializers in test helpers include `cwd: None`. The production `SessionTab` initializers in `app.rs` (lines ~2860, ~2920, ~2957, ~2984, ~3785) also include `cwd: None`.

## Drag-and-drop pane rearrangement (Task 16, completed 2026-07-18; performance optimization 2026-07-19)

- **Goal**: let users drag sessions between panes and drag sidebar connections onto panes to open them in specific panes (rather than always as a new active tab).
- **Design decision (grid-only)**: chose to keep the existing uniform row-major grid (every row has the same number of columns) rather than refactoring `PaneLayout` to a tmux-style binary tree. Arbitrary tree-style splits would break the `rows * cols == panes.len()` invariant that `pane_rect` and `visible_panes` rely on, and would invalidate the 41 existing layout tests. The user can still drag sessions between existing panes and drag sidebar connections onto existing panes; splitting panes is left to the existing `cycle_layout_preset` / `apply_layout_preset` path. A future task can introduce tree-based splits if needed.

### Performance optimization (2026-07-19)
- **drag_over_pane signal**: added `drag_over_pane: Signal<Option<usize>>` in `App()` (near other `use_signal` calls) and passed to `multi_pane_container`. Each pane reads `drag_over_pane()` once during `pane_items` Vec construction to compute `border_style` (highlight when `Some(idx)`). This subscribes `App` to the signal — any change triggers ONE re-render of `App` (which rebuilds `pane_items` with the new values). The Signal equality check prevents re-renders for no-op `set(Some(idx))` calls in the high-frequency `ondragover` (~60Hz) handler. The highlight changes only on `ondragenter` (when the dragged pane actually changes), not per dragover-tick. This aligns with the user's "取舍分频性能" preference: fewer re-renders over per-tick feedback.
- **Visual feedback**: panes now show a 2px solid `#7aa2f7` border when dragged-over, 2px transparent border otherwise. `box-sizing: border-box` ensures the border doesn't shift the pane's content. `ondragenter` and `ondragover` set `drag_over_pane.set(Some(idx))`; `ondrop` sets `drag_over_pane.set(None)`. Do NOT use `ondragleave` — it fires when moving between child elements (bubbling), causing flicker.
- **pane_items simplification**: reduced from 5-tuple `(idx, session_id, rect, drop_session_id, drop_pane_idx)` to 5-tuple `(idx, session_id, rect, drop_session_id, border_style)`. Replaced redundant `drop_pane_idx` (a copy of `idx`) with `idx` directly (since `usize` is `Copy`, the ondrop closure captures it without an extra clone). Added `border_style: &'static str` (Copy) pre-computed from `drag_over_pane()` during Vec construction — avoids reading the Signal inside the rsx! `for` body (which is forbidden — `let` statements aren't allowed in the for body).
- **Key stability decision (CRITICAL)**: KEPT the pane key as `pane-{idx}-{session_id}` (did NOT change to `pane-{idx}`). `TerminalView` uses `use_effect` (terminal_view.rs L777-796) and `use_future` (L825-829) that capture `session_id` by clone — these run only on mount. If `session_id` prop changed without remount, the focus/resize scripts would reference stale DOM element IDs (`terminal-input-{old_session}` vs the rendered `terminal-input-{new_session}`). So when a session swaps panes, the TerminalView MUST be remounted (which the `{session_id}` in the key ensures). This is correct behavior, not a perf bug.
- **Performance contract tests** (6 new tests, all passing):
  - `layout.rs`: `swap_panes_preserves_pane_count` (grid invariant preserved through swaps), `set_pane_session_out_of_range_returns_false_without_panicking` (O(1) bounds check), `visible_panes_yields_exactly_panes_len_when_not_zoomed` (no over/under-allocation).
  - `state.rs`: `swap_pane_sessions_only_touches_two_panes` (only 2 panes differ after swap), `set_pane_session_for_active_out_of_range_is_o1_no_panic` (O(1) failure path), `pane_index_for_active_session_returns_none_without_layout_o1` (early-return when no layout).
  - The `drag_over_pane` signal itself lives in the Dioxus runtime (not on `AppState`), so it can't be unit-tested without spinning up a Dioxus runtime. Its behavior is pinned by the call-site comments in `multi_pane_container` instead.
- **Performance characteristics**:
  - **During drag**: `ondragover` fires ~60/sec, each call is O(1) (prevent_default + set_drop_effect + Signal equality check). No re-render per dragover-tick.
  - **On pane-enter**: `drag_over_pane` value changes → ONE re-render of `App()`. Re-render is O(panes) for layout + O(terminal_size) per pane for TerminalView HTML generation. Cheap because terminal output is unchanged (prop comparison short-circuits).
  - **On drop**: 1-2 state writes (set_pane_session_for_active or swap_pane_sessions), triggers ONE re-render. Layout computation is O(panes) = O(16) max.
  - **Allocation**: one `String` clone per pane per render (for `drop_session_id`). Trivial.
- **Data layer** (`crates/rusterm-ui/src/layout.rs`):
  - `PaneLayout::set_pane_session(idx, session_id)` — already existed (line ~329); replaces the session at a pane index. Now also clears a pane when `session_id` is empty.
  - `PaneLayout::swap_panes(a, b)` — NEW. Swaps the `session_id` of two panes; re-anchors `row`/`col` to the pane INDEX (not the session), so `pane_rect` still draws each pane at its grid position. Self-swap is a no-op; out-of-range returns false.
  - `PaneLayout::swap_panes_by_session(from, to)` — NEW. Convenience wrapper that looks up pane indices by session_id.
- **State wrappers** (`crates/rusterm-ui/src/state.rs`):
  - `set_pane_session_for_active(state, pane_idx, session_id) -> bool` — replaces the session at a pane in the active tab's layout.
  - `swap_pane_sessions(state, from_session, to_session) -> bool` — swaps two panes by session id.
  - `pane_index_for_active_session(state, session_id) -> Option<usize>` — looks up the pane index displaying a session.
  - `session_at_pane(state, pane_idx) -> Option<String>` — looks up the session at a pane index.
  - All four return false/None gracefully when there's no active session, no layout, or out-of-range indices. This is what the drop handler uses to fall back to the legacy "open new tab" path.
- **UI wiring** (`crates/rusterm-ui/src/app.rs`, `components/sidebar.rs`, `components/tab_bar.rs`):
  - **Sidebar `ConnItem`**: added `draggable: true` + `ondragstart` handler that sets `application/x-rusterm-connection-id` MIME on the DragEvent's DataTransfer. Sets `drop_effect="copy"` and `effect_allowed="copy"` (semantic: dragging a sidebar connection creates a new session).
  - **TabBar tabs**: added `draggable: true` + `ondragstart` handler that sets `application/x-rusterm-session-id` MIME. Sets `drop_effect="move"` and `effect_allowed="move"` (semantic: dragging an open session moves it).
  - **`multi_pane_container` panes**: each pane `<div>` now has `ondragover` (prevent_default to allow drop) + `ondragenter` (prevent_default for cross-browser compat) + `ondrop` handler. The drop handler reads the MIME type to dispatch:
    - `application/x-rusterm-session-id` present → drag from tab bar. If target pane is empty, move the session there (and clear the source pane via `set_pane_session_for_active`). If target pane has a session, swap via `swap_pane_sessions`. If the dragged session equals the target pane's session, no-op.
    - `application/x-rusterm-connection-id` present → drag from sidebar. Looks up the `ConnectionConfig`, calls `open_connection(state, input_senders, conn, Some(pane_idx))`.
  - **`open_connection` helper** (NEW, `app.rs` ~line 2820): factors out the connection-opening logic from the sidebar's `on_connect` handler. Takes an optional `target_pane_idx: Option<usize>` parameter:
    - `None` → open as a new active tab (legacy "click to connect" flow).
    - `Some(idx)` → open AND assign the new session to pane `idx` via `set_pane_session_for_active`. If there's no layout (active tab is Single preset), falls back to making the new session active. The new session's tab is still pushed to `state.sessions` (so it appears in the tab bar), but `active_session` is NOT changed (the user's active tab stays as whatever they were looking at when they dragged).
  - The existing `on_connect` handler now just calls `open_connection(state, input_senders, conn, None)`.
- **MIME types** (custom, distinguish drag sources):
  - `application/x-rusterm-session-id` — drag from tab bar (move existing session).
  - `application/x-rusterm-connection-id` — drag from sidebar (open new connection).
- **rsx! `for` body constraint**: the dioxus 0.7 rsx! macro does NOT allow `let` statements inside a `for` loop body (the body must be a single rsx element). Workaround: pre-compute owned clones into a Vec before the `rsx!` block, then iterate over the Vec by value in the `for` loop. The `multi_pane_container` now builds a `Vec<(idx, session_id, rect, drop_session_id, drop_pane_idx)>` (5-tuple with redundant clones) before the rsx, and the for pattern destructures all 5 fields.
- **Dioxus 0.7 drag API**: `DragEvent` is in `dioxus::prelude::*` (re-exported via `dioxus_html::events::*`). `e.data_transfer()` returns a `DataTransfer` with `set_data(format, data)`, `get_data(format)`, `set_drop_effect(effect)`, `set_effect_allowed(effect)`. Available events: `ondragstart`, `ondrag`, `ondragend`, `ondragenter`, `ondragover`, `ondragleave`, `ondrop`. Both `ondragover` AND `ondragenter` must call `e.prevent_default()` for the drop to work cross-browser.
- **Borrow checker note**: the drop handler's move closures capture `state: Signal<AppState>` by copy (Signal is Copy). Calling `state.write()` inside a closure requires `state` to be declared `mut` in the enclosing function (`multi_pane_container`'s signature was updated to `mut state: Signal<AppState>`). Multiple sequential `state.write()` calls in the same closure are fine as long as each `let mut s = state.write();` is in its own scope.

## Multi-pane layout (Tasks 14 & 15, completed 2026-07-18)

- `crates/rusterm-ui/src/layout.rs` — `PaneLayout` engine: panes stored row-major with `col_fracs`/`row_fracs` (normalized to sum=1.0). `LayoutPreset` enum: `Single`, `Split2H` (1×2), `Split2V` (2×1), `Grid4` (2×2), `Grid8` (2×4). `MAX_PANES = 16`, `MIN_PANE_FRAC = 0.1`.
- State-level helpers in `state.rs`: `apply_layout_preset`, `cycle_layout_preset`, `toggle_pane_zoom` (全屏), `toggle_comparison_mode` (跨终端比对), `resize_layout_col`/`resize_layout_row`, `broadcast_targets`, `scroll_sync_targets`.
- Layouts are **per-tab** (keyed by active session id in `state.layouts: HashMap<String, PaneLayout>`). Switching tabs preserves each tab's layout.
- `apply_layout_preset` anchors the active session at pane 0 and fills remaining panes with other open sessions in tab order (deduped).
- Render path: `(Some(active), is_multi_pane)` → `multi_pane_container` renders each visible pane via `render_terminal_pane` with absolute positioning; `(Some(active), false)` → legacy single-pane path (also taken when zoomed — the zoomed pane fills the container).
- Splitter bars: click to grow left/top by 5%, right-click to shrink. `resize_col`/`resize_row` reject deltas that would push a pane below `MIN_PANE_FRAC`.
- Comparison mode (`comparison: bool` on PaneLayout): when ON, `broadcast_targets` returns all non-empty pane session_ids (deduped); when OFF, only the active session. Used by `on_input` handler in `render_terminal_pane` to broadcast keystrokes to every pane's PTY (tmux synchronize-panes style).
- Zoom (`zoomed: Option<usize>`): a zoomed pane's `pane_rect` returns the full container; other panes return `None` (hidden but preserved). Unzooming restores prior fracs exactly.
- Toolbar (status bar): `Layout: <preset>` (cycle on click), `Compare` (toggle, highlighted when on), `⤢` (zoom toggle).
- Hotkeys: Cmd/Ctrl+Shift+L cycle layout, Cmd/Ctrl+Shift+C toggle comparison, Cmd/Ctrl+Shift+F toggle zoom.
- `layout_entry_is_safe_to_remove_when_session_closes` test pins the cleanup contract: closing a session removes its entry from `layouts`.

## Suggestion-query tracing (added 2026-07-18)

- `[SUGGESTION-QUERY] STALE — spawn epoch=N but current=M (skipped)` — logged in the `on_input` spawn when the epoch check fails (a newer keystroke or the delete handler bumped the epoch).
- `[SUGGESTION-QUERY] session=… line empty — hiding popup` / `cmd_part empty (line=…) — hiding popup` — early-return paths.
- `[SUGGESTION-QUERY] session=… cmd_part=… epoch=… current_epoch=… results=… recent_failed=…` — the main outcome log, emitted right before the popup is shown/hidden. Use this to diagnose "popup doesn't appear after delete": if `results=[]`, the history sources legitimately had no match (after filtering); if `results` is non-empty but the popup still doesn't show, there's a render/Signal issue.

## Icon Assets

- `assets/icon.svg` — full-color macOS app-tile (Rust orange gradient + shield + comet cursor `>` + corner rivets)
- `assets/icon-speed-security.svg` (NEW 2026-07-18) — enhanced version with vault shield (filled), brass padlock at base, motion-tail chevron. Combines Rust-speed + local-security motifs more explicitly.
- `assets/icon-template.svg` — pure-black macOS menu-bar template (per Apple HIG). Shield ring + `> _` prompt glyph.
- `assets/icon-template.icns` — 4 sizes (16, 16@2x, 32, 32@2x).
- `assets/icon-dark.svg`, `assets/icon-a.svg`, `assets/icon-b.svg` — older variants.

## Splitter drag-resize fix (2026-07-19, REVISED)

**Bug**: "分屏无法调整" (can't drag splitter bars to resize panes) + "分屏后无法输入" (can't type after splitting).

**Prior failed approaches** (DO NOT REPEAT):
1. ❌ Overlay with `onmousemove`/`onmouseup` — fails because implicit pointer capture routes events to the splitter (mousedown target), not the overlay.
2. ❌ Splitter bar with `onmousemove`/`onmouseup` — fails because dioxus 0.7's desktop webview (WKWebView/webkitgtk/WebView2) does NOT reliably fire element-level mouse events during a button-held drag. Pointer-capture behavior is inconsistent across webviews.
3. ❌ JS `eval` + `spawn`-installed document listeners — claimed broken by "missing `return` prefix" and "race between spawn and first mousemove". The actual root cause was never confirmed.

**Current (working) fix** — document-level capture-phase listeners + polling `use_future`:

The splitter bar carries ONLY `onmousedown`. When pressed:
1. Sets `split_drag = Some(...)` (mounts a visual-only cursor overlay with `pointer-events: none`).
2. Calls `install_split_drag_js_listeners(x, y)` which `eval`s a JS IIFE that:
   - Initializes `window.__rusterm_drag_pos = 'x,y'` and `window.__rusterm_drag_done = false`.
   - Removes any prior listeners (idempotent).
   - Installs `moveHandler` (capture-phase `document.addEventListener('mousemove', ..., true)`) that writes `e.clientX,e.clientY` to `__rusterm_drag_pos` and calls `preventDefault()`.
   - Installs `upHandler` (capture-phase `document.addEventListener('mouseup', ..., true)`) that sets `__rusterm_drag_done = true` and self-removes both listeners.
   - Stores `window._rusterm_split_drag_remove` for `end_split_drag` to call.

A polling `use_future` in `App` (named `_split_drag_poll`) loops forever:
- When `split_drag` is `None`: sleeps 32ms, re-checks.
- When `split_drag` is `Some`: calls `poll_split_drag_state()` (an `async fn` that `eval`s `"return (function() { ... })()"` to read `__rusterm_drag_pos` + `__rusterm_drag_done` as a string `"x,y,done"`).
  - If `done == true`: calls `end_split_drag` (clears `split_drag`, spawns JS cleanup, restores focus). Loops back to idle (does NOT break — `use_future` only runs its closure once, so breaking would prevent subsequent drags).
  - Else: calls `apply_split_drag_step(state, split_drag, pos)` where `pos` is `x` for col drag, `y` for row drag.
- Sleeps 16ms (60Hz) between polls.

**Why this works** (and prior approaches didn't):
- Document-level capture-phase listeners ALWAYS fire, in every webview, regardless of pointer capture. They fire BEFORE element-level handlers, BEFORE pointer capture kicks in, and keep firing even when the cursor moves outside the original target.
- The polling approach avoids needing JS→Rust callbacks (which dioxus 0.7 doesn't support well — `eval` is request/response only).
- 16ms polling is fast enough for smooth dragging (60Hz, indistinguishable from native).
- The `use_future` runs forever (never breaks), so subsequent drags work without re-mounting the component.

**Focus restore after drag** (`end_split_drag` + `restore_focus_to_active_session`):
- The splitter's `onmousedown` `prevent_default` prevents the splitter from receiving focus (correct), but nothing restored focus to the pane that had it before — so keystrokes went nowhere after the drag. Fix: `end_split_drag` calls `restore_focus_to_active_session(state, 20)` which `eval`s `document.getElementById('terminal-input-{sid}')?.focus()` after a 20ms delay (lets the overlay unmount commit).
- Also called after `cycle_layout_preset` (100ms delay) — applying a new preset re-mounts panes, and the auto-focus `use_effect` in each pane's `TerminalView` may race.

**Key helpers** (all in `app.rs`):
- `SplitDragState { is_col, idx, container_extent, last_applied_pos }` — `last_applied_pos` is viewport-relative (matching `e.client_coordinates()` and JS `e.clientX`/`e.clientY`).
- `build_install_split_drag_script(x, y) -> String` — pure function that builds the JS IIFE string (extracted for unit testing).
- `install_split_drag_js_listeners(x, y)` — calls `build_install_split_drag_script` and `spawn`s the `eval`.
- `parse_split_drag_poll_response(s) -> Option<(f64, f64, bool)>` — pure function that parses the `"x,y,done"` response (extracted for unit testing).
- `poll_split_drag_state() -> Option<(f64, f64, bool)>` — async fn that `eval`s the poll script and calls `parse_split_drag_poll_response`.
- `compute_split_drag_delta(&drag, pos) -> Option<f64>` — pure function, returns the fractional delta to apply (or `None` for no-op: duplicate event or zero container extent).
- `apply_split_drag_step(state, split_drag, pos)` — wraps `compute_split_drag_delta` + `resize_layout_col`/`resize_layout_row` + `last_applied_pos` update.
- `end_split_drag(state, split_drag)` — clears `split_drag`, spawns JS listener cleanup, restores focus.
- `restore_focus_to_active_session(state, delay_ms)` — `eval`s the focus script after a delay.

**Overlay** (in `multi_pane_container`):
- VISUAL CURSOR INDICATOR ONLY. Has `pointer-events: none` so it never intercepts events.
- Shows `col-resize`/`row-resize` cursor across the whole viewport during the drag.
- Carries NO event handlers (the document-level JS listeners handle everything).

**Splitter bar** (in `render_col_splitters`/`render_row_splitters`):
- Width increased from 6px to 10px (easier to grab).
- Carries ONLY `onmousedown` (sets `split_drag`, installs JS listeners) + `oncontextmenu` (5% shrink on right-click).
- `onmousedown` calls `e.stop_propagation()` to prevent any parent handler from interfering.

**Tests** (33 new in this revision, 18 JS-bridge-specific):
- `app.rs::split_drag_tests` (8 tests, unchanged from prior revision): delta computation, viewport-coordinate fix, direction reversal, zero-extent guard, row drag, full drag sequence, focus-restore id format.
- `app.rs::split_drag_js_tests` (18 NEW tests): JS install script structure (IIFE, document capture-phase listeners, preventDefault, self-remove on mouseup, remove function stored), poll response parsing (valid/empty/malformed/negative/integer coordinates, done flag format), round-trip format consistency.
- `layout.rs::tests` (7 tests, unchanged): end-to-end mouse-drag simulations.

**Total test count**: 377 (was 373 before this fix).

## Multi-pane input routing fix (2026-07-19, Task 17/19)

**Bug**: "除了第一个分屏外的其他终端会话都无法输入命令" — in multi-pane mode with comparison OFF, only pane 0 accepted commands; panes 1..N's keystrokes were silently routed to pane 0's PTY.

**Root cause**: `render_terminal_pane`'s `on_input` handler (app.rs ~L206) used a buggy broadcast predicate:
```rust
let is_broadcast = broadcast_targets.len() > 1
    || (broadcast_targets.len() == 1 && broadcast_targets[0] != sid_clone);
```
The second clause `(len == 1 && broadcast_targets[0] != sid_clone)` was intended to handle some edge case but actually broke non-comparison multi-pane input. Since `broadcast_targets` returns `[active_session]` when comparison is OFF, pane N (N>0) had `broadcast_targets[0] = active_session != sid_clone (pane_N_session)` → `is_broadcast = true` → keystrokes were sent to `broadcast_targets` (= `[active_session]` = pane 0's PTY) instead of `sid_clone` (pane N's own PTY).

**Fix**: changed the predicate to `is_broadcast = broadcast_targets.len() > 1`. This is true ONLY when comparison is ON with 2+ non-empty pane sessions (the only case where `broadcast_targets` returns >1 entry). In all other cases, each pane sends to its own `sid_clone`.

**Critical design insight**: `active_session` is a **tab pointer**, NOT a **focused-pane pointer**. Layouts are keyed by `active_session` in `state.layouts: HashMap<String, PaneLayout>` (see `App` render path at app.rs ~L4688: `state.read().layouts.get(sid)`). Changing `active_session` on pane click would break the layout lookup (the new `active_session` might not have a layout entry → `is_multi = false` → single-pane fallback). So clicking a pane does NOT update `active_session` — it only updates DOM focus via `onclick_focus` in terminal_view.rs (L1005). This is correct: `active_session` stays as the tab's session, and each pane routes its own input to its own PTY.

**`onclick_focus` in terminal_view.rs (L1005-1016)**: only sets the local `focused` signal + calls `document.getElementById('terminal-input-{cid}')?.focus()`. Does NOT (and must not) update `active_session`. The auto-focus `use_effect` (L794-828) has a `document.activeElement` check to avoid multi-pane mount races — this is correct and should not be removed.

**Tests** (4 new in `state.rs::tests`, all passing):
- `non_comparison_multi_pane_input_routes_to_each_pane_own_session` — Split2H + comparison OFF → predicate false for all panes (the direct regression).
- `after_drag_swap_panes_input_still_routes_to_own_session` — simulates drag-and-drop pane swap via `swap_pane_sessions`, verifies predicate still false after swap (covers the "drag session onto pane → new pane can accept input" flow at the state level).
- `comparison_on_multi_pane_input_broadcasts_to_all_panes` — Grid4 + comparison ON → predicate true (pins that the fix didn't break synchronization).
- `comparison_on_single_non_empty_pane_does_not_broadcast` — Grid4 + comparison ON + only 1 non-empty pane → predicate false (edge case: only one target anyway).

**Why no dioxus-runtime test**: `on_input` is a closure inside `render_terminal_pane` which requires a Dioxus desktop runtime + webview to execute. The routing decision is purely a function of `broadcast_targets` (which IS unit-testable) + the `len > 1` predicate. The tests pin both: the `broadcast_targets` output for each scenario AND the corrected predicate. The actual DOM event flow (click pane → focus → keydown → on_input) is a webview concern that can't be unit-tested.

## Drag tab from top bar to create split pane (Task 19, completed 2026-07-19; UI wiring fix 2026-07-19)

**Goal**: dragging a session tab from the top tab bar onto a pane that already has a session should CREATE a new split pane for the dragged session (not just move/swap into existing panes). Previously, drag-drop only moved/swapped sessions between EXISTING panes — dropping a background tab (a session NOT in any pane) onto a filled pane silently failed because `swap_pane_sessions` looks up the source pane index and returns `false` when the source isn't in any pane.

**Fix (state level)**: new state helper `drop_background_tab_to_create_split(state, dragged_sid, target_pane_idx) -> DropSplitOutcome` in `state.rs`. The drop handler in `app.rs` (`multi_pane_container`'s `ondrop`) now checks `pane_index_for_active_session(state, dragged_sid)` BEFORE attempting a swap:
- If `Some(_)` → pane-to-pane drag → `swap_pane_sessions` (existing behavior).
- If `None` → background-tab drag → `drop_background_tab_to_create_split` (new behavior).

**Fix (UI wiring, 2026-07-19)** — THE KEY FIX that made the feature actually work in the running app:

The state-level fix was correct but the UI drag-drop still didn't create a split. Root cause: the App render path (`app.rs` ~L4798) has TWO branches — `(Some(sid), true) → multi_pane_container` and `(Some(sid), false) → render_terminal_pane`. The drop handlers with `drop_background_tab_to_create_split` live ONLY in `multi_pane_container`. When the user has a single session with NO layout (the most common starting state — Single preset), the App takes the `(Some(sid), false)` branch and renders `render_terminal_pane`, which returns a bare `TerminalView` with NO drop handlers. Drops from the tab bar went nowhere.

Fix: new helper `single_pane_with_drop(state, input_senders, render_sid, drag_over_pane)` in `app.rs` (~L849). Wraps `render_terminal_pane` in a `<div>` with `ondragover`/`ondragenter`/`ondrop` handlers that mirror `multi_pane_container`'s per-pane drop logic:
- `application/x-rusterm-session-id` present → if `dragged_sid == render_sid`, no-op; else if `pane_index_for_active_session(state, dragged_sid).is_some()`, no-op (session is in a hidden pane of a zoomed layout — user should unzoom); else call `drop_background_tab_to_create_split(state, dragged_sid, 0)`.
- `application/x-rusterm-connection-id` present → `open_connection(state, input_senders, conn, Some(0))` (falls back to making the new session active when there's no layout — acceptable).

The `(Some(sid), false) =>` branch now calls `single_pane_with_drop(state, input_senders, render_sid, drag_over_pane)` instead of `render_terminal_pane` directly. After the drop creates a Split2H layout, the next render sees `is_multi = true` and switches to `multi_pane_container`, which has its own per-pane drop handlers for subsequent drags.

The `drag_over_pane` signal is shared between `single_pane_with_drop` and `multi_pane_container` — the single-pane path uses `Some(0)` (the only pane) so the highlight border (`#7aa2f7`) is consistent across single-pane and multi-pane modes.

**`drop_background_tab_to_create_split` strategy** (uniform-grid model, NOT a binary tree):
1. No layout yet (Single preset) → apply `Split2H`, place dragged session in pane 1.
2. Layout has an empty pane slot → fill it via `set_pane_session_for_active` (preset unchanged).
3. No empty slots → cycle to next larger preset (`Single→Split2H`, `Split2H/Split2V→Grid4`, `Grid4→Grid8`). `apply_layout_preset` re-fills ALL panes from `state.sessions` in tab order — the dragged session is usually placed naturally. If not, manually place it in the first empty slot.
4. Already at `Grid8` (max) → return `FallbackSwap` → caller attempts `swap_pane_sessions` (which will also fail silently since the source isn't in a pane, but the contract is documented).

**`DropSplitOutcome` enum** (returned by the helper, lets the caller log/fall back):
- `Created { pane_idx }` — new pane created (preset upgraded) and dragged session placed.
- `FilledExisting { pane_idx }` — existing empty slot filled (preset unchanged).
- `FallbackSwap` — at Grid8 max; caller should attempt swap.
- `Failed` — no active session or unexpected state mutation failure.

**CRITICAL invariants preserved**:
- `active_session` is NOT updated (it's a tab pointer; the layout is keyed by it). Changing it would break the layout lookup.
- `apply_layout_preset` re-fills ALL panes — any manual `set_pane_session_for_active` must come AFTER.
- The multi-pane input routing fix (`is_broadcast = broadcast_targets.len() > 1`) is unaffected — the new pane's session is in `state.sessions` (it was dragged from the tab bar), so it has a PTY sender in `input_senders` already.
- After creating a new pane, `restore_focus_to_active_session(state, 80)` is called to restore focus after the new pane's DOM commits.

**Tests** (7 in `state.rs::tests`, all passing):
- `drop_background_tab_creates_split_when_no_layout` — Single → Split2H, dragged session in pane 1.
- `drop_background_tab_creates_multi_pane_layout_from_single` (NEW 2026-07-19) — pins that after the drop, `layout.is_multi_pane()` is true, which is what triggers the App render-path switch from `single_pane_with_drop` to `multi_pane_container`.
- `drop_background_tab_fills_existing_empty_slot` — Grid4 with 1 empty slot → fills it, preset unchanged.
- `drop_background_tab_cycles_split2h_to_grid4` — Split2H full → Grid4, dragged session placed.
- `drop_background_tab_at_grid8_returns_fallback_swap` — Grid8 full → `FallbackSwap` (no mutation).
- `drop_background_tab_returns_failed_with_no_active_session` — defensive contract.
- `after_drop_background_tab_input_routes_to_each_pane_own_session` — after creating a split, the routing predicate is still false with comparison OFF (regression for the Task 17/19 input bug).

**Why no dioxus-runtime test for `single_pane_with_drop`**: the wrapper is a dioxus `rsx!` function that requires a desktop runtime + webview to execute. Its drop logic mirrors `multi_pane_container`'s per-pane `ondrop` (which also can't be unit-tested for the same reason). The state-level helper IS unit-testable, and the `is_multi_pane()` contract test pins the render-path switch. The actual DOM event flow (drag tab → drop → state update → re-render → multi_pane_container) is a webview concern that can't be unit-tested.

## Manual mouse-based tab drag (Task 22, replaces HTML5 DnD for tabs)

**Why HTML5 DnD was abandoned for tabs**: HTML5 drag-and-drop (`draggable: true`, `ondragstart`, `ondrop`) was UNRELIABLE in dioxus 0.7's desktop webview (WKWebView on macOS, WebView2 on Windows, webkitgtk on Linux). Despite correct state-level logic (`drop_background_tab_to_create_split` — 6 passing tests) and correct UI wiring (`single_pane_with_drop` wrapper, multi-pane `ondrop` handlers), the user reported across tasks 17, 19, and 22 that dragging a tab onto a pane didn't create a split. The splitter drag-resize feature hit the SAME wall and was fixed by switching to document-level capture-phase JS listeners + polling (see the "Splitter drag-resize fix" section). Task 22 reworked tab drags to mirror that PROVEN pattern exactly.

**What was replaced**:
- `tab_bar.rs`: tab div's `draggable: true` + `ondragstart` REMOVED. Replaced with `onmousedown` (primary button only) that calls a new `on_drag_start: EventHandler<(String, String, f64, f64)>` prop (session_id, session_name, x, y). The close button got `onmousedown: stop_propagation` so closing a tab doesn't start a drag.
- `multi_pane_container` pane title bar: `draggable: true` + `ondragstart` REMOVED. Replaced with `onmousedown` (primary button only) that calls `start_tab_drag`. The `multi_pane_container` signature gained a `tab_drag: Signal<Option<TabDragState>>` param.
- `single_pane_with_drop` and `multi_pane_container` `ondrop` session-id branches: refactored to call the new `execute_tab_drop_on_pane` state function (dedup). These branches are now defensive fallbacks — the manual mouse-based system is the primary path. The connection-id branches (sidebar→pane, Task 16) remain HTML5-based and untouched.

**What was added** (mirrors the splitter drag-resize fix structure exactly):
- `TabDragState { session_id, session_name, start_x, start_y, cur_x, cur_y, dragging }` (`#[derive(Debug, Clone, PartialEq)]`) in `app.rs`.
- `TAB_DRAG_THRESHOLD = 6.0` (CSS px) — minimum cursor displacement for a mousedown to promote to a drag.
- `tab_drag_threshold_exceeded(start_x, start_y, cur_x, cur_y) -> bool` (Euclidean distance > threshold) — pure, tested.
- `build_install_tab_drag_script(x, y) -> String` — pure fn, builds JS IIFE string (unit-tested). Uses SEPARATE global variable names from the splitter (`__rusterm_tab_drag_pos`, `__rusterm_tab_drag_done`, `_rusterm_tab_drag_remove`) so the two systems can't clobber each other. **KEY difference from the splitter's script**: the `upHandler` RECORDS the final mouse position (the splitter's `upHandler` only set the done flag — a tab drop needs the release coordinates for hit-testing). Also captures `#terminal-content`'s `getBoundingClientRect()` at install time into `__rusterm_tab_drag_container_left/top` (the polling loop reads these to convert viewport-relative cursor coords into container-relative coords for hit-testing).
- `parse_tab_drag_poll_response(s) -> Option<(f64, f64, bool, f64, f64)>` — returns `(x, y, done, container_left, container_top)`; format `"x,y,done,left,top"` — pure, tested.
- `poll_tab_drag_state() -> Option<(f64, f64, bool, f64, f64)>` — async fn `eval`s the poll script.
- `install_tab_drag_js_listeners(x, y)` — `spawn`s the `eval`.
- `start_tab_drag(tab_drag, session_id, session_name, x, y)` — sets signal (`dragging: false`) + installs JS listeners.
- `hit_test_pane_at(cursor_x, cursor_y, container_left, container_top, container_w, container_h, layout: &PaneLayout) -> Option<(usize, String)>` — pure, tested. Iterates `layout.visible_panes(w, h)` (container-relative px rects); returns first pane containing the cursor. Caller handles single-pane path separately (cursor in container rect → `Some((0, render_sid))`).
- `finish_tab_drag(state, tab_drag, drag_over_pane, container_size, final_x, final_y, container_left, container_top)` (annotated `#[allow(clippy::too_many_arguments)]`) — does final hit-test, calls `execute_tab_drop_on_pane`, logs outcome, calls `restore_focus_to_active_session(state, 80)` on `SplitCreated`/`SplitFilledExisting`, clears `tab_drag` + `drag_over_pane`, spawns JS cleanup, restores focus (20ms delay).
- `_tab_drag_poll` `use_future` in `App` (mirrors `_split_drag_poll`): loops forever; idle 32ms sleep when `tab_drag` None; polls 16ms when Some. On each poll: updates `tab_drag`'s `cur_x`/`cur_y`; if `!dragging && threshold_exceeded` → `dragging = true`; if `done` → `finish_tab_drag` (only if `dragging` was true; else click cleanup); else if `dragging` → live hit-test → `drag_over_pane.set(target_idx)`. **NEVER breaks**.
- Ghost element in `App` (after the `match` block, before `AiPanel`): when `tab_drag().dragging == true`, renders `div { position: fixed; left: {cur_x+12}px; top: {cur_y+14}px; pointer-events: none; z-index: 9999; background: #24283b; border: 1px solid #7aa2f7; padding: 4px 8px; border-radius: 4px; font-size: 12px; color: #c0caf5; box-shadow: 0 2px 8px rgba(0,0,0,0.4); } "{session_name}" }`.

**Click vs drag**: `onmousedown` sets `tab_drag` with `dragging: false`. The poll loop watches the cursor; once it moves more than `TAB_DRAG_THRESHOLD` (6px) from the start, `dragging` becomes `true`. The drop is ONLY executed on `mouseup` if `dragging == true`. This preserves plain click-to-select: a mousedown with no significant mousemove is a click — the poll cleans up the signal and the tab's `onclick` fires normally. (The close button's `onmousedown: stop_propagation` ensures closing a tab doesn't start a drag.)

**Single source of truth for drop dispatch**: `execute_tab_drop_on_pane(state, dragged_sid, target_pane_idx, target_pane_session) -> TabDropOutcome` in `state.rs`. Called by BOTH the manual mouse-based `finish_tab_drag` AND the (now-residual) HTML5 `ondrop` handlers in `single_pane_with_drop` / `multi_pane_container`. This deduplication keeps the dispatch logic in one unit-testable place — the UI layers just hand off `(dragged_sid, target_pane_idx, target_pane_session)`. Logic: (1) self-drop → `NoOpSelfDrop`; (2) target empty → move (clear source if any) / assign; (3) target non-empty + src in pane → swap; (3b) target non-empty + src NOT in pane → `drop_background_tab_to_create_split` (background tab → create split).

**`TabDropOutcome` enum** (returned by `execute_tab_drop_on_pane`): `NoOpSelfDrop`, `MovedToEmptyPane { cleared_source_pane: Option<usize> }`, `AssignedToEmptyPane`, `Swapped`, `SwapFailed`, `SplitCreated { pane_idx }`, `SplitFilledExisting { pane_idx }`, `SplitFallbackSwapFailed`, `SplitFailed`.

**Tests** (38 new total):
- `state.rs::tests::execute_tab_drop_*` (7 new): self-drop no-op, pane-to-pane swap, background→create split, pane-to-empty move, background→empty assign, background→cycles preset, background→Grid8 fallback swap fails.
- `app.rs::tab_drag_tests` (31 new): `tab_drag_threshold_exceeded` (4 — below/above/Euclidean/constant), `build_install_tab_drag_script` (8 — initializes pos, clears done, IIFE, capture-phase, stores remove fn, upHandler records position, captures container offset, removes prior listeners, separate globals from splitter), `parse_tab_drag_poll_response` (8 — valid in-progress/done, empty, too few/many fields, non-numeric, negative, integer, zero offset), `hit_test_pane_at` (8 — single pane, outside container left/above, Split2H left/right/boundary, Grid4 top-right/bottom-left, container right edge exclusive).

**Total test count**: 422 (was 384 before Task 22). Clippy warnings: 62 (was 63 — actually reduced by 1 from dedup).

**HTML5 DnD REMAINS for**: sidebar→pane connection drags (Task 16, untouched). The `application/x-rusterm-connection-id` MIME type and the `ondrop` connection-id branches in `single_pane_with_drop` / `multi_pane_container` are unchanged.

**Critical invariants preserved**:
- `active_session` is NOT updated on drop (it's a tab pointer; layouts are keyed by it).
- `apply_layout_preset` re-fills ALL panes — manual `set_pane_session_for_active` must come AFTER (only relevant inside `drop_background_tab_to_create_split`).
- The multi-pane input routing fix (`is_broadcast = broadcast_targets.len() > 1`) is unaffected.
- The splitter drag-resize fix is untouched — the tab-drag system mirrors its structure but uses separate JS global names.
- `_tab_drag_poll` `use_future` NEVER breaks (loops forever; idle 32ms sleep).
- `upHandler` records final mouse position (unlike the splitter's `upHandler` — a tab drop needs the release coordinates for hit-testing).
- No re-entrant signal writes: `tab_drag.read()` borrow is dropped before `state.write()` in `finish_tab_drag`.
- The poll `use_future` only executes a drop if `dragging == true` (preserves plain click-to-select).

## Task 22 runtime bug: active-tab self-drop silently no-oped (FIXED 2026-07-19)

**Symptom**: user dragged a tab into the terminal area — nothing happened, despite the whole manual mouse-drag chain working (422 tests passing).

**Diagnosis via file logs** (`~/Library/Application Support/rusterm/logs/rusterm.log.*`, JSON lines, target `rusterm_ui::app`): 9 × `[TAB-DRAG] done flag detected ... finishing drag` entries proved mousedown → JS listeners → poll → `finish_tab_drag` all worked at runtime. But ZERO outcome logs (`created split`/`swapped`/`drop failed` are info/warn) followed — every drop fell into a then-debug-level silent branch. 4 release points were dead-center in the terminal area → hit-test must have hit pane 0 → the only possible branch was `NoOpSelfDrop`: **the user drags the ACTIVE tab (the natural gesture), the single-pane hit-test resolves to pane 0 which holds the active session itself → self-drop → silent no-op**.

**Initial fix** (`state.rs::execute_tab_drop_on_pane` case 1): self-drop with NO layout for the active session applied `Split2H` and returned `SplitCreated { pane_idx: 1 }` (pane 0 keeps active session; pane 1 auto-fills next background tab via `apply_layout_preset`'s fill order, or stays empty `""`). Multi-pane self-drop was still `NoOpSelfDrop`.

**Extended fix (2026-07-19, "自由分裂")**: self-drop in a MULTI-PANE layout now also grows the grid to the next larger preset (Split2H/Split2V → Grid4, Grid4 → Grid8), PRESERVING the existing pane arrangement (unlike `apply_layout_preset`, which re-fills all panes in tab order and would blow away manual rearrangements). Implementation: seed `PaneLayout::from_preset(next, &ids)` with the current panes' session ids in order (from_preset pads new slots with `""`), copy `comparison` flag, then auto-fill the new empty slots with background tabs not already placed (in tab order). Repeated drags thus freely create more sub-panes: 1→2→4→8. At Grid8 (8 panes) the self-drop is `NoOpSelfDrop` (max). `pane_idx` returned is the index of the first newly-created slot (for logging/focus). Layout preset inferred from pane count (NOT global `state.layout_preset` — layouts are per-tab). Existing test `execute_tab_drop_self_drop_is_noop` replaced with `execute_tab_drop_self_drop_multi_pane_grows_grid` (asserts growth + preservation). Added `execute_tab_drop_self_drop_growth_fills_new_slots_with_background_tabs` and `execute_tab_drop_repeated_self_drops_grow_until_grid8_then_noop`.

**Empty-pane placeholder**: empty-session panes (drop-zones) now render a VISIBLE dashed-border placeholder ("拖拽标签页或侧栏连接到此处") instead of nothing. Title bar shows "空白窗格". Empty panes' title bar `onmousedown` won't start a tab drag (guard `!drag_sid.is_empty()`).

**Log level change**: `finish_tab_drag`'s two silent branches (`self-drop no-op`, `release outside any pane`) upgraded from `tracing::debug!` to `tracing::info!` so future runtime diagnosis works at the default filter.

**Tests**: workspace total = 427 (was 424). +1 net for the self-drop growth tests (replaced 1, added 3) and +1 for `tab_drag_install_script_suppresses_text_selection_and_restores_it`.

## Drag-time text selection fix (2026-07-19)

**Symptom**: "拖拽时文本被错误选中" — during a tab drag, page text (terminal content + tab labels) got blue-highlighted. moveHandler's `e.preventDefault()` is insufficient in WebKit because selection started on the mousedown (before moveHandler runs).

**Fix (3 layers)**:
1. **Static CSS** `user-select: none; -webkit-user-select: none;` on tab divs (tab_bar.rs) and pane title bars (multi_pane_container).
2. **`e.prevent_default()` on tab/pane-title `onmousedown`** (primary button only) — blocks the native text-selection drag at the source. Safe for `onclick` (click still fires after a prevented mousedown per the HTML spec; splitter already uses this pattern).
3. **JS-side body-level suppression** in `build_install_tab_drag_script`: on install set `document.body.style.webkitUserSelect = 'none'` (+ `userSelect`) and `window.getSelection().removeAllRanges()`; restore (`= ''`) and clear selection in the `_rusterm_tab_drag_remove` function AND in both Rust-side cleanup evals (finish_tab_drag's end-cleanup + _tab_drag_poll's click-cleanup). Defensive: redundant restores are idempotent.

**Tests**: +1 `tab_drag_install_script_suppresses_text_selection_and_restores_it` (asserts both `webkitUserSelect = 'none'` set on install and `= ''` restored in remove fn).

## Container-size measurement fix (2026-07-19, "窗口无法被填满")

**Symptom**: panes didn't fill the window — both panes ended ~100-250px above the window bottom, full-width blank strip below. `container_size` was `None` → fallback `(1200, 800)` → panes sized 800 tall in a ~1060 tall container → bottom gap.

**Root cause**: the ResizeObserver install ran ONCE at startup in `_container_measure` `use_future`, BEFORE `#terminal-content` existed (unlock screen / no session). The `if (!el || el._rusterm_ro) return;` guard silently bailed → observer never installed → `_rusterm_container_resize_pending` never set → poll returned `''` forever → `container_size` stuck at `None`.

**Fix**: moved observer install INTO the per-tick poll script. Each 100ms tick: get `#terminal-content`; if missing return `''`; if no `_rusterm_ro` yet, install ResizeObserver + force `_rusterm_container_resize_pending = true` for an immediate measure; then if the flag is set, clear it and return `getBoundingClientRect` dimensions. The `el._rusterm_ro` guard still prevents re-install on the SAME element instance; a remounted element (fresh DOM instance, no `_rusterm_ro`) gets a fresh observer + forced measure. 100ms tick rate keeps the retry cheap.

## Diagnosis tips (macOS)**:
- Logs go to daily-rotating JSON files under `~/Library/Application Support/rusterm/logs/` (`rusterm-core::logging::init_logging`); default `EnvFilter` directive `rusterm=info` — launch with `RUST_LOG=rusterm_ui=debug` to see debug lines.
- Browser MCP can't attach to WKWebView; CGEvent mouse simulation needs Accessibility trust (`AXIsProcessTrusted()` was false for the agent terminal) — file-log forensics is the practical runtime-diagnosis channel.

## Self-drop auto-clones real terminal sessions (2026-07-19)

- `state.rs::execute_tab_drop_on_pane` now returns `TabDropOutcome::SelfDropExpanded { first_pane_idx, pane_count }` for self-drop growth (1→2→4→8). It only mutates layout and leaves all new slots empty; it no longer auto-fills them from unrelated background tabs.
- `app.rs::open_cloned_sessions_for_self_drop` consumes that outcome at all three drop entry points (manual `finish_tab_drag`, single-pane HTML5 fallback, multi-pane HTML5 fallback). It clones `state.session_configs[source_session_id]` and calls the existing `open_connection(..., Some(pane_idx))` once per new slot.
- This creates a fresh session ID, terminal model, input sender, close/resize senders, and independent SSH/local-shell task for every new pane while preserving `active_session` as the layout anchor.
- Supported automatic clone types are `ConnectionKind::Ssh` and `ConnectionKind::Shell`; missing configs or unsupported kinds log `[SPLIT-CLONE]` warnings and leave the reserved panes empty.
- `open_connection` now returns its new session ID for `[SPLIT-CLONE] source=... target_pane=... new_session=...` diagnostics.
- Regression tests assert self-drop reserves exactly 1/2/4 clone targets, preserves existing pane order, and never reuses background tabs. `cargo test --workspace` and `cargo build` pass. Runtime binary restarted as PID 49989 for manual WKWebView verification.

## Known Issues / Future Work

- DuckDB `bundled` build is slow (~2min cold). Consider pre-built libduckdb for CI.
- `AnalyticsHandle` in AppState is cloned by `Arc<Mutex<...>>` but the runtime path re-opens DuckDB on each successful command (not using the shared handle) — a future optimization should pass the handle clone into the spawn.
- No UI panel yet for analytics — the API is ready (`classify`, `success_rate_by_prefix`, `usage_patterns_by_time_of_day`, `behavior_summary`).
- Shell integration not loaded for fish/nu/pwsh — failed commands in those shells won't be filtered at runtime.
- `block v0.1.6` future-incompat warning — pre-existing, external dependency, not our code.


## Floating pane windows + embedded runtime SVG (2026-07-19)

- `Pane` now has optional normalized `FloatingPane` geometry (`x/y/width/height/z_index`). Preset grids remain the initial layout; pressing the dedicated `⠿` title-bar handle promotes all panes to floating windows and moves only the selected pane.
- Floating movement uses separate document capture-phase globals (`__rusterm_pane_move_pos`, `__rusterm_pane_move_done`, `_rusterm_pane_move_remove`) plus a never-ending 16ms Dioxus polling future. The session title retains the existing tab/session drag gesture, so window movement and session swapping/cloning do not conflict.
- Floating panes clamp inside the terminal container, preserve proportional geometry across app resizes, keep `active_session` unchanged, and use z-aware hit testing for overlaps. Grid splitters are hidden in floating mode; cycling a preset restores a grid.
- Self-drop 1→2→4→8 preserves existing floating geometries and opens real cloned SSH/shell sessions in new panes.
- `rusterm-app/build.rs` now copies the source SVG to `OUT_DIR/assets/gemini-svg.svg` in addition to rasterizing `icon.png`. Both are embedded with `include_bytes!`; the WebView receives the original SVG as a base64 `data:image/svg+xml` favicon, so packaged/debug builds never depend on runtime working-directory asset paths.

## macOS application icon fix (2026-07-19)

- Root cause of the generic `exec` Dock icon: Tao 0.34's macOS `set_window_icon` is intentionally a no-op; Dioxus `Config::with_icon` cannot set `NSApplication`'s Dock icon.
- `rusterm-app` now uses `objc2-app-kit` in `Config::with_on_window` to decode the embedded PNG into `NSImage` and call `NSApplication::setApplicationIconImage`, so `cargo run` and bare debug binaries receive the correct Dock icon.
- `build.rs` renders `assets/gemini-svg.svg` into the embedded 512px PNG, copies the original SVG for the WebView data URL, and generates a complete macOS `AppIcon.icns` iconset (16px through 1024px) via `iconutil`.
- `assets/AppIcon.icns` is the checked-in Dioxus bundle icon. `crates/rusterm-app/Dioxus.toml` declares `com.rusterm.app`, the ICNS icon, `../../assets` as the asset directory, and macOS metadata.
- `scripts/bundle-macos.sh` creates and ad-hoc signs `dist/RusTerm.app`; it includes `Contents/Resources/AppIcon.icns`, the source SVG at `Contents/Resources/assets/gemini-svg.svg`, and an `Info.plist` with `CFBundleIconFile=AppIcon.icns`.
- `scripts/verify-macos-bundle.sh` is the deterministic regression loop for bundle structure, plist metadata, ICNS format, SVG byte equality, executable permissions, and signature. `scripts/update-macos-icon.sh` refreshes the checked-in ICNS after SVG changes.

## Usage-habits collection + login scripts + privacy export (Task #64, 2026-08-02)

**Goal**: opt-in local usage-habit learning (DuckDB ranking of success/failure), per-connection login initialization scripts (expect/send DSL), a privacy toggle with explicit disclosure, and a privacy-safe JSON export. All credential material is sanitized before reaching the local store; nothing is ever auto-uploaded.

### Privacy sanitizer (`rusterm-analytics/src/sanitize.rs`, NEW)
- `pub fn contains_sensitive_material(text: &str) -> bool` — PEM private-key blocks, AWS `AKIA…`, `Authorization:` headers, ssh private-key material.
- `pub fn sanitize_command(text: &str) -> Option<String>` — `None` drops the whole line (PEM block, AKIA key); otherwise redacts secret VALUES to `***` for: env assignments whose name matches `password|passwd|secret|token|api[-_]?key|private[-_]?key` (case-insensitive), long flags `--password=`/`--token=`/etc., `Authorization:` header values, glued `-p<secret>` ONLY for `mysql`/`mysqldump`/`psql`/`mongo`, 32+ hex / 40+ base64url bare tokens. `ssh-rsa`/`ssh-ed25519`/`ecdsa` PUBLIC key blobs are preserved (they are not secret).
- **Wired into storage as defense-in-depth**: `AnalyticsDB::record_command` and `record_command_correction` call `sanitize_command` on insert — `None` ⇒ silent skip (`Ok(())`), `Some(redacted)` ⇒ persist the redacted form. So even if a caller forgets to gate, no secret reaches DuckDB.
- 20 unit tests in `sanitize.rs`; `record_command` skip/redact behavior covered in lib.rs tests.

### Habit-ranking APIs (`rusterm-analytics/src/lib.rs`)
- `pub struct CommandRanking { command, successes, failures }` with `total()` and `success_rate()`.
- `AnalyticsDB::command_rankings(limit: u32)` — per-full-command success/failure counts, most-used first; NULL-exit-code rows excluded from both buckets.
- `AnalyticsDB::high_failure_commands(max_success_rate: f64, min_observations: u64, limit: u32)` — frequently-failed commands (observations ≥ min AND success_rate ≤ max), worst first. Intended for downgrading risky suggestions (not yet wired into the suggestion query — future work).
- `AnalyticsDB::all_command_corrections()` — every typo→correction pair, most-observed first. Used by the export path.
- analytics tests: 15 → 43.

### Login-script DSL (`rusterm-core/src/login_script.rs`, NEW)
- `pub enum LoginStep { Expect{pattern}, Send{text}, SendOneKey{name}, Delay{ms} }` — `SendOneKey` resolves its credential at runtime from the unlocked OneKey library; the script text itself never contains a password.
- `pub fn parse_login_script(script: &str) -> Result<Vec<LoginStep>, LoginScriptError>` — line-based: `expect <regex>`, `send <text>`, `send_onekey <name>`, `delay <ms>`, `#` comments + blank lines ignored. `LoginScriptError` carries 1-based line number + offending text + reason.
- `ConnectionConfig` and `PersistedConnection` gained `#[serde(default)] pub login_script: Option<String>` (raw DSL text); legacy settings.json deserializes with `None`. `ConfigManager` threads it through `encrypt_connection`/`decrypt_connection`.
- 12 parser tests + 2 serde tests + 1 config_manager roundtrip test. core tests: 152 → 168.

### Login-script runtime (`rusterm-ui/src/state.rs` + `app.rs`)
- `LoginScriptRuntime { steps, idx, send_buffer, done, wait_started: Option<Instant> }` in `state.rs`; `AppState.login_scripts: HashMap<String, LoginScriptRuntime>` (serde-skip). One test pins the per-session map lifecycle.
- Pure evaluator `login_script_evaluate(steps, idx, output_tail, onekeys) -> LoginScriptEvaluation { Advance{actions,next_idx} | Wait | Finished }` in `app.rs`. Eagerly consumes consecutive Send/SendOneKey/Delay after a matched Expect; `SendOneKey` resolves from `state.onekeys` (locked library ⇒ skip step, not abort).
- `drive_login_script(state, input_senders, session_id, data)` — called from BOTH SSH and shell `SessionEvent::Output` handlers right after `check_onekey_match`. Lazily parses `session_configs[session_id].login_script` on first output; feeds the stripped last non-empty line (`login_script_output_tail`, mirrors OneKey's prompt extraction); executes sends via `input_senders` (appends `\r`); `Delay` blocks the spawned task (capped at 2000ms); per-Expect timeout `LOGIN_SCRIPT_EXPECT_TIMEOUT_SECS = 30` aborts stuck scripts. Runtime removed on session disconnect.
- Connection dialog (`connection_dialog.rs`) gained a `login_script: String` field on `NewConnectionForm` + a textarea (with DSL hint + "never paste passwords here" note). `on_create`/`on_edit` in `app.rs` propagate the form value (`None` when empty).

### Usage-habits opt-in toggle + privacy disclosure
- `PersistedConfig.collect_usage_habits: bool` (serde default **false** — opt-in). `ConfigManager::load_usage_habits_enabled()` / `save_usage_habits_enabled(bool)` (read-modify-write, mirrors `save_suggestion_settings`). Threaded through ALL 10 `save_*` PersistedConfig constructions + `read_persisted` default.
- `AppState.collect_usage_habits: bool` (serde-skip; loaded at startup alongside suggestion settings). Default `false`.
- **Gating**: the 4 analytics recording spawns (2× `record_command` on rc==0, 2× `record_command_correction` on accepted typo) are wrapped in `if state.read().collect_usage_habits { ... }`. When off, no analytics task is spawned at all (the `#[cfg(feature="analytics")]` block inside is empty when the feature is off, so this compiles in both configs).
- Settings dialog (`settings_dialog.rs`) gained a "Usage habits & privacy / 使用习惯与隐私" section: a checkbox + a disclosure box listing **what is collected** (command name + category, success/failure counts, hourly distribution, typo→correction pairs, distinct-host count) and **what is never collected** (passwords, passphrases, private keys, API tokens, OneKey credential values, env var values, remote output, full command args when a credential flag is detected). Bilingual (EN + 中文). Save/Reset buttons thread the toggle.

### Privacy-safe JSON export
- `AnalyticsHandle::build_usage_habits_report() -> Result<UsageHabitsReport>` (feature-gated). `UsageHabitsReport` (in `analytics.rs`) carries only aggregated fields: `generated_at`, `total_commands`, `classifications`, `hourly_usage`, `prefix_success_rates`, `typo_corrections`. The disabled-feature stub returns an empty report (just `generated_at`) so call sites compile without DuckDB.
- The export re-runs `sanitize_command` on every `typo_corrections` row as defense-in-depth (the storage layer already sanitized on insert).
- Settings dialog "Export privacy-safe report (JSON)" button (disabled when collection is off) fires `on_export_usage_habits`, which spawns `build_usage_habits_report` + `serde_json::to_string_pretty` + `std::fs::write` to `~/Downloads/rusterm-usage-habits-<timestamp>.json`. **No automatic upload** — the disclosure text states the user pastes the file into their uploader of choice (GitHub Gist / object storage) with their own token; RusTerm never transmits anything automatically.

### Workspace note (IMPORTANT)
- An external process periodically rewrites+commits the working tree (commits like `fix rusterm`, `Update *.rs`). Uncommitted edits get clobbered on each cycle (~1-2 min). Lesson: **commit immediately after a green test run**, or accept that work may need redoing. All Task #64 work is in commit `5da72ff` (HEAD as of 2026-08-02 12:15).
- Test totals: rusterm-analytics 43, rusterm-core 168, rusterm-ui 524 (default) / 523 (`--features analytics`). Workspace `cargo test --workspace` = all green, 0 failures.

### Future work (not done in #64)
- Wire `high_failure_commands` into the suggestion query to downgrade/remove risky suggestions (the SQLite `HAVING` clause already filters fully-failed commands; this would additionally suppress commands with low success rate but ≥1 success).
- Native Gist upload via `reqwest` (the handoff suggested it; v1 ships file export only to keep the dependency surface unchanged and to avoid auto-transmission).
- A dedicated analytics panel UI (the `behavior_summary` API has been ready since 2026-07-18; no panel exists yet).

## Sudo credential lease refresh-on-use fix (2026-08-02)

**Bug**: API requests with `elevated:true` returned `elevation_required` even though the user had just run `sudo` in a OneKey-enabled SSH session. Diagnosed via `relay-audit.jsonl` + `rusterm.log.2026-08-02`:
- 15:18:18 UTC (23:18 Beijing) — OneKey auto-submitted remembered sudo password → `cache_sudo_credential` wrote the lease with `SUDO_CREDENTIAL_TTL = 15 min` (expires 15:33:18).
- 15:28:08 UTC — second OneKey auto-submit refreshed the lease (expires 15:43:08).
- 15:46:56 UTC (23:46 Beijing) — API `exec` for `build.sh health` failed with `elevation_required` — the lease had expired ~3 min earlier.

**Root cause**: the lease TTL was a fixed 15 minutes with **no refresh-on-use**. `SudoCredentialCache::get` returned the credential without extending `expires_at`. So even if the API used the credential every 5 minutes, the lease still died 15 minutes after the *initial* OneKey submission. The user's 18-minute gap between `sudo` and the API call exceeded the 15-min window.

**Fix** (3 layers, all in `crates/rusterm-ui/src/relay_tunnel.rs`):
1. **`SudoCredentialCache::touch(key, now)`** — NEW method that refreshes `expires_at = now + SUDO_CREDENTIAL_TTL` on an existing entry. Called by the elevated executor after a successful `sudo -S` (i.e. the remote accepted the password and refreshed its own sudo timestamp). This keeps the local lease alive as long as the API keeps using it, mirroring the remote sudo timestamp.
2. **`SudoCredentialCache::get_with_status(key, now) -> SudoCredentialLookup`** — NEW method returning `Hit(Zeroizing<String>)` | `Expired` | `Missing`. The executor now distinguishes the two failure modes:
   - `Expired` → returns `relay.sudo_authorization_expired` ("...已过期...再次运行一次 sudo") — tells the user the lease *was* cached but timed out.
   - `Missing` → returns `relay.sudo_authorization_unavailable` (the original "run sudo once" message) — tells the user no lease was ever cached for this host.
   The old `get()` method is kept (`#[cfg(test)]`) for the existing host-bound test; production code uses `get_with_status`.
3. **`SUDO_CREDENTIAL_TTL` raised from 15 min to 30 min** — accommodates hosts whose sudoers raise `timestamp_timeout` above the 15-min distro default. The refresh-on-use is the primary mechanism; the longer TTL is a safety margin for the gap between the initial OneKey submission and the first API call.

**Observability** (all `tracing::info!` with `[SUDO-LEASE]` prefix, no secrets logged):
- `[SUDO-LEASE] write connection_id=... host=... port=... user=... session=... expires_in_secs=...` — on `cache()`.
- `[SUDO-LEASE] hit connection_id=... ... session=...` — on successful `get_with_status`.
- `[SUDO-LEASE] miss expired connection_id=... ... session=...` — on expired eviction.
- `[SUDO-LEASE] refresh connection_id=... ... session=... expires_in_secs=...` — on `touch()`.
- `[SUDO-LEASE] clear_for_session session=... removed=N` / `[SUDO-LEASE] clear_key connection_id=... ...` — on eviction.
These let future diagnosis distinguish "cache never written" (no `write` log) from "cache expired" (`miss expired` log) from "cache cleared by repeated-prompt rejection" (`clear_for_session` log) — the three hypotheses from the original incident.

**Executor flow** (`AppRelayExecutor::exec` elevated branch):
1. `sudo -n <cmd>` (non-interactive) — if it succeeds, use it (no lease needed; remote sudo timestamp already valid for this tty).
2. If `sudo -n` fails with auth-required → `get_with_status`:
   - `Expired` → return `ElevationRequired(sudo_authorization_expired)`.
   - `Missing` → return `ElevationRequired(sudo_authorization_unavailable)`.
   - `Hit(credential)` → `sudo -S <cmd>` with credential on stdin.
3. If `sudo -S` fails with auth-required → `clear_key` (password wrong/expired on remote) → return `ElevationRequired(sudo_authorization_rejected)`.
4. If `sudo -S` succeeds → **`touch(credential_key, now)`** (refresh local lease to match the remote sudo timestamp that was just refreshed) → return result.

**Key derivation is correct**: `sudo_credential_key(connection)` uses `connection.id` (the saved connection UUID), `ssh.host`, `ssh.port`, `ssh.username`. The OneKey submission path (`apply_onekey_popup` L6634 + `render_terminal_pane` L2420) passes `state.session_configs[session_id]` (the saved `ConnectionConfig`), and the API executor derives the key from `read_connections()` (the same saved configs mirrored into `CONNECTION_REGISTRY`). Both see the same `connection.id`, so the keys match. The `sudo_credential_key_is_identical_for_onekey_cache_and_executor_lookup` test pins this.

**Tests** (5 new in `relay_tunnel::tests`, all passing):
- `sudo_lease_expires_after_ttl_when_never_refreshed` — reproduces the incident: lease written, never refreshed, `get_with_status` at TTL boundary returns `Expired`, subsequent lookups return `Missing`.
- `sudo_lease_is_refreshed_on_successful_use_via_touch` — pins the fix: `touch` before the initial expiry extends the lease; a second `touch` extends it further; lookups past the original expiry but within the refreshed window still `Hit`.
- `sudo_lease_touch_on_missing_key_is_a_noop` — defensive: `touch` on a non-existent key doesn't panic or insert.
- `sudo_credential_key_is_identical_for_onekey_cache_and_executor_lookup` — pins that the OneKey write path and the executor lookup path derive the same key for a given saved SSH connection.
- `cache_sudo_credential_public_api_round_trips_through_executor_lookup` — end-to-end through the public `cache_sudo_credential` entry point (what `apply_onekey_popup` calls) → `get_with_status` (what the executor calls).

**i18n** (`crates/rusterm-ui/src/i18n.rs`): added `relay.sudo_authorization_expired` key (EN + 中文) distinguishing "expired" from "unavailable".

**TSM AdjustCapsLockLEDForKeyTransitionHandling**: this macOS system log line appeared in the user's terminal output alongside the 403 error but is **unrelated** — it's a macOS input-method/keyboard system message, not a RusTerm or sudo issue. Do not chase it as a root cause for elevation failures.

**Workspace test totals after this fix**: rusterm-ui 601 (was 596), relay 106, workspace all green. `cargo fmt` clean, no new clippy warnings.

**Why the fix is safe**:
- The credential value is never changed by `touch` — only `expires_at` is extended. The `Zeroizing<String>` still gets cleared on `clear_key`/`clear_for_session`/eviction.
- `touch` is only called after the remote *accepted* the password (`sudo_authorization_failed(&second)` is false). If the remote rejects it, `clear_key` drops the lease immediately. So a stale/wrong password can't be kept alive by `touch`.
- The 30-min TTL + refresh-on-use still respects the remote sudo authority: if the remote sudo timestamp expires (configurable via `timestamp_timeout`), the next `sudo -S` will fail with auth-required → `clear_key` → `sudo_authorization_rejected`. The user must re-run sudo in their terminal.
- Host/port/user isolation is unchanged: `SudoCredentialKey` still binds to `connection_id + host + port + username`. Editing a connection to a different host still produces a different key (pinned by the existing `sudo_credentials_are_host_bound_expiring_and_revocable_by_session` test).

## Output protection for huge output (tree/ls on millions of files, 2026-08-03)

**Problem**: `tree`/`ls` on a directory with millions of entries floods the SSH/PTY reader with thousands of small `SessionEvent::Output` chunks per second. The unbounded MPSC channel between reader and consumer grows without limit, the per-chunk VTE parse + render allocation overwhelms the event loop, and the session's memory footprint climbs until the process is killed.

**Fix** (commit 789ca4f, already in tree): three layers of protection in `crates/rusterm-ui/src/app.rs`:
1. **`drain_output_batch`** (~L202) — coalesces all immediately-available `SessionEvent::Output` events for the same session into a single buffer (capped at `MAX_OUTPUT_BATCH_BYTES = 1 MiB`), then processes+renders once. Cuts per-chunk overhead by 1/N.
2. **`shed_backlog_overflow`** (~L252) — when the batch hits the 1 MiB cap (meaning there's a backlog), drains remaining same-session `Output` events into a bounded tail buffer. If total backlog exceeds `MAX_BACKLOG_BYTES = 8 MiB`, drops the oldest surplus and keeps only the most recent 8 MiB (the tail — what the user cares about: end of `tree`/`ls` output, final prompt, exit codes). Mirrors hardware terminal overrun behavior.
3. **Terminal scrollback cap** (`rusterm-core/src/terminal.rs` L437) — `scrollback_capacity: 10_000` lines, trimmed on every push via `VecDeque::pop_front` (O(1)). The terminal grid itself never grows without limit.

All 4 connection types (SSH, shell, serial, telnet) use the same `drain_output_batch` + `pending_event` pattern in their event loops. Non-`Output` events (Disconnected, Error) are preserved — returned as the pending event so the outer loop processes them normally.

## Session recovery fix — stuck Reconnecting (2026-08-03)

**Bug**: "多次都没有触发会话恢复了" — after a disconnect, pressing Enter to reconnect sometimes didn't work. The session was stuck in `Reconnecting` and `begin_reconnect` only allows `Disconnected → Reconnecting`, so retries were silently rejected.

**Root cause**: `start_ssh_connection`'s `client.connect().await` call had NO overall timeout. The TCP connect has a 15s timeout (`CONNECT_TIMEOUT` in `rusterm-ssh/src/transport.rs`), but the SSH handshake and authentication phases have no timeout — a server that accepts TCP but never responds to the SSH version banner, or a keyboard-interactive auth that never sends its challenge, would hang forever. While hung, the state stays `Reconnecting` and the user can never trigger another reconnect.

**Fix** (2 layers, both in `crates/rusterm-ui/src/app.rs`):
1. **`SSH_CONNECT_TIMEOUT_SECS = 30`** — wraps the `client.connect()` call in `tokio::time::timeout`. On timeout, treats it as a connect failure (Err branch sets state to `Disconnected` + shows reconnect prompt). 30s is generous enough for a slow handshake + key auth on a high-latency link.
2. **`RECONNECT_WATCHDOG_SECS = 45`** — a `spawn` in `reconnect_session` that sleeps 45s, then checks if the session is still `Reconnecting`. If so, flips it to `Disconnected` + writes a reconnect prompt to the terminal. This is the safety net for any code path that sets `Reconnecting` but never transitions out (e.g. a dropped future, runtime overload). Set slightly above `SSH_CONNECT_TIMEOUT_SECS` so the explicit timeout fires first.

**Test**: `reconnect_retry_works_after_watchdog_flips_stuck_reconnecting_to_disconnected` — pins the before/after contract: stuck-`Reconnecting` blocks `begin_reconnect`, but once flipped to `Disconnected`, a retry succeeds.

## oh-my-zsh native integration (2026-08-03)

**Goal**: natively integrate oh-my-zsh (https://github.com/ohmyzsh/ohmyzsh) so the plugin aliases the user's shell already understands become first-class suggestions in the RusTerm UI — the "auto-popup" integration the user asked for.

**New crate**: `rusterm-ohmyzsh` (`crates/rusterm-ohmyzsh/`).
- `OhMyZsh::detect()` — finds `~/.oh-my-zsh` (or `$ZSH` env var), reads `~/.zshrc` for `plugins=(...)` and `ZSH_THEME="..."`. Returns `None` if not installed.
- `OhMyZsh::load()` — detect + read each enabled plugin's `*.plugin.zsh` file, parse `alias name='value'` definitions into an in-memory `HashMap<String, PluginAlias>`. Last-loaded plugin wins (mirrors zsh source order).
- `OhMyZsh::aliases_for_prefix(prefix, limit)` — returns aliases matching the prefix, sorted by name length (shorter first). Used by the suggestion query.
- `OhMyZsh::expand_alias(name)` — returns the expansion for a full alias name. Available for inline ghost-text (future work).
- `PluginAlias { name, expansion, plugin }` — the parsed alias struct.
- 20 unit tests (plugin parsing, theme parsing, alias parsing, prefix matching, last-loaded-wins).
- **NOT executed**: pure file reads + string parsing. No zsh scripts are run.
- **NOT installed**: reads the user's existing `~/.oh-my-zsh`; if not present, the crate is a no-op.

**Wiring** (`crates/rusterm-ui/src/app.rs` + `state.rs`):
- `AppState.ohmyzsh: Option<rusterm_ohmyzsh::OhMyZsh>` (serde-skip) — loaded once on startup.
- `_ohmyzsh_load` `use_future` in `App()` — calls `OhMyZsh::load` via `spawn_blocking` (synchronous file I/O), stores result in `state.write().ohmyzsh`. Logs `[ohmyzsh] integration active: N aliases from M plugins` or `not installed; integration inactive`.
- **Suggestion query source #4**: after the 3 existing sources (session history, SQLite FTS5, DuckDB analytics), the query checks `state.read().ohmyzsh` and calls `aliases_for_prefix(&cmd_part, sug_count * 2)`. Aliases matching the prefix AND not already seen AND not in `recent_failed_commands` are added to `all_suggestions`. The alias NAME is the suggestion (the popup shows `gst`, `gco`, etc.); the expansion is available for future ghost-text rendering.

**What this gives the user**: when typing `g`, the popup now shows oh-my-zsh git-plugin aliases (`g`, `ga`, `gaa`, `gst`, `gco`, ...) alongside history-based suggestions. When typing `d`, docker-plugin aliases appear. This is the "auto-popup" integration — the aliases surface automatically based on the user's `~/.zshrc` plugin configuration, without the shell having to be the source.

**Workspace changes**: `Cargo.toml` workspace members + deps gained `rusterm-ohmyzsh`; `rusterm-ui/Cargo.toml` gained `rusterm-ohmyzsh.workspace = true`.

**Test totals**: rusterm-ohmyzsh 20, rusterm-ui 619 (was 618 — +1 reconnect watchdog test), workspace all green.
