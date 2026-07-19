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
- Breakdown: 136 in rusterm-ui (was 124 — added 6 performance contract tests for Task 16 optimization: 3 in layout.rs, 3 in state.rs), 53 layout tests (was 50 — added 3 perf contract tests), 95 in rusterm-core (was 39 — increased due to pre-existing uncommitted `scan_cwd`/`session_state`/`command_safety` work), 25 in rusterm-db, 19 in rusterm-ai, 16 in rusterm-crypto, 14 in rusterm-analytics, rest in other crates.
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

## Known Issues / Future Work

- DuckDB `bundled` build is slow (~2min cold). Consider pre-built libduckdb for CI.
- `AnalyticsHandle` in AppState is cloned by `Arc<Mutex<...>>` but the runtime path re-opens DuckDB on each successful command (not using the shared handle) — a future optimization should pass the handle clone into the spawn.
- No UI panel yet for analytics — the API is ready (`classify`, `success_rate_by_prefix`, `usage_patterns_by_time_of_day`, `behavior_summary`).
- Shell integration not loaded for fish/nu/pwsh — failed commands in those shells won't be filtered at runtime.
- `block v0.1.6` future-incompat warning — pre-existing, external dependency, not our code.
