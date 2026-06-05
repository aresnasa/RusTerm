# RusTerm Theme System Design

## Goal

Replace all 157 hardcoded Tokyo Night color strings across 8 files with a configurable theme system. Support built-in preset themes, custom color editing, and full-app theming (terminal ANSI colors + UI chrome).

## Current State

- All colors are inline string literals (e.g. `#1a1b26`, `#7aa2f7`) scattered across 8 source files
- A dead `Theme` enum exists in `state.rs` with `Dark`/`Light` variants but is never read
- `PersistedConfig` has no appearance/theme fields
- `named_color_hex()` and `indexed_color_hex()` hardcode Tokyo Night ANSI palette

## Approach: CSS Variables + Theme Struct

### Theme Struct (`rusterm-core/src/theme.rs`)

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Theme {
    pub name: String,

    // Core UI (8 fields)
    pub background: String,       // --bg
    pub surface: String,          // --surface
    pub surface_hover: String,    // --surface-hover
    pub border: String,           // --border
    pub foreground: String,       // --fg
    pub muted: String,            // --muted
    pub accent: String,           // --accent
    pub error: String,            // --error

    // Terminal ANSI 16 colors
    pub ansi_black: String,            // --ansi-0
    pub ansi_red: String,              // --ansi-1
    pub ansi_green: String,            // --ansi-2
    pub ansi_yellow: String,           // --ansi-3
    pub ansi_blue: String,             // --ansi-4
    pub ansi_magenta: String,          // --ansi-5
    pub ansi_cyan: String,             // --ansi-6
    pub ansi_white: String,            // --ansi-7
    pub ansi_bright_black: String,     // --ansi-8
    pub ansi_bright_red: String,       // --ansi-9
    pub ansi_bright_green: String,     // --ansi-10
    pub ansi_bright_yellow: String,    // --ansi-11
    pub ansi_bright_blue: String,      // --ansi-12
    pub ansi_bright_magenta: String,   // --ansi-13
    pub ansi_bright_cyan: String,      // --ansi-14
    pub ansi_bright_white: String,     // --ansi-15

    // Cursor + selection
    pub cursor: String,           // --cursor
    pub selection: String,        // --selection (includes alpha, e.g. rgba(...))
}
```

Total: 28 color fields + name.

### Built-in Presets

Functions returning `Theme` instances:
- `tokyo_night()` — current default
- `dracula()` — Dracula theme
- `solarized_light()` — Solarized Light (light background)
- `catppuccin_mocha()` — Catppuccin Mocha
- `one_dark()` — One Dark

Lookup by name: `Theme::preset(name: &str) -> Option<Theme>`.

### CSS Variable Injection

On theme change (startup or user action), inject all 28 colors as CSS custom properties via Dioxus `document::eval()`:

```js
const root = document.documentElement.style;
root.setProperty('--bg', theme.background);
root.setProperty('--surface', theme.surface);
// ... all 28 properties
```

CSS variable names: `--bg`, `--surface`, `--surface-hover`, `--border`, `--fg`, `--muted`, `--accent`, `--error`, `--ansi-0` through `--ansi-15`, `--cursor`, `--selection`.

### Inline Style Migration

Every hardcoded color in inline `style=""` attributes is replaced with `var(--name)`:

- `background: #1a1b26` -> `background: var(--bg)`
- `color: #c0caf5` -> `color: var(--fg)`
- `background: #24283b` -> `background: var(--surface)`
- `border: 1px solid #2a2b3d` -> `border: 1px solid var(--border)`
- etc.

Files to migrate (157 occurrences):
1. `terminal_view.rs` (55)
2. `connection_dialog.rs` (29)
3. `sidebar.rs` (25)
4. `master_password_dialog.rs` (18)
5. `tab_bar.rs` (10)
6. `app.rs` (8)
7. `ai_panel.rs` (8)
8. `main.rs` (4)

### Terminal ANSI Color Mapping

`named_color_hex()` and `indexed_color_hex()` in `terminal_view.rs` are replaced with functions that read from the `Theme` struct:

```rust
fn named_color_hex(nc: NamedColor, theme: &Theme) -> String {
    match nc {
        NamedColor::Black => theme.ansi_black.clone(),
        NamedColor::Red => theme.ansi_red.clone(),
        // ...
        NamedColor::Foreground => theme.foreground.clone(),
        NamedColor::Background => theme.background.clone(),
        NamedColor::Cursor => theme.cursor.clone(),
        _ => theme.foreground.clone(),
    }
}
```

`indexed_color_hex(idx, theme)` uses `named_color_hex` for 0-15 and computes the 216-color cube + grayscale ramp (theme-independent).

### Persistence

Extend `PersistedConfig`:
```rust
pub struct PersistedConfig {
    pub version: u32,
    pub connections: Vec<PersistedConnection>,
    pub master_password_hash: Option<String>,
    pub active_theme: String,           // NEW: theme name
    pub custom_themes: Vec<Theme>,      // NEW: user-created themes
}
```

On load: resolve `active_theme` from built-in presets first, then `custom_themes`, fallback to `tokyo_night()`.
On save: persist `active_theme` name and `custom_themes`. Built-in presets are never persisted.

Migration: existing `settings.json` without theme fields defaults to Tokyo Night.

### UI — Sidebar Theme Panel

New section in the sidebar (below connections):

1. **Theme selector**: Dropdown listing all built-in presets + custom themes.
2. **Color editor**: When user clicks "Edit" or selects "Custom...", a scrollable grid of 28 color swatches. Each opens `<input type="color">` on click.
3. **Save as Custom**: Save current edits as a new named custom theme.
4. **Reset**: Revert to preset defaults or delete custom theme.

Layout: Compact grid, 4 columns, labeled groups (Core UI, ANSI Colors, Cursor/Selection).

### Data Flow

```
User selects theme in sidebar
  -> AppState.theme updated
  -> CSS variables injected into :root via document::eval()
  -> All var(--xxx) references update immediately (browser re-renders)
  -> TerminalView reads theme for ANSI color mapping
  -> save_config() persists active_theme + custom_themes
```

### Module Structure

New file: `rusterm-core/src/theme.rs`
- `Theme` struct with Serialize/Deserialize
- Built-in preset functions
- `Theme::preset(name)` lookup
- `Theme::css_variables_js()` method returning JS injection string
- `Theme::ansi_color(index)` method for terminal rendering

Modified files:
- `rusterm-core/src/config.rs` — add `active_theme`, `custom_themes` to `PersistedConfig`
- `rusterm-core/src/lib.rs` — export `theme` module
- `rusterm-ui/src/state.rs` — replace dead `Theme` enum with `rusterm_core::theme::Theme`
- `rusterm-ui/src/app.rs` — theme change handler, CSS var injection, save theme
- `rusterm-ui/src/components/terminal_view.rs` — theme-based ANSI mapping, var(--xxx) for styles
- `rusterm-ui/src/components/sidebar.rs` — theme panel UI, var(--xxx) for styles
- `rusterm-ui/src/components/connection_dialog.rs` — var(--xxx) for styles
- `rusterm-ui/src/components/master_password_dialog.rs` — var(--xxx) for styles
- `rusterm-ui/src/components/tab_bar.rs` — var(--xxx) for styles
- `rusterm-ui/src/components/ai_panel.rs` — var(--xxx) for styles
- `rusterm-app/src/main.rs` — window background, global CSS with var() references

### Native Window Background

`main.rs` sets `.with_background_color((26, 27, 38, 255))`. This can't use CSS variables. Workaround: parse theme's `background` hex and update window config on theme change. May flash briefly on startup before theme loads.

### Testing

- Unit tests for `Theme` preset functions (verify all fields are valid hex)
- Unit test for `Theme::ansi_color(index)` for all 256 indices
- Unit test for CSS variable injection string format
- Manual test: switch themes and verify all UI elements update
- Manual test: custom theme creation, editing, saving, loading
- Manual test: app restart preserves selected theme
