# Agent chat box (issue #122)

## What it is
A floating, draggable chat panel that lives bottom-left by default. Doubles as
a command palette (type `/` to fuzzy-search shell history + app commands).
Tab/Esc hands focus back to the terminal.

## Key files
- `crates/rusterm-core/src/config.rs` — `ChatSettings`, `AgentConfig`,
  `ChatAgentProvider`, `ChatPosition`, `KeybindingAction::ToggleChat` (+ the
  `Keybindings::toggle_chat` field, default Cmd/Ctrl+Shift+Space).
- `crates/rusterm-core/src/config_manager.rs` — `load_chat_settings` /
  `save_chat_settings` (read-modify-write pattern). The literal `PersistedConfig`
  constructors in every other `save_*` method carry `chat: existing.chat.clone()`.
- `crates/rusterm-ui/src/components/chat_panel.rs` — the panel component.
- `crates/rusterm-ui/src/state.rs` — runtime fields on `AppState`
  (`chat_visible`, `chat_settings`, `chat_messages`, `chat_input`,
  `chat_command_mode`, `chat_command_results`, `chat_command_selected`,
  `chat_drag_offset`, `chat_status`) + `ChatMessage`/`ChatRole`/
  `ChatCommandEntry`/`ChatCommandSource` types.
- `crates/rusterm-ui/src/app.rs` — `ToggleChat` arm in `run_keybinding_action`,
  plain-Cmd+Space special case in the global `onkeydown` (best-effort, since
  macOS Spotlight usually grabs it first), `<ChatPanel/>` rendered after
  `DockDragGhost`, and chat-settings load on unlock (next to skin/keybindings).

## Hotkey design
- **Configurable**: `KeybindingAction::ToggleChat`, default **Cmd+Shift+Space**
  (satisfies `is_safe_application_shortcut` = primary && shift, reachable on macOS).
- **Plain Cmd+Space**: special-cased in BOTH the `#main` `onkeydown` AND the
  terminal's `handle_keydown` (in `terminal_view.rs`) so it works whether or not
  a terminal has focus. Best-effort — macOS Spotlight intercepts it by default;
  the user must rebind Spotlight for it to reach the app.

## Drag mechanism
Mirrors the proven document-capture + ~60Hz polling pattern used by pane-move /
splitter-resize / tab-drag. Title bar `onmousedown` installs capture-phase
`mousemove`/`mouseup` listeners that write to `window.__rusterm_chat_drag_pos`;
a `use_future` polls and updates `chat_settings.position`. `mouseup` sets a done
flag → the loop cleans up the listeners and persists via `on_save_chat`.

## Hooks rule gotcha
`ChatPanel` calls ALL `use_signal`/`use_future` hooks BEFORE the
`if !visible { return rsx! {} }` early return. Returning before hooks would
desync the hook index across renders (Dioxus panics on hook-order mismatch).

## API key policy
API keys are NOT persisted in `settings.json` (matches the project's "never
persist secrets" policy). They're entered in the agent-config popover and held
in memory only. TODO: route through macOS Keychain like OneKey credentials.

## LLM round-trip
Currently STUBBED — `send_message` appends the user turn + a placeholder
assistant reply after 300ms. The real call should route through
`rusterm_ai::SuggestionEngine` (or a new chat-completions client) keyed off
the active agent's provider/model + the in-memory API key.

## Command palette
Typing `/` as the first char switches to command-search mode. Seeds the
dropdown synchronously with built-in app commands (zero-latency), then spawns
an async `rusterm_db::Database::search_history` query (frecency-ranked) and
merges results. Selecting a command inserts it into the active terminal's input
(NOT auto-run) and hands focus over.
