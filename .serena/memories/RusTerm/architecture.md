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

- 115 tests pass (workspace, no features): 23 in rusterm-db, 3 in rusterm-history (NEW), 9 in rusterm-ui state.rs, 14 in rusterm-analytics (NEW), rest in other crates.
- 115 tests also pass with `--features rusterm-ui/analytics`.

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
