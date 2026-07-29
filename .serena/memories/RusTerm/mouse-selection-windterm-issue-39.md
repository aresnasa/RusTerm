# RusTerm — Mouse selection & double-click word select (issue #39/#40)

WindTerm-style terminal mouse features in RusTerm (Dioxus 0.7.9 desktop, macOS
wry/WebKit). Implements items #39/#40 in `.claude/Claude.md`.

## What works
- **Mouse drag-select → copy** (selecting text with left-button drag copies on
  release, WindTerm default copy-on-select).
- **Double-click word select → copy** (double-click selects the word under the
  cursor and copies it immediately).
- **Middle-click paste** (xterm convention; OS clipboard carries the selection).
- **Right-click paste** when no app mouse reporting and not disconnected;
  right-click reconnect when disconnected.
- **Shift override**: holding Shift forces local selection even when an
  application has enabled mouse reporting (vim `set mouse=a`, tmux, htop).
- **App mouse reporting** (xterm modes 1000/1002/1003/1006) is forwarded to
  the PTY when the app owns the mouse.

## Key files
- `crates/rusterm-ui/src/components/terminal_view.rs` — all mouse handlers,
  selection state, the pure helpers `event_cell_from_coords` and
  `word_range_in_row`, and the unit tests.
- `crates/rusterm-core/src/terminal.rs` — `extract_selection`,
  `RenderCell`/`RenderRow`, `encode_mouse_report`.

## Architecture
- **Terminal-owned selection** (cell-coordinate based), NOT native DOM
  selection. The DOM selection can't distinguish wide-char boundary cells,
  dragged-overline raggedness across the monospace grid, or line-wrap joins.
  The content div uses `user-select:none` so the two never compete.
- `TextSelection { anchor: (row, col), head: (row, col) }` — cell coords into
  `render_output.rows` at the moment the drag/double-click happened.
- `selection: Signal<Option<TextSelection>>`, `selection_text: Signal<String>`,
  `selecting: Signal<bool>` (true only during an active drag).
- `extract_selection(rows, anchor, head)` normalizes endpoints, trims trailing
  blanks on unwrapped rows, joins wrapped rows without `\n`, skips
  `wide_next` cells so CJK glyphs emit once.
- Copy-on-select uses `copy_text_to_clipboard(text)` (async, spawned).

## Double-click word select — implementation
- `on_double_click` handler on the content div (same div as mousedown/move/up).
- Uses `event_cell(&e)` to get `(row, col)`, then
  `word_range_in_row(&row.cells, col)` → inclusive `(start, end)`.
- Sets `selection` to `{ anchor: (row, start), head: (row, end) }`,
  `selecting = false` (no drag follows unless a fresh mousedown starts),
  extracts text, sets `selection_text`, spawns `copy_text_to_clipboard`.
- When app mouse reporting is on AND Shift not held: forwards a press report
  (xterm sends the 2nd click of a double-click as another press) and lets the
  app do its own word selection. Shift forces local word select.

### `word_range_in_row(cells, col)` — pure helper
- Returns inclusive `(start_col, end_col)` of the maximal same-char-class run
  containing `col`.
- Three xterm-style char classes:
  - `Word`: `[A-Za-z0-9_]` (identifier chars, underscore included).
  - `Punct`: any other non-whitespace char.
  - `Space`: whitespace.
- Wide-char continuation cells (`wide_next == true`) inherit the class of
  their parent wide cell (cell to the left with `wide == true`), so the run
  doesn't fracture across a wide glyph's two columns and a double-click on
  either half selects the whole glyph.
- 9 unit tests covering: word run, punctuation run, whitespace run, row
  boundaries, out-of-range col clamping, empty row, underscore as word char,
  wide-char non-fracture, adjacent same-class wide chars extending.

## Critical implementation details (don't re-discover)
1. **NEVER use `element_coordinates()` for hit-testing** on dioxus desktop.
   It returns DOM `offsetX/offsetY` relative to the event TARGET, and terminal
   rows are raw HTML (`dangerous_inner_html`) so the target moves between
   spans/row-divs during a drag → garbage offsets. **Fix:** use
   `client_coordinates()` minus the live `getBoundingClientRect()` origin of
   the content div, polled every 500ms into `content_geo:
   Signal<Option<(left, top, cw, ch)>>`.
2. **`dioxus::document::eval` scripts must `return` their value explicitly.**
   Desktop wraps the script in `new AsyncFunction("dioxus", script)` and
   awaits the result. An IIFE expression statement like
   `"(function(){... return x; })()"` resolves to `undefined` — the inner
   return is swallowed. All value-returning evals are written as
   AsyncFunction bodies with top-level `return`. Promises ARE awaited, so
   `return navigator.clipboard.writeText(t).then(...)` and
   `return navigator.clipboard.readText()` work correctly.
3. **macOS wry: `prevent_default()` on mousedown cancels the subsequent
   click/focus** — don't call it in selection mousedown/dblclick handlers.
4. **`PrivateMode::Unknown(u16)` is NOT used for mouse modes in vte 0.15** —
   1000/1002/1003/1006 arrive as `PrivateMode::Named(ReportMouseClicks |
   ReportCellMouseMotion | ReportAllMouseMotion | SgrMouse)`.
5. `Signal<Option<T>>.take()` works in dioxus 0.7.
6. Bubbling mouse events DO fire through `dangerous_inner_html` children
   (root-delegation via `getTargetId` walks `data-dioxus-id` up the real DOM).
7. Closures passed to dioxus event handlers must capture only `Copy` types
   (Signal, f64, usize). Cache `render_output`-derived scalars in locals
   before the closure; don't borrow `render_output` (it's a moved prop).
8. In dioxus 0.7 the event handler attribute is `ondoubleclick` (NOT
   `ondblclick` — the latter is deprecated and emits a warning).

## Verified facts about this wry/WebKit env
- `navigator.clipboard` exists, `isSecureContext === true`, protocol is
  `dioxus:` (treated as secure).
- `navigator.clipboard.writeText` works and lands in the OS pasteboard
  (`pbpaste` confirms).
- `document.execCommand('copy')` also works (fallback available).
- Row height = **19.00px** (line-height:1.5 × font-size:13px); cell width ≈
  **7.80px** (JetBrains Mono). Row HTML is
  `<div style="white-space:pre;line-height:1.5;...">`.

## Tests
- `cargo test --workspace` — 629+ tests, all green.
- 9 new unit tests for `word_range_in_row` in `terminal_view::tests`.

## Debugging wry UI bugs
A fully agent-runnable feedback loop IS possible for wry UI bugs: env-gate a
debug hook (`RUSTERM_SEL_DEBUG=1`) that (a) bypasses the master-password gate
in `app.rs` and opens a local terminal, and (b) in `terminal_view.rs`
`use_effect` dispatches synthetic `MouseEvent`s via
`content.dispatchEvent(new MouseEvent(type, {bubbles:true, clientX, clientY, ...}))`
on the content div, then logs results. Synthetic events trigger the same
dioxus root-listener path as real ones. This was removed but can be re-added
if more interactive debugging is needed.

## Known limitations
- CJK word segmentation: all non-ASCII chars get class `Punct`, so adjacent
  CJK glyphs of the same class extend into one selection (e.g. double-clicking
  one CJK char may select a run of adjacent CJK chars). True CJK word
  segmentation is a harder problem WindTerm doesn't fully solve either; the
  char-class approach matches xterm/IDE behaviour for ASCII identifiers,
  paths, and URLs.
- Triple-click (select whole line) is NOT implemented (WindTerm does this;
  optional future enhancement).
