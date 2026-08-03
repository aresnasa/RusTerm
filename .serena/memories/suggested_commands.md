# Suggested commands

From project root:

- Compile main UI dependency graph: `cargo check -p rusterm-ui`
- Run focused crate tests: `cargo test -p rusterm-ssh --lib`, `cargo test -p rusterm-core`
- Filter UI regressions one filter per invocation: `cargo test -p rusterm-ui startup_restore`; `cargo test -p rusterm-ui session_snapshot_`
- Full workspace when appropriate: `cargo test --workspace`
- Optional analytics: `cargo test -p rusterm-ui --features analytics`
- Check only touched Rust files when unrelated formatting exists: `rustfmt --edition 2024 --check <files>`
- Patch hygiene: `git --no-pager diff --check HEAD`; status: `git --no-optional-locks status --short`
- Read-only git commands must use `git --no-pager ...` in agent terminal sessions.
- Live SSH tests are opt-in/ignored; see env-var instructions at top of `crates/rusterm-ssh/tests/live_pwdwd_scenario.rs`.