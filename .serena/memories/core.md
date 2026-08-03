# RusTerm project map

- Rust 2024 Cargo workspace; desktop binary in `crates/rusterm-app`.
- Terminal/session domain in `crates/rusterm-core`; Dioxus desktop orchestration/UI in `crates/rusterm-ui`; SSH/PTTY/SFTP/transport in `crates/rusterm-ssh`.
- Supporting crates: SQLite OLTP (`rusterm-db`), local history (`rusterm-history`), crypto (`rusterm-crypto`), protocol adapters (`rusterm-proto`), relay/tunnel (`rusterm-relay`, `rusterm-tunnel`), optional DuckDB analytics (`rusterm-analytics`).
- Interactive SSH lifecycle invariant: request PTY with valid dimensions and cooked modes before `request_shell`; UI session state must transition to Disconnected when reader/writer terminates.
- `active_session` is a tab/layout anchor, not focused-pane identity.
- More historical architecture details: `mem:RusTerm/architecture`; encrypted config sync: `mem:RusTerm/sync-crate`.
- Toolchain/dependency details: `mem:tech_stack`; project-specific style: `mem:conventions`; completion checks: `mem:task_completion`.