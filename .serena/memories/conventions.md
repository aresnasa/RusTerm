# Project conventions

- Keep changes surgical in this frequently concurrently-modified workspace; re-check status/diff before and after edits. Never reset or overwrite unrelated staged/unstaged work.
- Use `anyhow` at application boundaries and typed errors where crates expose domain APIs.
- Async session I/O uses Tokio MPSC channels and `SessionEvent`; failure/close must propagate to UI state instead of leaving a false Connected state.
- Interactive SSH uses CR for Enter plus PTY `ICRNL`; request PTY before shell, with nonzero rows/cols and standard cooked modes.
- Background SSH features must use separate channels. Never inject background history commands into or suppress output from the user's live PTY.
- Dioxus `Signal::write` needs mutable signal bindings; drop a write guard before cloning/re-borrowing the same signal.
- Preserve existing comments only when they explain non-obvious constraints; tests should target protocol/state seams rather than prompt text.
- Do not broadly run `cargo fmt --all` if unrelated legacy formatting blocks it; format/check touched files only.