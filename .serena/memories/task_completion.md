# Task completion checks

Use the narrowest applicable checks first, then broaden:

1. Run new/focused regression test and observe it fail before fix when diagnosing a bug; rerun green after fix.
2. Run affected crate tests (for SSH changes: `cargo test -p rusterm-ssh --lib`).
3. For UI/session work: `cargo check -p rusterm-ui`, then relevant UI filters such as `startup_restore` and `session_snapshot_`.
4. Run `cargo test -p rusterm-core` when core/session persistence changed.
5. `rustfmt --edition 2024 --check <only touched Rust files>`.
6. `git --no-pager diff --check HEAD` and inspect final scoped diff/status.
7. Do not claim workspace/UI validation if an unrelated concurrent edit blocks compilation; report exact file/error and still report narrower checks that passed.
8. Verify temporary `[DEBUG-*]`, `dbg!`, and throwaway harnesses are removed. Durable in-process protocol regression tests may remain.