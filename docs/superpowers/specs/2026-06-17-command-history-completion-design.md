# Command History Completion Design

## Overview

Add dropdown-based command completion to RusTerm, powered by the existing three-tier history system. All data stays strictly local — no network transmission, no cloud sync, no telemetry.

## Data Flow

1. User types → 200ms debounce → `extract_current_line()` + `strip_prompt()` gets input
2. Three-tier lookup (memory → shell files → SQLite FTS5), take top 8
3. Populate `tab.suggestions: Vec<String>` + `tab.suggestion: Option<String>` (first match)
4. Render: inline ghost text + dropdown popup

## Security Guarantees

- All data stored only in local SQLite (`~/.rusterm/history.db`) and in-memory `command_history`
- `NO_NETWORK` compile-time gate: history crate public functions gated behind `#[cfg(feature = "network")]` (disabled by default), ensuring no network path can access history
- Settings page shows a non-dismissible "Local Only" indicator
- Audit all `reqwest`/`hyper`/`tokio::net` dependencies to confirm no network calls in history code paths

## SuggestionPopup Component

**Render position**: Inside TerminalView, below cursor row, absolute-positioned overlay on terminal content.

**Visual design**:
- Width: widest suggestion text + padding, min 300px, max terminal width
- Height: up to 8 items, 24px per row
- Style: dark background (`#1a1b26`), highlighted selection (`#283457`), text `#c0caf5`
- Selected index `selected_index: usize`, defaults to 0

**Keyboard interactions**:
- `↓` / `Ctrl+N`: move selection down (wrap)
- `↑` / `Ctrl+P`: move selection up (wrap)
- `Tab` / `Enter`: accept selected suggestion (complete to cursor position)
- `Esc` / `Ctrl+G`: dismiss dropdown, keep inline ghost text
- Continued typing: live filter, reset selected_index to 0

**Mount**: In TerminalView's `rsx!`, render SuggestionPopup when `suggestions.len() > 1`, passing suggestions, selected_index, on_select callback.

## Suggestion Algorithm

### Three-tier lookup

1. **Tier 1 — In-memory `command_history`**: prefix match, sorted by recent use (reverse iteration), top 3
2. **Tier 2 — `HybridHistoryProvider`**: cross-shell history files (bash/zsh/fish/atuin), frecency scoring, top 5
3. **Tier 3 — `rusterm-db` FTS5**: SQLite full-text search + frecency (frequency + recency + success rate), top 5

### Merge and dedup

- Dedup by command text, keep highest score
- Sort descending by score
- Cap at 8 total
- First entry also set as `tab.suggestion` (inline ghost text)

### Cross-session global

All sessions share the SQLite history. On session start, load global history from DB into `command_history`.

### Recording trigger

On Enter: write to `tab.command_history` + SQLite. Skip empty or whitespace-only commands.

## State Changes

### SessionTab additions

```rust
pub struct SessionTab {
    // ... existing fields ...
    pub suggestion: Option<String>,       // inline ghost text (existing)
    pub suggestions: Vec<String>,         // dropdown candidates (existing, currently unused)
    pub suggestion_selected: usize,        // NEW: dropdown selected index
    pub suggestion_visible: bool,          // NEW: dropdown visibility
}
```

### AppState additions

None needed — the existing `suggestion_epoch` debounce counter is sufficient.

## Files Modified

| File | Change |
|------|--------|
| `crates/rusterm-ui/src/components/terminal_view.rs` | Add SuggestionPopup rendering, keyboard handling for up/down/tab/esc in dropdown |
| `crates/rusterm-ui/src/state.rs` | Add `suggestion_selected` and `suggestion_visible` to SessionTab |
| `crates/rusterm-ui/src/app.rs` | Populate `suggestions` vec, merge/dedup logic, global history loading on session start |
| `crates/rusterm-history/src/lib.rs` | Add `#[cfg(feature = "network")]` gates (off by default) |
| `crates/rusterm-ui/src/components/settings_view.rs` | Add "Local Only" indicator |

## New Files

| File | Purpose |
|------|---------|
| `crates/rusterm-ui/src/components/suggestion_popup.rs` | SuggestionPopup component |

## Out of Scope

- Fuzzy matching (prefix-only per design decision)
- AI-powered suggestions (handled by rusterm-ai separately)
- Cloud sync or remote history storage
- History encryption at rest (future consideration)
