# OneKeys — ZOC-style Expect/Send Auto-Fill

**Date:** 2026-06-22
**Status:** Approved (design)

## Goal

Add a "OneKeys" credential auto-fill system (like ZOC terminal's OneKeys): a library of
encrypted Expect/Send entries. When terminal output matches an entry's `expect` regex
(e.g. git asking `Username for 'https://...':` or `password:`), a popup appears above the
current command line listing matching entries (different users/accounts). Selecting one
sends its `send` value + Enter. New entries can be saved from the popup or managed in a
dedicated dialog.

## Requirements (from user)

1. Configure OneKeys including git account/token/URL use cases (generic Expect/Send covers all).
2. Popup only works after the master password unlocks (send values are encrypted at rest).
3. Popup supports selecting among different users/accounts.
4. Popup sits above the current command line, does not obscure it.
5. Switching focus must NOT re-trigger / re-scan — the popup persists until dismissed.

## Decisions

- **Data model:** generic Expect/Send. Each OneKey = `{ id, name, expect (regex), send (secret) }`.
  `name` is the selectable label (e.g. `ecs-user`, `git-inesa`); `expect` is a regex matched
  against terminal output; `send` is the value to type (encrypted at rest).
- **Trigger:** automatic, only on **new** terminal output (`SessionEvent::Output`). Focus
  changes produce no new output → no re-scan → popup persists. (Satisfies req 5.)
- **Select action:** send `send` + `\n` (Enter) to the session, then dismiss the popup.
- **Save from popup:** "Save In OneKeys" saves a new OneKey with `expect` = the currently
  matched pattern and `send` = the terminal's current input line.
- **Position:** reuse the existing `--suggestion-bottom` CSS variable (kept current by the
  resize future every 100ms) so the popup sits just above the cursor row, not obscuring it.
- **Encryption:** `send` encrypted via the existing `ConfigManager` master key
  (`encrypt_data`/`decrypt_data`); stored as `EncryptedValue` in `settings.json`. Decrypted
  into memory only after unlock.

## Architecture

Reuses existing patterns: encrypted `PersistedConfig`, the `SessionEvent::Output` handler,
and the `SuggestionPopup` positioning approach.

### Data model & persistence (`rusterm-core::config`, `config_manager`)

```rust
// In-memory (plaintext send)
pub struct OneKey { pub id: String, pub name: String, pub expect: String, pub send: String }

// Persisted (send encrypted)
pub struct PersistedOneKey { pub id: String, pub name: String, pub expect: String, pub send: EncryptedValue }
```

- Add `onekeys: Vec<PersistedOneKey>` to `PersistedConfig` (`#[serde(default)]`).
- `ConfigManager`: `save_onekeys(&[OneKey])` (encrypt each `send`) and `load_onekeys() -> Vec<OneKey>` (decrypt).
- On unlock (`App`), load OneKeys into `AppState.onekeys: Vec<OneKey>`.

### AppState additions (`rusterm-ui::state`)

```rust
pub onekeys: Vec<OneKey>,                          // decrypted, in memory
pub onekey_popup: OneKeyPopupState,                // current popup state
// OneKeyPopupState { visible: bool, entries: Vec<OneKey>, matched_expect: Option<String> }
```
`#[serde(skip)]` on both (runtime-only).

### Matching engine (in `SessionEvent::Output` handler, `app.rs`)

After `process_and_render`, on the new `data` (as UTF-8 lossy):
- For each OneKey, compile its `expect` regex (cache compiled regexes) and `is_match`.
- Collect matching OneKeys.
- If matches non-empty **and** `onekey_popup.visible == false`: set
  `onekey_popup = { visible: true, entries: matches, matched_expect: Some(expect) }`.
  `matched_expect` = the `expect` of the first matching OneKey (in practice all matches for
  a given prompt share one `expect`, e.g. all `Username for \S+:` entries; this is used only
  by "Save In OneKeys" to prefill the new entry's expect).
- If popup already visible: leave it (persist; no re-trigger).
- (No matching on focus changes — output handler isn't called then.)

Dismissal: selecting an entry or Escape sets `visible = false`.

### Popup UI (`rusterm-ui::components::onekey_popup.rs`)

- `OneKeyPopup` component, `position:absolute; left:0; right:0; bottom: var(--suggestion-bottom, 2em)`,
  rendered inside the `#terminal-input` container (sibling of `#terminal-scroll`), so it sits
  above the cursor row without obscuring it.
- Lists `entries` (names); ↑↓ navigates, Enter/click sends `send`+`\n` via `on_select`.
- A `Save In OneKeys [+]` row calls `on_save` (saves current input line + matched_expect).
- Keyboard handling in `TerminalView`: when `onekey_popup.visible`, intercept ↑↓/Enter/Escape
  for the popup (like the suggestion popup), else fall through to the PTY.

### OneKey manager dialog (`rusterm-ui::components::onekey_manager.rs`)

- A modal (like `ConnectionDialog`): left list of OneKeys + right form (Name / Expect / Send).
- Add / edit / delete; saves via `ConfigManager::save_onekeys` and updates `AppState.onekeys`.
- Opened from a "OneKeys" button in the status bar (next to the existing "AI" button).

### Decryption / security

- `send` is encrypted at rest; decrypted only after master-password unlock (`config_manager`
  is `Some`). The popup shows `name` only, never the secret. Sending uses the in-memory
  plaintext. If locked, `AppState.onekeys` is empty → no popup.

## Phasing

- **Phase 1:** data model + persistence (`OneKey`, `PersistedOneKey`, `ConfigManager`
  save/load) + `OneKeyManager` dialog (add/edit/delete, encrypted). Testable: configure
  OneKeys, verify they persist encrypted.
- **Phase 2:** matching engine + `OneKeyPopup` (autofill on match, send+Enter, Save In
  OneKeys, focus persistence).

## Testing

- Unit: `ConfigManager` OneKey save/load round-trip (encrypt → decrypt equals plaintext);
  regex match cases (`Username for 'x':`, `password:`).
- Phase 1: add a OneKey via manager, restart, verify it loads (decrypted) and `send` is
  encrypted in `settings.json`.
- Phase 2 (manual, gated by master password): trigger a git/ssh prompt, verify popup
  appears above the command line, selecting sends value+Enter, focus switch doesn't
  re-trigger, Save creates a new OneKey.

## Out of scope (YAGNI)

- Per-host scoping of OneKeys (global list for now).
- Importing OneKeys from other password managers.
- TOTP / time-based secrets.
- Sharing OneKeys across machines.
