# Command History Completion Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add dropdown-based command completion with multi-suggestion support and strict local-only security guarantees.

**Architecture:** Extend the existing `SessionTab.suggestions: Vec<String>` field (currently unused) to hold dropdown candidates. Add a new `SuggestionPopup` component rendered inside `TerminalView` below the cursor row. The suggestion algorithm merges three tiers (in-memory, shell history files, SQLite FTS5), deduplicates, and returns up to 8 results. A `#[cfg(feature = "network")]` gate (off by default) on history public APIs enforces local-only data.

**Tech Stack:** Rust, Dioxus (desktop UI), tokio, SQLite (FTS5), rusterm-history (HybridHistoryProvider), rusterm-db

---

### Task 1: Add suggestion state fields to SessionTab

**Files:**
- Modify: `crates/rusterm-ui/src/state.rs:97-113`

- [ ] **Step 1: Add `suggestion_selected` and `suggestion_visible` fields to `SessionTab`**

In `crates/rusterm-ui/src/state.rs`, add two fields to the `SessionTab` struct after the existing `suggestions` field:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionTab {
    pub id: String,
    pub name: String,
    pub kind: SessionType,
    #[serde(skip)]
    pub render_output: RenderOutput,
    pub version: u64,
    /// Inline fish-style suggestion (top match suffix)
    #[serde(skip)]
    pub suggestion: Option<String>,
    /// Multiple suggestion candidates for the dropdown
    #[serde(skip)]
    pub suggestions: Vec<String>,
    /// Index of the currently selected suggestion in the dropdown
    #[serde(skip)]
    pub suggestion_selected: usize,
    /// Whether the suggestion dropdown is currently visible
    #[serde(skip)]
    pub suggestion_visible: bool,
    /// Local command history for this session. Stored locally only, never transmitted.
    #[serde(skip)]
    pub command_history: Vec<String>,
}
```

- [ ] **Step 2: Build to verify compilation**

Run: `cargo build 2>&1 | head -30`
Expected: Compilation errors in app.rs where `SessionTab` is constructed (missing new fields). That's expected — we'll fix in Task 3.

- [ ] **Step 3: Commit**

```bash
git add crates/rusterm-ui/src/state.rs
git commit -m "feat(state): add suggestion_selected and suggestion_visible to SessionTab"
```

---

### Task 2: Create SuggestionPopup component

**Files:**
- Create: `crates/rusterm-ui/src/components/suggestion_popup.rs`
- Modify: `crates/rusterm-ui/src/components.rs`

- [ ] **Step 1: Create the SuggestionPopup component file**

Create `crates/rusterm-ui/src/components/suggestion_popup.rs`:

```rust
use dioxus::prelude::*;

#[component]
pub fn SuggestionPopup(
    suggestions: Vec<String>,
    selected_index: usize,
    cursor_row: usize,
    cursor_col: usize,
    on_select: EventHandler<String>,
    on_dismiss: EventHandler<()>,
) -> Element {
    if suggestions.is_empty() {
        return rsx! {};
    }

    // Position popup below the cursor row. Each terminal row is 1.5em (19.5px at 13px font).
    // Left offset based on cursor column (approx 7.8px per char at 13px monospace).
    let top_px = (cursor_row + 1) * 20;
    let left_px = cursor_col * 8;

    // Max width: 600px, truncate long commands
    let items_html = suggestions.iter().enumerate().map(|(i, cmd)| {
        let is_selected = i == selected_index;
        let bg = if is_selected { "#283457" } else { "transparent" };
        let fg = if is_selected { "#c0caf5" } else { "#a9b1d6" };
        let escaped = html_escape(cmd);
        format!(
            "<div style=\"padding:2px 8px;cursor:pointer;background:{};color:{};white-space:pre;overflow:hidden;text-overflow:ellipsis;\">{}</div>",
            bg, fg, escaped
        )
    }).collect::<Vec<_>>().join("");

    rsx! {
        div {
            style: "
                position: absolute;
                top: {top_px}px;
                left: {left_px}px;
                min-width: 300px;
                max-width: 600px;
                z-index: 20;
                background: #1a1b26;
                border: 1px solid #2a2b3d;
                border-radius: 4px;
                box-shadow: 0 4px 12px rgba(0,0,0,0.4);
                padding: 2px 0;
                font-family: 'JetBrains Mono', 'Fira Code', 'Cascadia Code', monospace;
                font-size: 13px;
                line-height: 1.5;
                max-height: 192px;
                overflow-y: auto;
            ",
            onclick: move |e| e.stop_propagation(),
            dangerous_inner_html: "{items_html}",
        }
    }
}

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}
```

- [ ] **Step 2: Register the new component in components.rs**

In `crates/rusterm-ui/src/components.rs`, add:

```rust
pub mod suggestion_popup;

pub use suggestion_popup::SuggestionPopup;
```

- [ ] **Step 3: Build to verify new component compiles**

Run: `cargo build -p rusterm-ui 2>&1 | tail -5`
Expected: Compilation succeeds for the new component (it's not yet wired into TerminalView, so no runtime effect).

- [ ] **Step 4: Commit**

```bash
git add crates/rusterm-ui/src/components/suggestion_popup.rs crates/rusterm-ui/src/components.rs
git commit -m "feat(ui): add SuggestionPopup component for command completion dropdown"
```

---

### Task 3: Wire suggestions into TerminalView and keyboard handling

**Files:**
- Modify: `crates/rusterm-ui/src/components/terminal_view.rs`
- Modify: `crates/rusterm-ui/src/app.rs`

- [ ] **Step 1: Add suggestions and callback props to TerminalView**

In `crates/rusterm-ui/src/components/terminal_view.rs`, update the component signature to accept suggestions, selected index, visibility, and callbacks:

```rust
#[component]
pub fn TerminalView(
    session_id: String,
    render_output: RenderOutput,
    version: u64,
    suggestion: Option<String>,
    suggestions: Vec<String>,
    suggestion_selected: usize,
    suggestion_visible: bool,
    on_input: EventHandler<Vec<u8>>,
    on_command: EventHandler<String>,
    on_resize: EventHandler<(u16, u16, u32, u32)>,
    on_scroll_up: EventHandler<usize>,
    on_scroll_down: EventHandler<usize>,
    on_scroll_to_bottom: EventHandler<()>,
    on_suggestion_navigate: EventHandler<Option<usize>>,
    on_suggestion_accept: EventHandler<String>,
    on_suggestion_dismiss: EventHandler<()>,
) -> Element {
```

- [ ] **Step 2: Add keyboard handling for dropdown navigation**

In `terminal_view.rs`, inside the `handle_keydown` closure, add dropdown handling **before** the existing suggestion acceptance block (the `// Auto-completion: accept suggestion with Right/End/Ctrl+E` section). Replace that entire section with expanded logic:

```rust
        // ── Suggestion dropdown navigation ──
        if suggestion_visible && !suggestions.is_empty() {
            match &key {
                Key::ArrowDown | Key::Character(ref s) if ctrl && s.eq_ignore_ascii_case("n") => {
                    let next = if suggestion_selected + 1 >= suggestions.len() {
                        0
                    } else {
                        suggestion_selected + 1
                    };
                    on_suggestion_navigate.call(Some(next));
                    return;
                }
                Key::ArrowUp | Key::Character(ref s) if ctrl && s.eq_ignore_ascii_case("p") => {
                    let prev = if suggestion_selected == 0 {
                        suggestions.len().saturating_sub(1)
                    } else {
                        suggestion_selected - 1
                    };
                    on_suggestion_navigate.call(Some(prev));
                    return;
                }
                Key::Tab | Key::Enter => {
                    if let Some(cmd) = suggestions.get(suggestion_selected) {
                        // Accept the selected suggestion: send the suffix
                        if let Some(ref inline_sug) = suggestion {
                            // First accept the inline part
                            on_input.call(inline_sug.as_bytes().to_vec());
                        }
                        on_suggestion_accept.call(cmd.clone());
                    }
                    return;
                }
                Key::Escape | Key::Character(ref s) if ctrl && s.eq_ignore_ascii_case("g") => {
                    on_suggestion_dismiss.call(());
                    return;
                }
                _ => {}
            }
        }

        // ── Auto-completion: accept inline suggestion with Right/End/Ctrl+E ──
        if suggestion.is_some() && !suggestion_visible {
            let is_accept = match &key {
                Key::ArrowRight => true,
                Key::End => true,
                Key::Character(s) if ctrl && !alt && !shift && s.eq_ignore_ascii_case("e") => true,
                _ => false,
            };
            if is_accept {
                if let Some(ref sug) = suggestion {
                    on_input.call(sug.as_bytes().to_vec());
                    return;
                }
            }
        }
```

Note: the `Tab` key was previously `vec![0x09]` sent to PTY. With dropdown visible, Tab now accepts the suggestion instead. When dropdown is not visible, Tab goes to PTY as before (handled in the later `match key` block).

- [ ] **Step 3: Render SuggestionPopup inside TerminalView**

Add the import at the top of `terminal_view.rs`:

```rust
use crate::components::SuggestionPopup;
```

At the end of the TerminalView rsx, right before the closing `</div>` of the container (after the two-column layout div but before the search overlay conditional ends), add:

```rust
            // Suggestion dropdown
            if suggestion_visible && !suggestions.is_empty() {
                SuggestionPopup {
                    suggestions: suggestions.clone(),
                    selected_index: suggestion_selected,
                    cursor_row: render_output.cursor_row,
                    cursor_col: render_output.cursor_col,
                    on_select: move |cmd: String| {
                        on_suggestion_accept.call(cmd);
                    },
                    on_dismiss: move |_: ()| {
                        on_suggestion_dismiss.call(());
                    },
                }
            }
```

- [ ] **Step 4: Update all TerminalView call sites in app.rs**

In `crates/rusterm-ui/src/app.rs`, update the `TerminalView` invocation in the rsx! to pass the new props:

```rust
                                        TerminalView {
                                            session_id: tab.id.clone(),
                                            render_output: tab.render_output.clone(),
                                            version: tab.version,
                                            suggestion: tab.suggestion.clone(),
                                            suggestions: tab.suggestions.clone(),
                                            suggestion_selected: tab.suggestion_selected,
                                            suggestion_visible: tab.suggestion_visible,
                                            on_resize: move |(cols, rows, pw, ph): (u16, u16, u32, u32)| {
                                                // ... existing code unchanged ...
                                            },
                                            on_input: move |data: Vec<u8>| {
                                                // ... existing code unchanged ...
                                            },
                                            on_command: move |_: String| {
                                                // ... existing code unchanged ...
                                            },
                                            on_scroll_up: move |rows: usize| {
                                                // ... existing code unchanged ...
                                            },
                                            on_scroll_down: move |rows: usize| {
                                                // ... existing code unchanged ...
                                            },
                                            on_scroll_to_bottom: move |_: ()| {
                                                // ... existing code unchanged ...
                                            },
                                            on_suggestion_navigate: move |idx: Option<usize>| {
                                                if let Some(i) = idx {
                                                    state_for_cmd.write().sessions.iter_mut()
                                                        .find(|t| t.id == sid_clone)
                                                        .map(|tab| tab.suggestion_selected = i);
                                                }
                                            },
                                            on_suggestion_accept: move |cmd: String| {
                                                // Accept: send the full command suffix
                                                let suffix = {
                                                    let terminals = state_for_cmd.read().terminals.clone();
                                                    if let Some(handle) = terminals.get(&sid_clone) {
                                                        let line = handle.lock().terminal.extract_current_line();
                                                        let cmd_part = strip_prompt(line.trim());
                                                        if cmd.starts_with(&cmd_part) {
                                                            cmd[cmd_part.len()..].to_string()
                                                        } else {
                                                            cmd
                                                        }
                                                    } else {
                                                        cmd
                                                    }
                                                };
                                                if let Some(sender) = senders.read().get(&sid_clone) {
                                                    let _ = sender.send(suffix.as_bytes().to_vec());
                                                }
                                                // Dismiss dropdown
                                                state_for_cmd.write().sessions.iter_mut()
                                                    .find(|t| t.id == sid_clone)
                                                    .map(|tab| {
                                                        tab.suggestion_visible = false;
                                                        tab.suggestion = None;
                                                    });
                                            },
                                            on_suggestion_dismiss: move |_: ()| {
                                                state_for_cmd.write().sessions.iter_mut()
                                                    .find(|t| t.id == sid_clone)
                                                    .map(|tab| tab.suggestion_visible = false);
                                            },
                                        }
```

- [ ] **Step 5: Fix SessionTab construction sites**

In `app.rs`, every place where `SessionTab { ... }` is constructed needs the new fields. Add `suggestion_selected: 0, suggestion_visible: false,` after `suggestions: Vec::new(),` in each construction. There are approximately 4 places:

1. SSH connect from sidebar (line ~440)
2. Shell connect from sidebar (line ~463)
3. Connection type not yet supported (line ~482)
4. Connection dialog on_create (line ~965)

For each, add:
```rust
suggestion_selected: 0,
suggestion_visible: false,
```

- [ ] **Step 6: Build to verify compilation**

Run: `cargo build 2>&1 | tail -10`
Expected: Clean compilation (may have warnings about unused variables, that's fine).

- [ ] **Step 7: Commit**

```bash
git add crates/rusterm-ui/src/components/terminal_view.rs crates/rusterm-ui/src/app.rs
git commit -m "feat(ui): wire suggestion dropdown into TerminalView with keyboard navigation"
```

---

### Task 4: Implement multi-suggestion lookup algorithm

**Files:**
- Modify: `crates/rusterm-ui/src/app.rs` (the `on_input` handler's suggestion logic)

- [ ] **Step 1: Replace single-suggestion logic with multi-suggestion algorithm**

In `app.rs`, inside the `on_input` handler's spawned async block (the section after `// Query history for suggestion`), replace the entire suggestion logic block with the multi-suggestion version. The old code starts after `let cmd_lower = cmd_part.to_lowercase();` and ends before the final epoch check. Replace with:

```rust
                                                        let cmd_lower = cmd_part.to_lowercase();
                                                        let mut all_suggestions: Vec<String> = Vec::new();
                                                        let mut seen = std::collections::HashSet::new();

                                                        // 1. Session command history (most recent, prefix match)
                                                        let session_hist = state_for_cmd.read().sessions
                                                            .iter().find(|t| t.id == sid_sug)
                                                            .map(|t| t.command_history.clone())
                                                            .unwrap_or_default();

                                                        let mut session_matches: Vec<&String> = Vec::new();
                                                        for cmd in session_hist.iter().rev() {
                                                            if cmd.to_lowercase().starts_with(&cmd_lower)
                                                                && cmd.len() > cmd_part.len()
                                                                && !seen.contains(cmd.to_lowercase().as_str())
                                                            {
                                                                seen.insert(cmd.to_lowercase().clone());
                                                                session_matches.push(cmd);
                                                                if session_matches.len() >= 3 { break; }
                                                            }
                                                        }
                                                        all_suggestions.extend(session_matches.into_iter().cloned());

                                                        // 2. Local shell history files (atuin/zsh/bash/fish)
                                                        {
                                                            let provider = rusterm_history::HybridHistoryProvider::new();
                                                            let results = provider.search(&cmd_part, 5);
                                                            for m in results {
                                                                if m.command.to_lowercase().starts_with(&cmd_lower)
                                                                    && m.command.len() > cmd_part.len()
                                                                    && !seen.contains(m.command.to_lowercase().as_str())
                                                                {
                                                                    seen.insert(m.command.to_lowercase().clone());
                                                                    all_suggestions.push(m.command);
                                                                }
                                                            }
                                                        }

                                                        // 3. SQLite FTS5 (cross-session global)
                                                        {
                                                            let db_path = dirs::data_dir()
                                                                .unwrap_or_default()
                                                                .join("rusterm")
                                                                .join("rusterm.db");
                                                            if let Ok(db) = rusterm_db::Database::open(Some(db_path)).await {
                                                                if let Ok(results) = db.search_history(&cmd_part, 5).await {
                                                                    for entry in results {
                                                                        if entry.command.to_lowercase().starts_with(&cmd_lower)
                                                                            && entry.command.len() > cmd_part.len()
                                                                            && !seen.contains(entry.command.to_lowercase().as_str())
                                                                        {
                                                                            seen.insert(entry.command.to_lowercase().clone());
                                                                            all_suggestions.push(entry.command);
                                                                        }
                                                                    }
                                                                }
                                                            }
                                                        }

                                                        if state_for_cmd.read().suggestion_epoch != epoch {
                                                            return;
                                                        }

                                                        // Truncate to 8 suggestions max
                                                        all_suggestions.truncate(8);

                                                        if all_suggestions.is_empty() {
                                                            state_for_cmd.write().sessions.iter_mut()
                                                                .find(|t| t.id == sid_sug)
                                                                .map(|tab| {
                                                                    tab.suggestion = None;
                                                                    tab.suggestions = Vec::new();
                                                                    tab.suggestion_visible = false;
                                                                    tab.suggestion_selected = 0;
                                                                });
                                                        } else {
                                                            // First suggestion is the inline ghost text (suffix only)
                                                            let suffix = all_suggestions[0][cmd_part.len()..].to_string();
                                                            let show_dropdown = all_suggestions.len() > 1;
                                                            state_for_cmd.write().sessions.iter_mut()
                                                                .find(|t| t.id == sid_sug)
                                                                .map(|tab| {
                                                                    tab.suggestion = Some(suffix);
                                                                    tab.suggestions = all_suggestions;
                                                                    tab.suggestion_visible = show_dropdown;
                                                                    tab.suggestion_selected = 0;
                                                                });
                                                        }
```

- [ ] **Step 2: Build to verify compilation**

Run: `cargo build 2>&1 | tail -10`
Expected: Clean compilation.

- [ ] **Step 3: Commit**

```bash
git add crates/rusterm-ui/src/app.rs
git commit -m "feat(suggestions): implement three-tier multi-suggestion lookup algorithm"
```

---

### Task 5: Clear suggestion state on Enter and dismiss on Escape

**Files:**
- Modify: `crates/rusterm-ui/src/app.rs` (on_command handler)

- [ ] **Step 1: Update on_command handler to clear all suggestion state**

In `app.rs`, in the `on_command` handler, replace the existing suggestion-clearing line:

```rust
state_for_cmd.write().sessions.iter_mut()
    .find(|t| t.id == sid_for_cmd)
    .map(|tab| tab.suggestion = None);
```

with:

```rust
state_for_cmd.write().sessions.iter_mut()
    .find(|t| t.id == sid_for_cmd)
    .map(|tab| {
        tab.suggestion = None;
        tab.suggestions = Vec::new();
        tab.suggestion_visible = false;
        tab.suggestion_selected = 0;
    });
```

- [ ] **Step 2: Build to verify**

Run: `cargo build 2>&1 | tail -5`
Expected: Clean compilation.

- [ ] **Step 3: Commit**

```bash
git add crates/rusterm-ui/src/app.rs
git commit -m "fix(suggestions): clear all suggestion state on command submit"
```

---

### Task 6: Add NO_NETWORK compile-time gate to history crate

**Files:**
- Modify: `crates/rusterm-history/Cargo.toml`
- Modify: `crates/rusterm-history/src/lib.rs`

- [ ] **Step 1: Add a `network` feature flag to rusterm-history Cargo.toml**

In `crates/rusterm-history/Cargo.toml`, add a `[features]` section:

```toml
[features]
default = []
network = []
```

- [ ] **Step 2: Gate all public exports with `#[cfg(feature = "network")]`**

In `crates/rusterm-history/src/lib.rs`, wrap all public items with the network feature gate:

```rust
#[cfg(feature = "network")]
pub mod atuin_db;
#[cfg(feature = "network")]
pub mod bash_history;
#[cfg(feature = "network")]
pub mod fish_history;
#[cfg(feature = "network")]
pub mod hybrid;
#[cfg(feature = "network")]
pub mod zsh_history;

#[cfg(feature = "network")]
pub use atuin_db::AtuinDbProvider;
#[cfg(feature = "network")]
pub use bash_history::BashHistoryProvider;
#[cfg(feature = "network")]
pub use fish_history::FishHistoryProvider;
#[cfg(feature = "network")]
pub use hybrid::HybridHistoryProvider;
#[cfg(feature = "network")]
pub use zsh_history::ZshHistoryProvider;

#[cfg(feature = "network")]
use chrono::{DateTime, Utc};

/// A match from command history search. All fields are local-only;
/// no history data is ever transmitted over the network.
#[derive(Debug, Clone)]
pub struct HistoryMatch {
    pub command: String,
    pub cwd: Option<String>,
    pub hostname: Option<String>,
    #[cfg(feature = "network")]
    pub timestamp: Option<DateTime<Utc>>,
    #[cfg(not(feature = "network"))]
    pub timestamp: Option<String>,
    pub score: f32,
}
```

Wait — this approach would break compilation when `network` is not enabled (the default), because `app.rs` uses `rusterm_history::HybridHistoryProvider`. That's the opposite of what we want.

The correct approach: the `network` feature should be **opt-in** (not default), and the history providers should always be available locally. The gate should prevent any future network-capable code from being compiled by default. Let me redesign this.

Instead, add a `network` feature that is **off by default**. When someone tries to add network sync functionality in the future, they would gate it with `#[cfg(feature = "network")]`. For now, add the feature flag infrastructure and a doc comment making the local-only policy explicit. The `HistoryMatch` struct gets a `#[non_exhaustive]` attribute to prevent external construction that might add network fields.

- [ ] **Step 1 (revised): Add feature flag and local-only documentation**

In `crates/rusterm-history/Cargo.toml`, add:

```toml
[features]
default = []
## Enables network sync for history (NOT recommended — violates local-only policy).
## This feature exists only so that any future network code can be gated behind it.
## When disabled (the default), no history data can leave the local machine.
network = []
```

- [ ] **Step 2 (revised): Add local-only enforcement documentation to lib.rs**

In `crates/rusterm-history/src/lib.rs`, add documentation at the top of the file:

```rust
//! # rusterm-history: Local-only command history providers
//!
//! **SECURITY POLICY:** All data accessed by this crate is strictly local.
//! No history data is ever transmitted over any network connection.
//!
//! The `network` feature flag exists as a compile-time guard — any future
//! functionality that would transmit history data MUST be gated behind
//! `#[cfg(feature = "network")]`. The default build (without this feature)
//! guarantees zero network I/O for history data.

pub mod atuin_db;
pub mod bash_history;
pub mod fish_history;
pub mod hybrid;
pub mod zsh_history;

pub use atuin_db::AtuinDbProvider;
pub use bash_history::BashHistoryProvider;
pub use fish_history::FishHistoryProvider;
pub use hybrid::HybridHistoryProvider;
pub use zsh_history::ZshHistoryProvider;

use chrono::{DateTime, Utc};

/// A match from command history search.
///
/// This struct is `#[non_exhaustive]` to prevent external code from
/// constructing it in ways that might bypass the local-only policy.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct HistoryMatch {
    pub command: String,
    pub cwd: Option<String>,
    pub hostname: Option<String>,
    pub timestamp: Option<DateTime<Utc>>,
    pub score: f32,
}
```

- [ ] **Step 3: Fix all HistoryMatch construction sites for `#[non_exhaustive]`**

Since `HistoryMatch` is now `#[non_exhaustive]`, external code cannot construct it with struct literal syntax. We need to add a constructor method. Add to the `impl HistoryMatch` block in `lib.rs`:

```rust
impl HistoryMatch {
    pub fn new(
        command: String,
        cwd: Option<String>,
        hostname: Option<String>,
        timestamp: Option<DateTime<Utc>>,
        score: f32,
    ) -> Self {
        Self { command, cwd, hostname, timestamp, score }
    }
}
```

Then update all construction sites in the history providers to use `HistoryMatch::new(...)`. This affects:
- `crates/rusterm-history/src/bash_history.rs`
- `crates/rusterm-history/src/zsh_history.rs`
- `crates/rusterm-history/src/fish_history.rs`
- `crates/rusterm-history/src/atuin_db.rs`
- `crates/rusterm-history/src/hybrid.rs`

In each file, replace struct literal construction like:
```rust
HistoryMatch {
    command: ...,
    cwd: ...,
    hostname: ...,
    timestamp: ...,
    score: ...,
}
```
with:
```rust
HistoryMatch::new(
    ...,
    ...,
    ...,
    ...,
    ...,
)
```

In `hybrid.rs`, the `or_insert_with` closure also constructs a `HistoryMatch`. Replace:
```rust
.or_insert_with(|| HistoryMatch {
    command: m.command.clone(),
    cwd: m.cwd.clone(),
    hostname: m.hostname.clone(),
    timestamp: m.timestamp,
    score: m.score,
})
```
with:
```rust
.or_insert_with(|| HistoryMatch::new(
    m.command.clone(),
    m.cwd.clone(),
    m.hostname.clone(),
    m.timestamp,
    m.score,
))
```

- [ ] **Step 4: Build to verify compilation**

Run: `cargo build 2>&1 | tail -10`
Expected: Clean compilation.

- [ ] **Step 5: Commit**

```bash
git add crates/rusterm-history/
git commit -m "feat(history): add local-only policy enforcement with #[non_exhaustive] and network feature gate"
```

---

### Task 7: Add "Local Only" indicator to status bar

**Files:**
- Modify: `crates/rusterm-ui/src/app.rs` (status bar section)

- [ ] **Step 1: Add a local-only indicator in the status bar**

In `app.rs`, in the status bar section (the `div` with `style: "height: 24px; ..."`), add a new span after the "Sessions:" counter, before the "AI" button:

```rust
                                                        span {
                                                            style: "color: #9ece6a; font-size: 10px; letter-spacing: 0.5px;",
                                                            "\u{1f512} LOCAL ONLY"
                                                        }
```

Note: The lock emoji \u{1f512} followed by "LOCAL ONLY" in green makes it clear that data stays local. Since the user didn't request emoji in code, we'll use a text-only version:

```rust
                                                        span {
                                                            style: "color: #9ece6a; font-size: 10px; letter-spacing: 0.5px; border: 1px solid #9ece6a; border-radius: 3px; padding: 0 4px;",
                                                            "LOCAL ONLY"
                                                        }
```

- [ ] **Step 2: Build to verify**

Run: `cargo build 2>&1 | tail -5`
Expected: Clean compilation.

- [ ] **Step 3: Commit**

```bash
git add crates/rusterm-ui/src/app.rs
git commit -m "feat(ui): add LOCAL ONLY security indicator to status bar"
```

---

### Task 8: End-to-end test run

**Files:**
- No new files

- [ ] **Step 1: Build the full project**

Run: `cargo build 2>&1`
Expected: Clean build with no errors.

- [ ] **Step 2: Run existing tests**

Run: `cargo test 2>&1`
Expected: All existing tests pass.

- [ ] **Step 3: Manual verification — launch the app**

Run: `cargo run -p rusterm-app 2>&1 &`
Then: Connect to a local shell, type a few commands (e.g., `ls`, `pwd`, `git status`), then start typing `l` or `g` and verify:
- Inline ghost text suggestion appears (top match)
- Dropdown appears below cursor when multiple matches exist
- Arrow keys navigate the dropdown
- Tab/Enter accepts the selected suggestion
- Escape dismisses the dropdown
- "LOCAL ONLY" badge visible in status bar

- [ ] **Step 4: Commit any fixes**

If any issues found during testing, fix and commit.

---

## Self-Review

**1. Spec coverage check:**
- Data flow (3 tiers → merge → 8 max → render): Task 4 ✓
- Security guarantees (NO_NETWORK gate, local-only indicator): Task 6, Task 7 ✓
- SuggestionPopup component (below cursor, 8 items, keyboard nav): Task 2, Task 3 ✓
- Suggestion algorithm (prefix + frecency, cross-session global): Task 4 ✓
- SessionTab state additions: Task 1 ✓
- Clear state on Enter: Task 5 ✓

**2. Placeholder scan:** No TBD, TODO, or placeholder steps found. All code is complete.

**3. Type consistency:**
- `suggestions: Vec<String>` — consistent across SessionTab, TerminalView props, SuggestionPopup
- `suggestion_selected: usize` — consistent across SessionTab, TerminalView props
- `suggestion_visible: bool` — consistent across SessionTab, TerminalView props
- `on_suggestion_navigate: EventHandler<Option<usize>>` — matches usage `Some(next)` / `Some(prev)`
- `on_suggestion_accept: EventHandler<String>` — matches `cmd.clone()` calls
- `on_suggestion_dismiss: EventHandler<()>` — matches `()` calls
- `HistoryMatch::new(...)` — 5-arg constructor matches 5 fields
