use dioxus::prelude::*;
// `MouseButton` lives in `dioxus::html::input_data` (not re-exported by
// dioxus::prelude) — same import pattern as tab_bar.rs.
use dioxus::html::input_data::MouseButton;

use rusterm_core::terminal::{
    CellColor, CellFlags, MouseReportKind, RenderCell, RenderOutput, RenderRow,
    encode_mouse_report, extract_selection,
};

use crate::components::OneKeyPopup;
use crate::components::SuggestionPopup;
use crate::state::{OneKeyMatch, OneKeySubmissionFeedback};

// ── Clipboard helpers ───────────────────────────────────────────────
//
// All clipboard I/O goes through `arboard` (native OS pasteboard) rather than
// `navigator.clipboard`. WKWebView's async Clipboard API (`readText`/
// `writeText`) silently rejects with NotAllowedError outside a real browser
// context — the dioxus `dioxus:` protocol is a secure context, but WKWebView
// still gates `readText` behind a permission the host app can't grant, so
// paste was swallowing the error and doing nothing. Native access has no such
// restriction and works on every platform.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ClipboardCopyOutcome {
    Copied(usize),
    SkippedEmpty,
    Failed,
}

/// Copy non-empty text to the OS clipboard. Synchronous (NSPasteboard / Win32
/// / X11 writes are sub-millisecond). Empty input is rejected so a missed
/// terminal mouseup can never erase the user's existing clipboard contents.
fn copy_text_to_clipboard(text: String) -> ClipboardCopyOutcome {
    if text.is_empty() {
        tracing::debug!("[COPY] refused to replace clipboard with empty text");
        return ClipboardCopyOutcome::SkippedEmpty;
    }

    let n = text.chars().count();
    match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(text)) {
        Ok(()) => ClipboardCopyOutcome::Copied(n),
        Err(e) => {
            tracing::info!("[COPY] native set_text failed: {e} chars={n}");
            ClipboardCopyOutcome::Failed
        }
    }
}

/// Read the OS clipboard and send it to the PTY (with bracketed-paste
/// wrapping when the application asked for it, mode 2004).
fn paste_from_clipboard(on_input: &EventHandler<Vec<u8>>, bracketed: bool) {
    let text = match arboard::Clipboard::new().and_then(|mut cb| cb.get_text()) {
        Ok(t) => t,
        Err(arboard::Error::ContentNotAvailable) => {
            tracing::debug!("[PASTE] clipboard has no text");
            return;
        }
        Err(e) => {
            tracing::info!("[PASTE] native get_text failed: {e}");
            return;
        }
    };
    if text.is_empty() {
        return;
    }
    let data = if bracketed {
        let mut buf = Vec::with_capacity(text.len() + 12);
        buf.extend_from_slice(b"\x1b[200~");
        buf.extend_from_slice(text.as_bytes());
        buf.extend_from_slice(b"\x1b[201~");
        buf
    } else {
        text.into_bytes()
    };
    on_input.call(data);
}

// ── Mouse selection ──────────────────────────────────────────────────

/// A mouse-drag text selection over the rendered rows. Coordinates are
/// (row, col) cell indices into `render_output.rows` at the moment the
/// drag happened; `text` is captured at mouseup so Ctrl+Shift+C and Cmd+C
/// can re-copy without re-hit-testing a possibly-shifted buffer.
#[derive(Clone, Copy)]
struct TextSelection {
    anchor: (usize, usize),
    head: (usize, usize),
}

/// Resolve the current terminal-owned selection to clipboard text.
///
/// The cached text is normally populated on mouseup. Keeping this fallback at
/// the copy boundary also covers a drag released outside the terminal before
/// its mouseup handler can update that cache.
fn terminal_selection_text(
    cached: &str,
    selection: Option<TextSelection>,
    rows: &[RenderRow],
) -> String {
    if !cached.is_empty() {
        return cached.to_owned();
    }

    let Some(selection) = selection else {
        return String::new();
    };
    if selection.anchor == selection.head {
        return String::new();
    }

    extract_selection(rows, selection.anchor, selection.head)
}

// ── Terminal key encoding helpers ──────────────────────────────────

fn csi_seq(param: u8, modifier: Option<u8>, final_byte: u8) -> Vec<u8> {
    let mut buf = vec![0x1b, 0x5b];
    buf.extend_from_slice(param.to_string().as_bytes());
    if let Some(m) = modifier {
        buf.push(b';');
        buf.extend_from_slice(m.to_string().as_bytes());
    }
    buf.push(final_byte);
    buf
}

fn cursor_key_seq(param: u8, final_byte: u8, app_cursor: bool, modifier: Option<u8>) -> Vec<u8> {
    if modifier.is_some() {
        csi_seq(param, modifier, final_byte)
    } else if app_cursor {
        vec![0x1b, 0x4f, final_byte]
    } else {
        csi_seq(param, None, final_byte)
    }
}

fn ctrl_char(s: &str) -> Vec<u8> {
    match s.to_lowercase().as_str() {
        "a" => vec![0x01],
        "b" => vec![0x02],
        "c" => vec![0x03],
        "d" => vec![0x04],
        "e" => vec![0x05],
        "f" => vec![0x06],
        "g" => vec![0x07],
        "h" => vec![0x08],
        "i" => vec![0x09],
        "j" => vec![0x0a],
        "k" => vec![0x0b],
        "l" => vec![0x0c],
        "m" => vec![0x0d],
        "n" => vec![0x0e],
        "o" => vec![0x0f],
        "p" => vec![0x10],
        "q" => vec![0x11],
        "r" => vec![0x12],
        "s" => vec![0x13],
        "t" => vec![0x14],
        "u" => vec![0x15],
        "v" => vec![0x16],
        "w" => vec![0x17],
        "x" => vec![0x18],
        "y" => vec![0x19],
        "z" => vec![0x1a],
        "[" => vec![0x1b],
        "\\" => vec![0x1c],
        "]" => vec![0x1d],
        "^" => vec![0x1e],
        "_" => vec![0x1f],
        "2" | "@" => vec![0x00],
        "3" => vec![0x1b],
        "4" => vec![0x1c],
        "5" => vec![0x1d],
        "6" => vec![0x1e],
        "7" | "/" => vec![0x1f],
        "8" => vec![0x7f],
        " " => vec![0x00],
        _ => vec![],
    }
}

fn code_to_char(code: &Code) -> u8 {
    match code {
        Code::Digit0 => b'0',
        Code::Digit1 => b'1',
        Code::Digit2 => b'2',
        Code::Digit3 => b'3',
        Code::Digit4 => b'4',
        Code::Digit5 => b'5',
        Code::Digit6 => b'6',
        Code::Digit7 => b'7',
        Code::Digit8 => b'8',
        Code::Digit9 => b'9',
        Code::Minus => b'-',
        Code::Equal => b'=',
        Code::BracketLeft => b'[',
        Code::BracketRight => b']',
        Code::Backslash => b'\\',
        Code::Semicolon => b';',
        Code::Quote => b'\'',
        Code::Backquote => b'`',
        Code::Comma => b',',
        Code::Period => b'.',
        Code::Slash => b'/',
        _ => 0,
    }
}

/// What the OneKey autofill popup should do with a key while it is visible.
/// Extracted as a pure function so the routing — especially "typing dismisses
/// the popup and falls through to the PTY" — is unit-testable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OneKeyKeyAction {
    /// Move the selection cursor to the given index.
    Navigate(usize),
    /// Send the selected entry's value + Enter (autofill).
    Select,
    /// Close the popup without sending anything (Escape).
    Dismiss,
    /// Close the popup AND forward the key to the PTY. The user started typing
    /// (or editing) manually — the popup must not stay open and hijack the next
    /// Enter, otherwise the typed text is concatenated with the popup's saved
    /// value and the credential is sent mangled.
    DismissAndForward,
}

/// Decide what the OneKey popup does for `key` while visible (`selected` is the
/// current index, `len` the number of matching entries).
fn onekey_popup_key_action(key: &Key, selected: usize, len: usize) -> OneKeyKeyAction {
    if len == 0 {
        return OneKeyKeyAction::Dismiss;
    }
    let selected = selected.min(len - 1);
    match key {
        Key::ArrowDown => OneKeyKeyAction::Navigate((selected + 1) % len),
        Key::ArrowUp => {
            OneKeyKeyAction::Navigate(if selected == 0 { len - 1 } else { selected - 1 })
        }
        // Both Enter and Tab confirm the highlighted credential. The caller
        // returns immediately after Select, so the Enter used for confirmation
        // cannot fall through and reach the PTY a second time.
        Key::Enter | Key::Tab => OneKeyKeyAction::Select,
        Key::Escape => OneKeyKeyAction::Dismiss,
        _ => OneKeyKeyAction::DismissAndForward,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CopyShortcut {
    Command,
    CtrlShift,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TerminalOverlayKeyAction {
    Copy(CopyShortcut),
    OneKey(OneKeyKeyAction),
    None,
}

/// Resolve copy shortcuts before popup keyboard handling. This keeps a popup in
/// one terminal pane from dismissing itself or forwarding bytes when the user
/// copies that pane's existing selection.
fn terminal_overlay_key_action(
    key: &Key,
    ctrl: bool,
    alt: bool,
    meta: bool,
    shift: bool,
    onekey_visible: bool,
    onekey_selected: usize,
    onekey_len: usize,
) -> TerminalOverlayKeyAction {
    let is_c = matches!(key, Key::Character(s) if s.eq_ignore_ascii_case("c"));
    if is_c && meta && !ctrl && !alt {
        return TerminalOverlayKeyAction::Copy(CopyShortcut::Command);
    }
    if is_c && ctrl && shift && !alt && !meta {
        return TerminalOverlayKeyAction::Copy(CopyShortcut::CtrlShift);
    }
    if onekey_visible && onekey_len > 0 {
        return TerminalOverlayKeyAction::OneKey(onekey_popup_key_action(
            key,
            onekey_selected,
            onekey_len,
        ));
    }
    TerminalOverlayKeyAction::None
}

/// Geometry of the scroll-position indicator thumb: `(visible, top_pct, height_pct)`.
/// `scroll_total` = scrollback lines, `scroll_offset` = current scroll (0 = at
/// the bottom), `visible_rows` = grid rows shown. The thumb rests at the bottom
/// when at the bottom and rises proportionally as you scroll up. Extracted as a
/// pure function so the math is unit-testable.
fn scroll_thumb_geometry(
    scroll_total: usize,
    scroll_offset: usize,
    visible_rows: usize,
) -> (bool, f64, f64) {
    if scroll_total == 0 {
        return (false, 0.0, 100.0);
    }
    let total_content = scroll_total + visible_rows;
    let height = ((visible_rows as f64 / total_content as f64) * 100.0).max(5.0);
    let top = (((scroll_total - scroll_offset) as f64 / total_content as f64) * 100.0)
        .min(100.0 - height)
        .max(0.0);
    (true, top, height)
}

// ── Color mapping (Tokyo Night theme) ──────────────────────────────

fn color_to_css(color: &CellColor) -> String {
    match color {
        CellColor::Default => String::new(),
        CellColor::Named(nc) => named_color_hex(*nc).to_string(),
        CellColor::Indexed(idx) => indexed_color_hex(*idx),
        CellColor::Spec(rgb) => format!("#{:02x}{:02x}{:02x}", rgb.r, rgb.g, rgb.b),
    }
}

fn named_color_hex(nc: vte::ansi::NamedColor) -> &'static str {
    match nc {
        vte::ansi::NamedColor::Black => "#414868",
        vte::ansi::NamedColor::Red => "#f7768e",
        vte::ansi::NamedColor::Green => "#9ece6a",
        vte::ansi::NamedColor::Yellow => "#e0af68",
        vte::ansi::NamedColor::Blue => "#7aa2f7",
        vte::ansi::NamedColor::Magenta => "#bb9af7",
        vte::ansi::NamedColor::Cyan => "#7dcfff",
        vte::ansi::NamedColor::White => "#c0caf5",
        // Bright variants are intentionally LIGHTER than their normal
        // counterparts so bold text (which typically uses the bright color)
        // pops on the dark background. Previously bright == normal, which
        // made bold text indistinguishable from regular text — the root
        // cause of the "颜色太暗" (colors too dim) report.
        vte::ansi::NamedColor::BrightBlack => "#7c89a3",
        vte::ansi::NamedColor::BrightRed => "#ff9eb3",
        vte::ansi::NamedColor::BrightGreen => "#c3f08c",
        vte::ansi::NamedColor::BrightYellow => "#ffd28a",
        vte::ansi::NamedColor::BrightBlue => "#a9c2ff",
        vte::ansi::NamedColor::BrightMagenta => "#d4b8ff",
        vte::ansi::NamedColor::BrightCyan => "#a3e5ff",
        vte::ansi::NamedColor::BrightWhite => "#e8edff",
        vte::ansi::NamedColor::Foreground => "#c0caf5",
        vte::ansi::NamedColor::Background => "#1a1b26",
        vte::ansi::NamedColor::Cursor => "#c0caf5",
        _ => "#c0caf5",
    }
}

fn indexed_color_hex(idx: u8) -> String {
    if idx < 16 {
        match idx {
            0 => "#414868",
            1 => "#f7768e",
            2 => "#9ece6a",
            3 => "#e0af68",
            4 => "#7aa2f7",
            5 => "#bb9af7",
            6 => "#7dcfff",
            7 => "#c0caf5",
            // 8-15 are the bright variants — must match `named_color_hex`
            // so bold text is lighter, not identical to normal text.
            8 => "#7c89a3",
            9 => "#ff9eb3",
            10 => "#c3f08c",
            11 => "#ffd28a",
            12 => "#a9c2ff",
            13 => "#d4b8ff",
            14 => "#a3e5ff",
            15 => "#e8edff",
            _ => "#c0caf5",
        }
        .to_string()
    } else if idx < 232 {
        let i = (idx - 16) as u32;
        let r_val = if i / 36 > 0 { 55 + (i / 36) * 40 } else { 0 };
        let g_val = if (i % 36) / 6 > 0 {
            55 + ((i % 36) / 6) * 40
        } else {
            0
        };
        let b_val = if i % 6 > 0 { 55 + (i % 6) * 40 } else { 0 };
        format!(
            "#{:02x}{:02x}{:02x}",
            r_val.min(255),
            g_val.min(255),
            b_val.min(255)
        )
    } else {
        let v = 8 + (idx - 232) as u16 * 10;
        let h = v.min(255) as u8;
        format!("#{h:02x}{h:02x}{h:02x}")
    }
}

// ── HTML escape ────────────────────────────────────────────────────

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

// ── Optimized row → HTML ───────────────────────────────────────────

/// Build CSS style string from cell attributes.
fn cell_style(fg: &CellColor, bg: &CellColor, flags: CellFlags) -> String {
    let mut parts = Vec::new();
    let fg_css = color_to_css(fg);
    if !fg_css.is_empty() {
        parts.push(fg_css);
    }
    let bg_css = color_to_css(bg);
    if !bg_css.is_empty() {
        parts.push(format!("background:{}", bg_css));
    }
    if flags.contains(CellFlags::BOLD) {
        parts.push("font-weight:700".to_string());
    }
    if flags.contains(CellFlags::ITALIC) {
        parts.push("font-style:italic".to_string());
    }
    if flags.contains(CellFlags::UNDERLINE) {
        parts.push("text-decoration:underline".to_string());
    }
    if flags.contains(CellFlags::STRIKETHROUGH) {
        parts.push("text-decoration:line-through".to_string());
    }
    parts.join(";")
}

/// Background applied to cells inside the active mouse-drag selection.
/// Matches the search-match highlight hue so all accent overlays read as
/// one system. Applied over the cell's own background (selection bg wins).
const SELECTION_BG: &str = "background:rgba(122,162,247,0.30)";

/// Convert client-viewport px coords to a `(row, col)` terminal cell using
/// the live `getBoundingClientRect` origin of the content div and the
/// measured monospace cell size. Coordinates OUTSIDE the grid clamp to the
/// nearest edge cell.
///
/// IMPORTANT: this must be fed `client_coordinates()` — NOT
/// `element_coordinates()`. On dioxus desktop the latter is DOM
/// `offsetX/offsetY`, which is relative to the event TARGET. Terminal rows
/// are raw HTML (`dangerous_inner_html`), so the target moves between
/// spans/row-divs as the cursor moves, producing garbage offsets
/// (client-rect math is target-independent). The content div is also the
/// cell-grid origin: the line-number gutter is a sibling column.
#[expect(
    clippy::too_many_arguments,
    reason = "geometry tuple unpacked + grid bounds"
)]
fn event_cell_from_coords(
    x: f64,
    y: f64,
    left: f64,
    top: f64,
    cw: f64,
    ch: f64,
    rows_len: usize,
    max_col: usize,
) -> (usize, usize) {
    let x = x - left;
    let y = y - top;
    let col = if x <= 0.0 {
        0
    } else {
        (x / cw).floor() as usize
    };
    let row = if y <= 0.0 {
        0
    } else {
        (y / ch).floor() as usize
    };
    (
        row.min(rows_len.saturating_sub(1)),
        col.min(max_col.saturating_sub(1)),
    )
}

/// Classify a single character into one of three xterm-style char classes
/// used for double-click word selection:
///   - `Word`: identifier characters `[A-Za-z0-9_]`
///   - `Punct`: any other non-whitespace character (symbols, punctuation)
///   - `Space`: whitespace (ASCII space — terminal cells never hold \n/\t;
///     those are encoded as a space in the rendered grid).
///
/// Selecting the run of the SAME class as the clicked cell reproduces
/// WindTerm/xterm behaviour: clicking inside an identifier selects the whole
/// identifier; clicking on a punctuation glyph selects a run of punctuation;
/// clicking on whitespace selects the whitespace run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CharClass {
    Word,
    Punct,
    Space,
}

fn char_class(c: char) -> CharClass {
    if c.is_ascii_alphanumeric() || c == '_' {
        CharClass::Word
    } else if c.is_whitespace() {
        CharClass::Space
    } else {
        CharClass::Punct
    }
}

/// Compute the inclusive `(start_col, end_col)` word range in `cells` for a
/// double-click at column `col`. The range covers the maximal same-`CharClass`
/// run containing `col`, matching WindTerm/xterm word selection.
///
/// Wide-char continuation cells (`wide_next == true`) are placeholders for the
/// second column of a CJK/emoticon glyph: they carry no character of their
/// own and must not break the selection at the glyph boundary. They inherit
/// the `CharClass` of their parent wide cell (the cell to their left with
/// `wide == true`), so a double-click on either half of a wide glyph selects
/// the whole glyph, and a run of same-class wide glyphs extends across both
/// columns of each.
///
/// If `col` is out of range the result is `(col, col)` clamped to the last
/// valid index (or `(0, 0)` for an empty row).
pub(crate) fn word_range_in_row(cells: &[RenderCell], col: usize) -> (usize, usize) {
    if cells.is_empty() {
        return (0, 0);
    }
    let col = col.min(cells.len() - 1);
    let clicked_class = cell_class(cells, col);

    let mut start = col;
    while start > 0 && cell_class(cells, start - 1) == clicked_class {
        start -= 1;
    }
    let mut end = col;
    while end + 1 < cells.len() && cell_class(cells, end + 1) == clicked_class {
        end += 1;
    }
    (start, end)
}

/// `CharClass` of the glyph occupying column `idx`. A `wide_next` continuation
/// cell inherits the class of its parent wide cell (the nearest `wide: true`
/// cell to the left — wide glyphs occupy exactly two columns), so the run
/// doesn't fracture across a wide glyph's two cells.
fn cell_class(cells: &[RenderCell], idx: usize) -> CharClass {
    if cells[idx].wide_next {
        if idx > 0 && cells[idx - 1].wide {
            return char_class(cells[idx - 1].character);
        }
        // Malformed grid (wide_next with no preceding wide cell) — treat as
        // punctuation so it at least doesn't merge with adjacent word runs.
        return CharClass::Punct;
    }
    char_class(cells[idx].character)
}

/// Render a terminal row to an HTML string. Uses `dangerous_inner_html`
/// for fast DOM updates — avoids Dioxus per-span VDOM diffing overhead.
///
/// When a suggestion is shown, we only render cells up to the cursor position,
/// then append the suggestion right after it. Cells after the cursor are
/// typically empty spaces and would push the suggestion to the end of the row.
///
/// `sel` is the inclusive cell-column range `(start, end)` inside the active
/// mouse selection for this row, if any; covered cells get [`SELECTION_BG`].
fn row_to_html(
    row: &RenderRow,
    cursor_col: Option<usize>,
    suggestion: Option<&str>,
    sel: Option<(usize, usize)>,
) -> String {
    let mut html = String::with_capacity(row.cells.len() * 4);

    let mut cur_fg = CellColor::Default;
    let mut cur_bg = CellColor::Default;
    let mut cur_flags = CellFlags::empty();
    let mut cur_sel = false;
    let mut cur_text = String::new();

    let flush = |html: &mut String,
                 text: &str,
                 fg: &CellColor,
                 bg: &CellColor,
                 flags: CellFlags,
                 sel: bool| {
        if text.is_empty() {
            return;
        }
        let mut style = cell_style(fg, bg, flags);
        if sel {
            if style.is_empty() {
                style = SELECTION_BG.to_string();
            } else {
                style.push(';');
                style.push_str(SELECTION_BG);
            }
        }
        let escaped = html_escape(text);
        if style.is_empty() {
            html.push_str(&escaped);
        } else {
            html.push_str("<span style=\"");
            html.push_str(&style);
            html.push_str("\">");
            html.push_str(&escaped);
            html.push_str("</span>");
        }
    };

    // If we have a suggestion, stop rendering after the cursor position
    // so the suggestion appears immediately after the typed text.
    let stop_at = if suggestion.is_some() {
        cursor_col.map(|c| c + 1)
    } else {
        None
    };

    for (i, cell) in row.cells.iter().enumerate() {
        if let Some(stop) = stop_at {
            if i >= stop {
                break;
            }
        }

        if cell.wide_next {
            continue;
        }

        let is_cursor = cursor_col == Some(i);
        if is_cursor {
            flush(&mut html, &cur_text, &cur_fg, &cur_bg, cur_flags, cur_sel);
            cur_text.clear();

            let ch = if cell.character == ' ' {
                "&nbsp;"
            } else {
                &html_escape(&cell.character.to_string())
            };
            let base_style = cell_style(&cell.fg, &cell.bg, cell.flags);
            let cursor_style = if base_style.is_empty() {
                "border-left:2px solid #c0caf5;margin-left:-1px".to_string()
            } else {
                format!(
                    "{};border-left:2px solid #c0caf5;margin-left:-1px",
                    base_style
                )
            };
            html.push_str("<span style=\"");
            html.push_str(&cursor_style);
            html.push_str("\">");
            html.push_str(ch);
            html.push_str("</span>");

            cur_fg = CellColor::Default;
            cur_bg = CellColor::Default;
            cur_flags = CellFlags::empty();
            // cur_sel persists across the cursor — the selection underneath
            // the cursor cell continues after it.
            continue;
        }

        let in_sel = sel.is_some_and(|(a, b)| i >= a && i <= b);
        let same_style =
            cell.fg == cur_fg && cell.bg == cur_bg && cell.flags == cur_flags && in_sel == cur_sel;
        if !cur_text.is_empty() && !same_style {
            flush(&mut html, &cur_text, &cur_fg, &cur_bg, cur_flags, cur_sel);
            cur_text.clear();
        }

        cur_fg = cell.fg.clone();
        cur_bg = cell.bg.clone();
        cur_flags = cell.flags;
        cur_sel = in_sel;
        cur_text.push(cell.character);
    }

    flush(&mut html, &cur_text, &cur_fg, &cur_bg, cur_flags, cur_sel);

    // Insert suggestion right after the cursor content
    if let Some(sug) = suggestion {
        html.push_str("<span style=\"color:#565f89;opacity:0.55\">");
        html.push_str(&html_escape(sug));
        html.push_str("</span>");
    }

    html
}

// ── TerminalView component ─────────────────────────────────────────

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
    /// Delete the currently-selected suggestion from history (dirty-data
    /// cleanup). Triggered by Shift+Delete while the suggestion panel is open.
    /// The handler in `app.rs` removes the command from `command_history`,
    /// inserts it into `recent_failed_commands` as an immediate guard, and
    /// spawns `mark_command_failed(&cmd, 1)` so the failure marker is durable
    /// against the next history import. We use `mark_command_failed` (NOT
    /// `delete_history_by_command`) because deletion would let the next
    /// `~/.bash_history` import re-introduce the command as `exit_code = NULL`,
    /// which the HAVING clause keeps — re-surfacing the typo.
    on_suggestion_delete: EventHandler<String>,
    onekey_visible: bool,
    onekey_entries: Vec<OneKeyMatch>,
    onekey_selected: usize,
    onekey_submission_feedback: Option<OneKeySubmissionFeedback>,
    on_onekey_navigate: EventHandler<Option<usize>>,
    on_onekey_select: EventHandler<usize>,
    on_onekey_save: EventHandler<()>,
    on_onekey_dismiss: EventHandler<()>,
    /// True when the session's SSH/shell channel has dropped. While set, Enter
    /// triggers `on_reconnect` and all other keys are ignored (no live PTY).
    disconnected: bool,
    on_reconnect: EventHandler<()>,
) -> Element {
    let mut focused = use_signal(|| false);
    let mut search_visible = use_signal(|| false);
    let mut search_query = use_signal(String::new);
    let mut search_match_index = use_signal(|| 0usize);
    let mut search_matches: Signal<Vec<(usize, usize)>> = use_signal(Vec::new);

    // ── Mouse selection & reporting state ──
    let mut selection: Signal<Option<TextSelection>> = use_signal(|| None);
    // Text captured for the current selection at mouseup (kept so
    // Ctrl+Shift+C / Cmd+C can re-copy it without re-hit-testing).
    let mut selection_text: Signal<String> = use_signal(String::new);
    let mut selecting = use_signal(|| false);
    let mut mouse_button_down: Signal<Option<u8>> = use_signal(|| None);
    let mut last_motion_cell: Signal<Option<(usize, usize)>> = use_signal(|| None);
    // (content-left, content-top, cell-width, cell-height), in client px.
    // Polled by the geometry effect: the rect changes on resize/pane drags,
    // the cell metrics only change with a font (re)load. NONE until the first
    // successful poll — mouse/wheel hit-testing ignores events before that.
    let mut content_geo: Signal<Option<(f64, f64, f64, f64)>> = use_signal(|| None);

    let current_suggestion = suggestion.clone();
    let current_suggestions = suggestions.clone();
    let current_suggestion_visible = suggestion_visible;
    let current_suggestion_selected = suggestion_selected;

    let current_onekey_visible = onekey_visible;
    let current_onekey_entries = onekey_entries.clone();
    let current_onekey_selected = onekey_selected;
    let current_onekey_submission_feedback = onekey_submission_feedback.clone();
    let current_disconnected = disconnected;

    let closure_suggestions = current_suggestions.clone();
    let sid_for_keydown_log = session_id.clone();
    let sid_for_copy = session_id.clone();
    let copy_rows = render_output.rows.clone();
    let handle_keydown = move |e: KeyboardEvent| {
        let key = e.key();
        let code = e.code();
        let mods = e.modifiers();
        let ctrl = mods.ctrl();
        let alt = mods.alt();
        let meta = mods.meta();
        let shift = mods.shift();
        tracing::info!(
            "[KEYDOWN] session={:?} key={:?} code={:?} ctrl={} alt={} meta={} shift={}",
            &sid_for_keydown_log[..sid_for_keydown_log.len().min(8)],
            key,
            code,
            ctrl,
            alt,
            meta,
            shift
        );

        // macOS conventions: Cmd+V always pastes into the PTY. Copy is
        // routed together with Ctrl+Shift+C below so both shortcuts take
        // priority over a visible OneKey popup.
        if meta && !ctrl && !alt {
            if let Key::Character(ref s) = key {
                if s.eq_ignore_ascii_case("v") {
                    e.prevent_default();
                    let bracketed = render_output.mode_bracketed_paste;
                    paste_from_clipboard(&on_input, bracketed);
                    return;
                }
            }
        }

        let overlay_key_action = terminal_overlay_key_action(
            &key,
            ctrl,
            alt,
            meta,
            shift,
            current_onekey_visible,
            current_onekey_selected,
            current_onekey_entries.len(),
        );
        match overlay_key_action {
            TerminalOverlayKeyAction::Copy(CopyShortcut::Command) => {
                // Mouseup can occur outside this pane. Recompute from the
                // terminal-owned cell range when its mouseup cache is empty.
                let text = terminal_selection_text(&selection_text(), selection(), &copy_rows);
                if !text.is_empty() {
                    e.prevent_default();
                    copy_text_to_clipboard(text);
                }
                // With no terminal-owned selection, preserve the browser's
                // native copy behavior for popup/input DOM selections.
                return;
            }
            TerminalOverlayKeyAction::Copy(CopyShortcut::CtrlShift) => {
                e.prevent_default();
                let text = terminal_selection_text(&selection_text(), selection(), &copy_rows);
                if !text.is_empty() {
                    if let ClipboardCopyOutcome::Copied(n) = copy_text_to_clipboard(text) {
                        tracing::info!("[COPY] Ctrl+Shift+C copied {n} chars (terminal selection)");
                    }
                    return;
                }
                // No terminal-owned selection — fall back to the DOM selection
                // (or an active INPUT/TEXTAREA). The selection text is only
                // available from JS, so eval to read it, then write it to the
                // clipboard natively (arboard) once it comes back.
                let sid_for_copy_log = sid_for_copy.clone();
                spawn(async move {
                    let js = "\
                        var sel = window.getSelection();
                        var text = sel ? sel.toString() : '';
                        if (!text) {
                            var a = document.activeElement;
                            if (a && (a.tagName === 'INPUT' || a.tagName === 'TEXTAREA') && typeof a.selectionStart === 'number' && typeof a.selectionEnd === 'number') {
                                text = a.value.substring(a.selectionStart, a.selectionEnd);
                            }
                        }
                        return text || '';
";
                    let text: String = match dioxus::document::eval(js).await {
                        Ok(v) => v.as_str().unwrap_or("").to_string(),
                        Err(_) => String::new(),
                    };
                    if !text.is_empty() {
                        if let ClipboardCopyOutcome::Copied(n) = copy_text_to_clipboard(text) {
                            tracing::info!(
                                "[COPY] Ctrl+Shift+C copied {} chars for session {:?}",
                                n,
                                &sid_for_copy_log[..sid_for_copy_log.len().min(8)]
                            );
                        }
                    } else {
                        tracing::debug!(
                            "[COPY] Ctrl+Shift+C fired but no selection for session {:?}",
                            &sid_for_copy_log[..sid_for_copy_log.len().min(8)]
                        );
                    }
                });
                return;
            }
            TerminalOverlayKeyAction::OneKey(_) | TerminalOverlayKeyAction::None => {}
        }

        if meta {
            return;
        }
        e.prevent_default();

        // Ctrl+Shift+W — cross-platform "close focused pane" shortcut.
        // Let it bubble to the App's `onkeydown` (which calls
        // `close_session`) WITHOUT sending anything to the PTY. Without
        // this early return, the keymap's `Ctrl+Shift+<alpha>` arm would
        // emit a CSI modifier-6 sequence (Ctrl+Shift+W = `CSI 1;6 W`),
        // and the pane would close at the same time — double action.
        // macOS Cmd+Shift+W already bubbles via the `meta` early return
        // above, so this only needs to handle the Ctrl variant.
        if ctrl && shift && !alt {
            if let Key::Character(ref s) = key {
                if s.eq_ignore_ascii_case("w") {
                    return;
                }
            }
        }

        // Disconnected session: Enter reconnects, everything else is ignored
        // (there's no live PTY to send to).
        if current_disconnected {
            if matches!(key, Key::Enter) {
                on_reconnect.call(());
            }
            return;
        }

        // OneKey autofill popup — takes precedence when visible. Keep every
        // handled key inside this TerminalView so Enter/Tab cannot bubble to an
        // ancestor keyboard handler after selecting a credential.
        if let TerminalOverlayKeyAction::OneKey(action) = overlay_key_action {
            e.stop_propagation();
            match action {
                OneKeyKeyAction::Navigate(idx) => {
                    on_onekey_navigate.call(Some(idx));
                    return;
                }
                OneKeyKeyAction::Select => {
                    let selected =
                        current_onekey_selected.min(current_onekey_entries.len().saturating_sub(1));
                    on_onekey_select.call(selected);
                    return;
                }
                OneKeyKeyAction::Dismiss => {
                    on_onekey_dismiss.call(());
                    return;
                }
                OneKeyKeyAction::DismissAndForward => {
                    // The user is typing/editing manually. Close the popup so it
                    // can't hijack the next Enter (which would concatenate the
                    // popup's saved value onto the typed credential), then let
                    // the key fall through to the PTY — no `return`.
                    on_onekey_dismiss.call(());
                }
            }
        }

        // Ctrl+Shift+F: toggle search bar
        if ctrl && shift && matches!(key, Key::Character(ref s) if s == "f" || s == "F") {
            search_visible.toggle();
            if !search_visible() {
                search_query.set(String::new());
                search_matches.set(Vec::new());
                search_match_index.set(0);
            }
            return;
        }

        if search_visible() {
            if matches!(key, Key::Enter) {
                let matches = search_matches();
                if !matches.is_empty() {
                    let next = (search_match_index() + 1) % matches.len();
                    search_match_index.set(next);
                }
                return;
            }
            if matches!(key, Key::Escape) {
                search_visible.set(false);
                search_query.set(String::new());
                search_matches.set(Vec::new());
                search_match_index.set(0);
                return;
            }
            return;
        }

        // Ctrl+Shift+V / Shift+Insert: paste from clipboard
        if (ctrl && shift && matches!(key, Key::Character(ref s) if s == "v" || s == "V"))
            || (shift && matches!(key, Key::Insert))
        {
            let bracketed = render_output.mode_bracketed_paste;
            paste_from_clipboard(&on_input, bracketed);
            return;
        }

        // ── Suggestion panel ──
        //
        // Arrow keys are intentionally NOT intercepted by the suggestion
        // panel. In a terminal, arrow keys are the primary cursor-movement
        // and history-navigation mechanism (Left/Right move within the line,
        // Up/Down traverse command history). If the suggestion panel hijacked
        // them for list navigation, the user would lose the ability to move
        // the cursor or browse history whenever a suggestion happened to be
        // visible — which is almost always, because the suggestion query
        // fires on every keystroke.
        //
        // Instead, when an arrow key is pressed while the panel is visible,
        // we dismiss the panel and let the key fall through to the PTY (so
        // the shell moves the cursor / changes history as expected). The
        // panel can still be driven via Tab (accept), Escape (dismiss), and
        // Shift+Delete (purge entry).
        if current_suggestion_visible && !closure_suggestions.is_empty() {
            match &key {
                // Arrow keys dismiss the panel and fall through to the PTY
                // so the shell handles cursor movement / history navigation.
                Key::ArrowDown | Key::ArrowUp | Key::ArrowLeft | Key::ArrowRight => {
                    on_suggestion_dismiss.call(());
                    // Don't return — let the key continue to the PTY.
                }
                Key::Tab => {
                    // Tab accepts the selected suggestion
                    if let Some(cmd) = closure_suggestions.get(current_suggestion_selected) {
                        on_suggestion_accept.call(cmd.clone());
                    }
                    return;
                }
                Key::Escape => {
                    on_suggestion_dismiss.call(());
                    return;
                }
                // Shift+Delete: delete the currently-selected suggestion from
                // history. This is the user-facing dirty-data cleanup affordance
                // — typos and broken commands that snuck into suggestions (from
                // bash/zsh flat history files that have no exit-code info) can
                // be purged on the spot. The handler in app.rs marks the command
                // as failed durably (via `mark_command_failed`) so subsequent
                // history imports skip it.
                //
                // Why Shift+Delete (not Ctrl+Delete or plain Delete)? Matches
                // the convention used by VS Code and IntelliJ for
                // "delete suggestion" / "remove autocomplete entry". Plain
                // Delete is reserved for shell-side forward-delete.
                Key::Delete if shift => {
                    if let Some(cmd) = closure_suggestions.get(current_suggestion_selected) {
                        on_suggestion_delete.call(cmd.clone());
                    }
                    return;
                }
                // Enter falls through to PTY normally (also dismisses panel)
                Key::Enter => {
                    on_suggestion_dismiss.call(());
                    // Don't return — let Enter continue to PTY
                }
                _ => {}
            }
        }

        // ── Auto-completion: accept inline suggestion with End/Ctrl+E/Tab ──
        //
        // ArrowRight is intentionally excluded — in a terminal, Right moves
        // the cursor forward within the command line. If an inline ghost-text
        // suggestion were accepted on Right, the user could never move the
        // cursor rightward without accidentally swallowing the suggestion.
        // End, Tab, and Ctrl+E remain as accept keys (they naturally land at
        // end-of-line or are explicit "accept" affordances).
        if current_suggestion.is_some() {
            let is_accept = match &key {
                Key::End => true,
                Key::Tab => true,
                Key::Character(s) if ctrl && !alt && !shift && s.eq_ignore_ascii_case("e") => true,
                _ => false,
            };
            if is_accept {
                if let Some(ref sug) = current_suggestion {
                    on_input.call(sug.as_bytes().to_vec());
                    return;
                }
            }
        }

        // Shift+PageUp/PageDown/Home/End: scroll local scrollback
        if shift && !ctrl && !alt {
            match key {
                Key::PageUp => {
                    on_scroll_up.call(10);
                    return;
                }
                Key::PageDown => {
                    on_scroll_down.call(10);
                    return;
                }
                Key::Home => {
                    on_scroll_up.call(render_output.scrollback_total);
                    return;
                }
                Key::End => {
                    on_scroll_to_bottom.call(());
                    return;
                }
                _ => {}
            }
        }

        let is_enter = !ctrl && !alt && matches!(key, Key::Enter);
        let app_cursor = render_output.mode_cursor_keys;

        let modifier: Option<u8> = match (ctrl, alt, shift) {
            (false, false, false) => None,
            (false, false, true) => Some(2),
            (false, true, false) => Some(3),
            (false, true, true) => Some(4),
            (true, false, false) => Some(5),
            (true, false, true) => Some(6),
            (true, true, false) => Some(7),
            (true, true, true) => Some(8),
        };

        let data: Vec<u8> = match key {
            Key::ArrowUp => cursor_key_seq(1, b'A', app_cursor, modifier),
            Key::ArrowDown => cursor_key_seq(1, b'B', app_cursor, modifier),
            Key::ArrowRight => cursor_key_seq(1, b'C', app_cursor, modifier),
            Key::ArrowLeft => cursor_key_seq(1, b'D', app_cursor, modifier),

            Key::Home => csi_seq(1, modifier, b'H'),
            Key::End => csi_seq(1, modifier, b'F'),
            Key::Insert => csi_seq(2, modifier, b'~'),
            Key::Delete => csi_seq(3, modifier, b'~'),
            Key::PageUp => csi_seq(5, modifier, b'~'),
            Key::PageDown => csi_seq(6, modifier, b'~'),

            Key::F1 => cursor_key_seq(1, b'P', app_cursor, modifier),
            Key::F2 => cursor_key_seq(1, b'Q', app_cursor, modifier),
            Key::F3 => cursor_key_seq(1, b'R', app_cursor, modifier),
            Key::F4 => cursor_key_seq(1, b'S', app_cursor, modifier),

            Key::F5 => csi_seq(15, modifier, b'~'),
            Key::F6 => csi_seq(17, modifier, b'~'),
            Key::F7 => csi_seq(18, modifier, b'~'),
            Key::F8 => csi_seq(19, modifier, b'~'),
            Key::F9 => csi_seq(20, modifier, b'~'),
            Key::F10 => csi_seq(21, modifier, b'~'),
            Key::F11 => csi_seq(23, modifier, b'~'),
            Key::F12 => csi_seq(24, modifier, b'~'),

            Key::Character(ref s) if ctrl && !alt && !shift => ctrl_char(s),
            Key::Character(ref s) if alt && !ctrl => {
                let mut buf = vec![0x1b];
                buf.extend_from_slice(s.as_bytes());
                buf
            }
            Key::Character(ref s) if ctrl && shift && !alt => {
                let c = s.chars().next().unwrap_or('A');
                if c.is_ascii_alphabetic() {
                    csi_seq(1, Some(6), c as u8)
                } else {
                    let base = code_to_char(&code);
                    csi_seq(1, Some(6), base)
                }
            }
            Key::Character(ref s) if ctrl && alt && !shift => {
                let ctrl_ch = ctrl_char(s);
                if !ctrl_ch.is_empty() && ctrl_ch[0] != 0x1b {
                    let mut buf = vec![0x1b];
                    buf.extend_from_slice(&ctrl_ch);
                    buf
                } else {
                    vec![]
                }
            }

            Key::Enter => {
                if alt {
                    vec![0x1b, 0x0d]
                } else {
                    vec![0x0d]
                }
            }
            Key::Backspace => {
                if alt {
                    vec![0x1b, 0x7f]
                } else {
                    vec![0x7f]
                }
            }
            Key::Tab => vec![0x09],
            Key::Escape => vec![0x1b],

            Key::Character(ref s) => s.as_bytes().to_vec(),
            _ => vec![],
        };

        if !data.is_empty() {
            if is_enter {
                on_command.call(version.to_string());
            }
            on_input.call(data);
        }
    };

    let container_id = format!("terminal-input-{session_id}");
    let scroll_id = format!("terminal-scroll-{session_id}");

    let sid_for_focus = session_id.clone();
    use_effect(move || {
        let focus_sid = sid_for_focus.clone();
        let cid = format!("terminal-input-{focus_sid}");
        tracing::info!(
            "[AUTOFOCUS] use_effect mounted for session={:?}",
            &focus_sid[..focus_sid.len().min(8)]
        );
        spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            // Defensive: only focus if no other terminal-input-* element is
            // already focused. This prevents multi-pane mount races where
            // each TerminalView's use_effect fires and they all try to grab
            // focus, with the last one winning (which may not be the active
            // pane). By checking first, we ensure only the first-mounted
            // pane grabs focus — which, in the split-from-single case, is
            // the existing (active) pane.
            //
            // The check uses `document.activeElement` and matches by id
            // prefix `terminal-input-` so any already-focused terminal input
            // suppresses the auto-focus.
            let check_and_focus = format!(
                "return (function() {{
                    const active = document.activeElement;
                    if (active && active.id && active.id.indexOf('terminal-input-') === 0) {{
                        return 'already-focused:' + active.id;
                    }}
                    const el = document.getElementById('{cid}');
                    if (el) {{ el.focus(); return 'focused'; }}
                    return 'not-found';
                }})()"
            );
            let result = dioxus::document::eval(&check_and_focus).await;
            tracing::info!("[AUTOFOCUS] check_and_focus #{} result={:?}", &cid, result);
        });
    });

    let sid_for_window_focus = session_id.clone();
    use_effect(move || {
        let cid = format!("terminal-input-{sid_for_window_focus}");
        // Use a per-session global key on `window` (NOT on the element) so
        // that re-mounts (which create a NEW element with the same id) can
        // still find and remove the previous handlers. The prior approach
        // stored handlers on `el._windowFocusHandler`, but when the element
        // is replaced by dioxus on re-mount, the new element doesn't have
        // the old handlers — so `removeEventListener` was a no-op and the
        // old handlers leaked, piling up on every layout change. After N
        // layout changes there'd be N stale focus handlers all trying to
        // focus a removed element — causing focus races and lost input.
        let handler_key = format!("_rusterm_focus_handler_{sid_for_window_focus}");
        let script = format!(
            r#"
            (function() {{
                // Remove previous handlers for this session if they exist.
                if (window['{handler_key}_focus']) {{
                    window.removeEventListener('focus', window['{handler_key}_focus']);
                    window.removeEventListener('blur', window['{handler_key}_blur']);
                    delete window['{handler_key}_focus'];
                    delete window['{handler_key}_blur'];
                }}
                const cid = '{cid}';
                const focusHandler = function() {{
                    const el = document.getElementById(cid);
                    if (!el) return;
                    const active = document.activeElement;
                    const isInteractive = active && (
                        active.tagName === 'INPUT' || active.tagName === 'BUTTON' ||
                        active.tagName === 'SELECT' || active.tagName === 'TEXTAREA' ||
                        active.closest('[role="dialog"]')
                    );
                    if (!isInteractive) el.focus();
                }};
                const blurHandler = function() {{
                    const el = document.getElementById(cid);
                    if (!el) return;
                    if (document.activeElement === el) el.blur();
                }};
                window['{handler_key}_focus'] = focusHandler;
                window['{handler_key}_blur'] = blurHandler;
                window.addEventListener('focus', focusHandler);
                window.addEventListener('blur', blurHandler);
            }})()
            "#
        );
        spawn(async move {
            let _ = dioxus::document::eval(&script).await;
        });
    });

    let resize_sid = session_id.clone();
    let resize_on_resize = on_resize;
    let _resize_future = use_future(move || {
        let sid = resize_sid.clone();
        let on_resize_cb = resize_on_resize;
        async move {
            let mut last_cols: u16 = 0;
            let mut last_rows: u16 = 0;
            let cid = format!("terminal-input-{sid}");

            // Set up a ResizeObserver for immediate resize detection
            // (more responsive than polling alone, handles window maximize)
            let observer_script = format!(
                "(function() {{ const el = document.getElementById('{cid}'); if (!el || el._rusterm_ro) return; el._rusterm_ro = new ResizeObserver(function() {{ el._rusterm_resize_pending = true; }}); el._rusterm_ro.observe(el); }})()"
            );
            let _ = dioxus::document::eval(&observer_script).await;

            loop {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                let measure_cid = cid.clone();
                let scroll_cid = format!("terminal-scroll-{sid}");
                // Measurement strategy: compute the terminal content width as
                // (scroll_div_width - gutter_width) where gutter_width is read
                // directly from the gutter element (terminal-scroll's
                // firstElementChild). This is stable from first mount onward,
                // unlike the previous approach which measured
                // `lastElementChild` (the content div) — that returned an
                // over-wide value on the first poll because the gutter hadn't
                // been laid out yet (render_output was Default::default() with
                // scrollback_capacity=0, producing a 2ch gutter instead of the
                // stable 6ch). The result was a transient wrong-cols resize
                // (e.g. 207 → 203 within 100ms) that visibly re-wrapped remote
                // output. Reading both children's bounding rects explicitly
                // avoids that race.
                let result = dioxus::document::eval(&format!(
                    "return (function() {{ const el = document.getElementById('{measure_cid}'); if (!el) return 'no-el'; const rect = el.getBoundingClientRect(); if (rect.width <= 0 || rect.height <= 0) return 'zero'; const cs = getComputedStyle(el); const padH = parseFloat(cs.paddingLeft) + parseFloat(cs.paddingRight); const padV = parseFloat(cs.paddingTop) + parseFloat(cs.paddingBottom); const bw = parseFloat(cs.borderLeftWidth) + parseFloat(cs.borderRightWidth); const bh = parseFloat(cs.borderTopWidth) + parseFloat(cs.borderBottomWidth); const h = rect.height - padV - bh; if (h <= 0) return 'small'; const sd = document.getElementById('{scroll_cid}'); if (!sd) return 'no-scroll'; const sdRect = sd.getBoundingClientRect(); if (sdRect.width <= 0) return 'small'; let w = sdRect.width; if (sd.firstElementChild) {{ const gutterW = sd.firstElementChild.getBoundingClientRect().width; w = Math.max(0, sdRect.width - gutterW); }} if (w <= 0) return 'small'; const test = document.createElement('span'); test.textContent = 'M'; test.style.cssText = 'font-family:JetBrains Mono,Fira Code,Cascadia Code,monospace;font-size:13px;line-height:1.5;position:absolute;visibility:hidden;white-space:pre;'; document.body.appendChild(test); const tr = test.getBoundingClientRect(); document.body.removeChild(test); const cw = Math.max(1, tr.width); const ch = Math.max(1, tr.height); const cols = Math.max(1, Math.floor(w / cw)); const rows = Math.max(1, Math.floor(h / ch)); const cr_sug = el.querySelector('[data-cursor-row=\"1\"]'); if (cr_sug) {{ const tr_sug = el.getBoundingClientRect(); const cr_r_sug = cr_sug.getBoundingClientRect(); el.style.setProperty('--suggestion-bottom', (tr_sug.bottom - cr_r_sug.top) + 'px'); el.style.setProperty('--suggestion-top', (cr_r_sug.bottom - tr_sug.top) + 'px'); }} return cols + ',' + rows + ',' + cw.toFixed(2) + ',' + ch.toFixed(2); }})()"
                )).await;
                if let Ok(value) = result {
                    if let Some(s) = value.as_str() {
                        if s == "no-el"
                            || s == "no-scroll"
                            || s == "zero"
                            || s == "small"
                            || s.is_empty()
                        {
                            continue;
                        }
                        let parts: Vec<&str> = s.split(',').collect();
                        if parts.len() >= 2 {
                            if let (Ok(cols), Ok(rows)) =
                                (parts[0].parse::<u16>(), parts[1].parse::<u16>())
                            {
                                if cols != last_cols || rows != last_rows {
                                    last_cols = cols;
                                    last_rows = rows;
                                    let char_w: f64 =
                                        parts.get(2).and_then(|v| v.parse().ok()).unwrap_or(0.0);
                                    let char_h: f64 =
                                        parts.get(3).and_then(|v| v.parse().ok()).unwrap_or(0.0);
                                    let pw = (char_w * cols as f64).round() as u32;
                                    let ph = (char_h * rows as f64).round() as u32;
                                    on_resize_cb.call((cols, rows, pw, ph));
                                }
                            }
                        }
                    }
                }
            }
        }
    });

    let cursor_row = render_output.cursor_row;
    let cursor_col = render_output.cursor_col;
    let cursor_visible = render_output.cursor_visible;
    let line_number_start = render_output.line_number_start;
    let total_rows = render_output.rows.len();

    // Scroll-position indicator: a small "thumb" on the right edge showing
    // where the visible window sits within (scrollback + grid). Only shown when
    // there is scrollback. At the bottom (scroll_offset 0) the thumb rests at
    // the bottom; scrolling up moves it up proportionally.
    let (show_position_indicator, thumb_top_pct, thumb_height_pct) = scroll_thumb_geometry(
        render_output.scrollback_total,
        render_output.scrollback_offset,
        total_rows,
    );
    let thumb_top_str = format!("{:.2}", thumb_top_pct);
    let thumb_height_str = format!("{:.2}", thumb_height_pct);

    // The `--suggestion-bottom` CSS variable (used by SuggestionPopup to sit
    // above the cursor row) is kept current by the resize future above, which
    // re-measures every 100ms. (A use_effect here would only run once on mount
    // — version is a plain prop, not a tracked Signal — leaving the value stale.)

    // Recompute search matches
    {
        let query = search_query();
        let _ = version;
        if !query.is_empty() {
            let q = query.to_lowercase();
            let mut found = Vec::new();
            for (row_idx, row) in render_output.rows.iter().enumerate() {
                let line: String = row
                    .cells
                    .iter()
                    .filter(|c| !c.wide_next)
                    .map(|c| c.character)
                    .collect();
                let lower = line.to_lowercase();
                let mut start = 0;
                while let Some(pos) = lower[start..].find(&q) {
                    found.push((row_idx, start + pos));
                    start = start + pos + 1;
                    if start >= lower.len() {
                        break;
                    }
                }
            }
            search_matches.set(found);
        }
    }

    let focus_container_id = container_id.clone();
    let onclick_focus = move |_| {
        tracing::info!(
            "[FOCUS] onclick_focus fired for session={:?}",
            &focus_container_id[..focus_container_id.len().min(20)]
        );
        focused.set(true);
        let cid = focus_container_id.clone();
        spawn(async move {
            let _ =
                dioxus::document::eval(&format!("document.getElementById('{cid}')?.focus()")).await;
        });
    };

    // Right-click handler. We always prevent the browser's native context
    // menu — the terminal owns right-click for its own affordances. When the
    // session is disconnected, right-click triggers reconnect (mirrors the
    // Enter-to-reconnect path in `handle_keydown`). When the session is live,
    // right-click PASTES (the WindTerm/xterm convention; Ctrl+Shift+V and
    // Cmd+V also work). While the app has mouse reporting enabled the
    // right-button press was already forwarded to the PTY by `on_mousedown`,
    // so we only suppress the native menu here.
    let reconnect_sid = session_id.clone();
    let contextmenu_on_input = on_input;
    let contextmenu_bracketed = render_output.mode_bracketed_paste;
    let contextmenu_reporting = render_output.mode_mouse_reporting;
    let oncontextmenu_reconnect = move |e: MouseEvent| {
        e.prevent_default();
        if current_disconnected {
            tracing::info!(
                "[RECONNECT] right-click triggered for session={:?}",
                &reconnect_sid[..reconnect_sid.len().min(8)]
            );
            on_reconnect.call(());
        } else if !contextmenu_reporting {
            paste_from_clipboard(&contextmenu_on_input, contextmenu_bracketed);
        }
    };

    // Gutter width is based on the STABLE maximum line number (scrollback
    // capacity + visible rows), not the current line count — otherwise the
    // gutter widens at 10/100/1000/10000-line thresholds as scrollback fills,
    // shifting all content horizontally (a display anomaly). Computed before
    // the mouse-handling closures because hit-testing needs it to convert
    // pixel X into a cell column.
    let max_line_num = (render_output.scrollback_capacity.max(10_000) + total_rows).max(1);
    let gutter_width = (max_line_num.ilog10() as usize + 1) + 1; // digits + 1 padding

    // ── Mouse handling: text selection (WindTerm-style) & app reporting ──
    //
    // Selection is terminal-owned (cell-coordinate based), NOT the native
    // DOM selection: the DOM selection can't distinguish wide-char boundary
    // cells, dragged-overline raggedness across the monospace grid, or the
    // line-wrap joins we reconstruct at copy time via `wrapped` flags. The
    // content div uses `user-select:none` so the two never compete.
    //
    let max_col_cached = render_output
        .rows
        .iter()
        .map(|r| r.cells.len())
        .max()
        .unwrap_or(80);
    let rows_len_cached = render_output.rows.len();
    let event_cell = move |e: &MouseEvent| -> Option<(usize, usize)> {
        let (left, top, cw, ch) = content_geo()?;
        let pt = e.client_coordinates();
        Some(event_cell_from_coords(
            pt.x,
            pt.y,
            left,
            top,
            cw,
            ch,
            rows_len_cached,
            max_col_cached,
        ))
    };

    let sel_on_input = on_input;
    let down_reporting = render_output.mode_mouse_reporting && render_output.scrollback_offset == 0;
    let down_sgr = render_output.mode_mouse_sgr;
    let on_mouse_down = move |e: MouseEvent| {
        if current_disconnected {
            return;
        }
        let button = match e.trigger_button() {
            Some(MouseButton::Primary) => 0u8,
            Some(MouseButton::Auxiliary) => 1u8,
            Some(MouseButton::Secondary) => 2u8,
            _ => return,
        };
        let mods = e.modifiers();
        let shift = mods.shift();

        if down_reporting && !shift {
            // App owns the mouse (vim `set mouse=a`, htop, tmux): forward a
            // press report and arm release tracking. Motion (1002) is only
            // reported for the primary button, but EVERY pressed button must
            // get its release report or the app sees a stuck click.
            let Some((row, col)) = event_cell(&e) else {
                return;
            };
            sel_on_input.call(encode_mouse_report(
                MouseReportKind::Press,
                button,
                col,
                row,
                shift,
                mods.alt(),
                mods.ctrl(),
                down_sgr,
            ));
            mouse_button_down.set(Some(button));
            if button == 0 {
                last_motion_cell.set(Some((row, col)));
            }
            return;
        }

        if button == 0 {
            // Local selection (Shift forces it even when the app reports).
            let Some(cell) = event_cell(&e) else { return };
            selection.set(Some(TextSelection {
                anchor: cell,
                head: cell,
            }));
            selection_text.set(String::new());
            selecting.set(true);
        } else if button == 1 {
            // Middle-click paste (xterm convention; consistent with
            // copy-on-select, the OS clipboard carries the selection).
            let bracketed = render_output.mode_bracketed_paste;
            paste_from_clipboard(&on_input, bracketed);
        }
    };

    let move_on_input = on_input;
    let move_reporting = down_reporting;
    let move_button_motion = render_output.mode_mouse_button_motion;
    let move_sgr = render_output.mode_mouse_sgr;
    let on_mouse_move = move |e: MouseEvent| {
        if selecting() {
            if let Some(ts) = selection() {
                let Some(cell) = event_cell(&e) else { return };
                if cell != ts.head {
                    selection.set(Some(TextSelection { head: cell, ..ts }));
                }
            }
            return;
        }

        // App-button-drag motion (1002): only while button 0 is held and the
        // app asked for button-event motion tracking.
        if move_reporting && move_button_motion {
            if mouse_button_down().is_some() {
                let Some((row, col)) = event_cell(&e) else {
                    return;
                };
                if last_motion_cell() != Some((row, col)) {
                    last_motion_cell.set(Some((row, col)));
                    let mods = e.modifiers();
                    move_on_input.call(encode_mouse_report(
                        MouseReportKind::Motion,
                        0,
                        col,
                        row,
                        mods.shift(),
                        mods.alt(),
                        mods.ctrl(),
                        move_sgr,
                    ));
                }
            }
        }
    };

    let up_on_input = on_input;
    let up_reporting = down_reporting;
    let up_sgr = render_output.mode_mouse_sgr;
    let up_rows = render_output.rows.clone();
    let on_mouse_up = move |e: MouseEvent| {
        if up_reporting {
            if let Some(button) = mouse_button_down.take() {
                let Some((row, col)) = event_cell(&e) else {
                    return;
                };
                let mods = e.modifiers();
                up_on_input.call(encode_mouse_report(
                    MouseReportKind::Release,
                    button,
                    col,
                    row,
                    mods.shift(),
                    mods.alt(),
                    mods.ctrl(),
                    up_sgr,
                ));
                last_motion_cell.set(None);
            }
            return;
        }

        if !selecting() {
            return;
        }
        selecting.set(false);
        let Some(ts) = selection() else { return };
        if ts.anchor == ts.head {
            // Plain click, no drag → clear any prior selection.
            selection.set(None);
            selection_text.set(String::new());
            return;
        }
        let text = extract_selection(&up_rows, ts.anchor, ts.head);
        if !text.is_empty() {
            selection_text.set(text.clone());
            // Copy-on-select (WindTerm default): the selection text is in
            // the clipboard the moment the button is released.
            copy_text_to_clipboard(text);
        }
    };

    // Double-click selects the word under the cursor (WindTerm behaviour) and
    // copies it immediately. When an application has enabled mouse reporting
    // (vim `set mouse=a`, tmux, htop) and Shift isn't held, xterm forwards the
    // double-click as a second press report and the application performs its
    // own word selection — we must NOT also run a local selection, otherwise
    // both the app and the terminal try to own the click. Shift forces local
    // word selection even under app reporting (same override as single-click
    // drag selection).
    let dbl_reporting = down_reporting;
    let dbl_sgr = render_output.mode_mouse_sgr;
    let dbl_rows = render_output.rows.clone();
    let dbl_on_input = on_input;
    let on_double_click = move |e: MouseEvent| {
        if current_disconnected {
            return;
        }
        let mods = e.modifiers();
        let shift = mods.shift();

        if dbl_reporting && !shift {
            // App owns the mouse: forward as a press report (xterm sends the
            // second click of a double-click as another press). The app does
            // its own word selection.
            let Some((row, col)) = event_cell(&e) else {
                return;
            };
            dbl_on_input.call(encode_mouse_report(
                MouseReportKind::Press,
                0,
                col,
                row,
                shift,
                mods.alt(),
                mods.ctrl(),
                dbl_sgr,
            ));
            return;
        }

        // Local word selection: expand to the maximal same-char-class run.
        let Some((row, col)) = event_cell(&e) else {
            return;
        };
        let Some(row_cells) = dbl_rows.get(row) else {
            return;
        };
        let (start_col, end_col) = word_range_in_row(&row_cells.cells, col);
        // An empty/whitespace run or a single cell is still a valid selection:
        // WindTerm selects the whitespace run too. But a degenerate range on an
        // empty row isn't worth copying.
        let anchor = (row, start_col);
        let head = (row, end_col);
        selection.set(Some(TextSelection { anchor, head }));
        selecting.set(false); // no drag follows unless a fresh mousedown starts
        let text = extract_selection(&dbl_rows, anchor, head);
        if !text.is_empty() {
            selection_text.set(text.clone());
            copy_text_to_clipboard(text);
        } else {
            selection_text.set(String::new());
        }
    };

    // Extract in screen reading order (top-left → bottom-right regardless
    // of drag direction), then produce the clipboard text —
    // `extract_selection` already normalizes endpoints, so callers pass
    // anchor/head directly.
    //
    // Wheel reporting to app-mouse modes. `wheel_report_seq` builds N
    // repeated reports for one wheel notch batch.
    let wheel_reporting = render_output.mode_mouse_reporting;
    let wheel_sgr = render_output.mode_mouse_sgr;
    #[expect(clippy::too_many_arguments, reason = "xterm mouse report fields")]
    fn wheel_report_seq(
        button: u8,
        notches: usize,
        col: usize,
        row: usize,
        shift: bool,
        alt: bool,
        ctrl: bool,
        sgr: bool,
    ) -> Vec<u8> {
        let mut buf = Vec::with_capacity(16 * notches);
        for _ in 0..notches {
            buf.extend_from_slice(&encode_mouse_report(
                MouseReportKind::Press,
                button,
                col,
                row,
                shift,
                alt,
                ctrl,
                sgr,
            ));
        }
        buf
    }

    // Poll the content div's geometry: client-rect origin + measured
    // monospace cell size. `getBoundingClientRect` must be read live — it
    // shifts whenever panes are resized/dragged or the window moves. The
    // effect re-renders only when the tuple actually changes. NOTE: dioxus
    // desktop eval wraps scripts in an AsyncFunction body, so results must be
    // returned explicitly (`return ...`), not via an IIFE expression.
    {
        let scroll_id_geo = scroll_id.clone();
        use_effect(move || {
            let scroll_id_geo = scroll_id_geo.clone();
            spawn(async move {
                loop {
                    let js = format!(
                        "var el = document.querySelector('#{scroll_id_geo} > div:last-child');\
                         if (!el) {{ return null; }}\
                         var r = el.getBoundingClientRect();\
                         var test = document.createElement('span');\
                         test.textContent = 'MMMMMMMMMM';\
                         test.style.cssText = 'font-family:JetBrains Mono,Fira Code,Cascadia Code,monospace;font-size:13px;line-height:1.5;position:absolute;visibility:hidden;white-space:pre;';\
                         document.body.appendChild(test);\
                         var tr = test.getBoundingClientRect();\
                         document.body.removeChild(test);\
                         if (tr.width <= 0 || tr.height <= 0) {{ return null; }}\
                         return r.left.toFixed(2) + ',' + r.top.toFixed(2) + ',' + (tr.width / 10).toFixed(2) + ',' + tr.height.toFixed(2);"
                    );
                    if let Ok(v) = dioxus::document::eval(&js).await {
                        if let Some(s) = v.as_str() {
                            let parts: Vec<&str> = s.split(',').collect();
                            if let (Some(l), Some(t), Some(w), Some(h)) = (
                                parts.first().and_then(|p| p.parse::<f64>().ok()),
                                parts.get(1).and_then(|p| p.parse::<f64>().ok()),
                                parts.get(2).and_then(|p| p.parse::<f64>().ok()),
                                parts.get(3).and_then(|p| p.parse::<f64>().ok()),
                            ) {
                                if w > 0.0 && h > 0.0 && content_geo() != Some((l, t, w, h)) {
                                    content_geo.set(Some((l, t, w, h)));
                                }
                            }
                        }
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
            });
        });
    }

    // Pre-render line numbers as a single HTML block (gutter column)
    let gutter_html = render_output
        .rows
        .iter()
        .enumerate()
        .map(|(row_idx, _)| {
            let line_num = line_number_start + row_idx;
            format!(
                "<div style=\"height:1.5em;line-height:1.5\">{}</div>",
                line_num
            )
        })
        .collect::<Vec<_>>()
        .join("");

    // Per-row selection ranges for the mouse-drag highlight. `usize::MAX`
    // means "to end of row" (extraction clamps per-row anyway).
    let selection_read = selection();
    let sel_range = selection_read
        .as_ref()
        .map(|ts| rusterm_core::terminal::normalize_selection(ts.anchor, ts.head));
    let sel_for_row = |row_idx: usize| -> Option<(usize, usize)> {
        let ((sr, sc), (er, ec)) = sel_range?;
        if row_idx < sr || row_idx > er {
            None
        } else {
            let a = if row_idx == sr { sc } else { 0 };
            let b = if row_idx == er { ec } else { usize::MAX };
            Some((a, b))
        }
    };

    // Pre-render content rows to HTML (no line numbers, no flex per-row)
    let row_htmls: Vec<String> = render_output
        .rows
        .iter()
        .enumerate()
        .map(|(row_idx, row)| {
            let is_cursor_row = row_idx == cursor_row && cursor_visible;
            let cur_col = if is_cursor_row {
                Some(cursor_col)
            } else {
                None
            };
            let sug = if is_cursor_row {
                suggestion.as_deref()
            } else {
                None
            };

            let sm = search_matches();
            let sidx = search_match_index();
            let is_current_match = sm.get(sidx).map(|(r, _)| *r == row_idx).unwrap_or(false);
            let is_search_match = sm.iter().any(|(r, _)| *r == row_idx);

            let row_bg = if is_current_match {
                "background:rgba(122,162,247,0.2);"
            } else if is_search_match {
                "background:rgba(122,162,247,0.08);"
            } else {
                ""
            };

            let content_html = row_to_html(row, cur_col, sug, sel_for_row(row_idx));

            let mut html = String::with_capacity(content_html.len() + 80);
            html.push_str("<div style=\"white-space:pre;line-height:1.5;");
            html.push_str(row_bg);
            if is_cursor_row {
                html.push_str("\" data-cursor-row=\"1");
            }
            html.push_str("\">");
            html.push_str(&content_html);
            html.push_str("</div>");
            html
        })
        .collect();

    rsx! {
        div {
            id: "{container_id}",
            style: "
                position: absolute;
                left: 0; right: 0; top: 0; bottom: 0;
                background: #1a1b26;
                padding: 8px 12px 4px 4px;
                overflow-y: hidden;
                font-family: 'JetBrains Mono', 'Fira Code', 'Cascadia Code', monospace;
                font-size: 13px;
                line-height: 1.5;
                color: #c0caf5;
                outline: none;
                cursor: text;
                box-sizing: border-box;
                -webkit-appearance: none;
                appearance: none;
                scrollbar-width: none;
                -ms-overflow-style: none;
            ",
            onmounted: move |_| {
                let cid = container_id.clone();
                spawn(async move {
                    let _ = dioxus::document::eval(&format!(
                        "(function() {{ const el = document.getElementById('{cid}'); if (!el) return; el.style.caretColor = 'transparent'; el.style.webkitTapHighlightColor = 'transparent'; el.addEventListener('focus', function() {{ this.style.outline = 'none'; this.style.boxShadow = 'none'; }}); if (el.dataset.rustermPointerCapture !== '1') {{ el.dataset.rustermPointerCapture = '1'; el.addEventListener('pointerdown', function(event) {{ if (typeof this.setPointerCapture === 'function') {{ try {{ this.setPointerCapture(event.pointerId); }} catch (_) {{}} }} }}); }} }})()"
                    )).await;
                });
            },
            tabindex: "0",
            onclick: onclick_focus,
            oncontextmenu: oncontextmenu_reconnect,
            onfocus: move |_| focused.set(true),
            onblur: move |_| focused.set(false),
            onkeydown: handle_keydown,
            onmousedown: on_mouse_down,
            onmousemove: on_mouse_move,
            onmouseup: on_mouse_up,
            ondoubleclick: on_double_click,
            onwheel: move |e: WheelEvent| {
                e.prevent_default();
                let v = e.delta().strip_units();
                if v.y == 0.0 {
                    return;
                }
                // While the app tracks the mouse (and we're at the live view),
                // the wheel scrolls the APP (vim, less, tmux); otherwise it
                // scrolls our local scrollback. One report per ~40px notch.
                if wheel_reporting && render_output.scrollback_offset == 0 {
                    let button: u8 = if v.y < 0.0 { 64 } else { 65 };
                    let notches = ((v.y.abs() / 40.0).ceil() as usize).max(1);
                    let Some((left, top, cw, ch)) = content_geo() else { return };
                    let pt = e.client_coordinates();
                    let max_col = render_output
                        .rows
                        .iter()
                        .map(|r| r.cells.len())
                        .max()
                        .unwrap_or(80);
                    let (row, col) = event_cell_from_coords(
                        pt.x,
                        pt.y,
                        left,
                        top,
                        cw,
                        ch,
                        render_output.rows.len(),
                        max_col,
                    );
                    let mods = e.modifiers();
                    let bytes = wheel_report_seq(
                        button,
                        notches,
                        col,
                        row,
                        mods.shift(),
                        mods.alt(),
                        mods.ctrl(),
                        wheel_sgr,
                    );
                    on_input.call(bytes);
                    return;
                }
                if v.y < 0.0 {
                    let rows = ((-v.y / 40.0).ceil() as usize).max(1);
                    on_scroll_up.call(rows);
                } else {
                    let rows = ((v.y / 40.0).ceil() as usize).max(1);
                    on_scroll_down.call(rows);
                }
            },

            // Search overlay bar
            if search_visible() {
                {
                    let query = search_query();
                    let matches = search_matches();
                    let match_idx = search_match_index();
                    let match_info = if matches.is_empty() {
                        "No matches".to_string()
                    } else {
                        format!("{}/{}", match_idx + 1, matches.len())
                    };
                    rsx! {
                        div {
                            style: "
                                position: absolute;
                                top: 0; left: 0; right: 0;
                                z-index: 10;
                                display: flex;
                                align-items: center;
                                gap: 8px;
                                padding: 6px 10px;
                                background: #24283b;
                                border-bottom: 1px solid #2a2b3d;
                                border-radius: 4px 4px 0 0;
                            ",
                            span { style: "color: #565f89; font-size: 12px; white-space: nowrap;", "Find:" }
                            input {
                                r#type: "text",
                                value: "{query}",
                                style: "
                                    flex: 1;
                                    background: #1a1b26;
                                    border: 1px solid #2a2b3d;
                                    border-radius: 3px;
                                    color: #c0caf5;
                                    padding: 3px 8px;
                                    font-size: 12px;
                                    font-family: 'JetBrains Mono', monospace;
                                    outline: none;
                                ",
                                oninput: move |e: FormEvent| {
                                    search_query.set(e.value());
                                    search_match_index.set(0);
                                },
                                onkeydown: move |e: KeyboardEvent| {
                                    e.stop_propagation();
                                    if matches!(e.key(), Key::Escape) {
                                        search_visible.set(false);
                                        search_query.set(String::new());
                                        search_matches.set(Vec::new());
                                        search_match_index.set(0);
                                    } else if matches!(e.key(), Key::Enter) {
                                        let matches = search_matches();
                                        if !matches.is_empty() {
                                            let next = (search_match_index() + 1) % matches.len();
                                            search_match_index.set(next);
                                        }
                                    }
                                },
                            }
                            span { style: "color: #565f89; font-size: 11px; white-space: nowrap; min-width: 60px; text-align: right;", "{match_info}" }
                            button {
                                style: "background:none;border:none;color:#565f89;cursor:pointer;font-size:14px;padding:0 4px;",
                                onclick: move |_| {
                                    let matches = search_matches();
                                    if !matches.is_empty() {
                                        let next = (search_match_index() + 1) % matches.len();
                                        search_match_index.set(next);
                                    }
                                },
                                "\u{25BC}"
                            }
                            button {
                                style: "background:none;border:none;color:#565f89;cursor:pointer;font-size:14px;padding:0 4px;",
                                onclick: move |_| {
                                    let matches = search_matches();
                                    if !matches.is_empty() {
                                        let prev = if search_match_index() == 0 { matches.len() - 1 } else { search_match_index() - 1 };
                                        search_match_index.set(prev);
                                    }
                                },
                                "\u{25B2}"
                            }
                            button {
                                style: "background:none;border:none;color:#565f89;cursor:pointer;font-size:14px;padding:0 4px;",
                                onclick: move |_| {
                                    search_visible.set(false);
                                    search_query.set(String::new());
                                    search_matches.set(Vec::new());
                                    search_match_index.set(0);
                                },
                                "\u{2715}"
                            }
                        }
                    }
                }
            }

            // Two-column layout: line number gutter + terminal content
            div {
                id: "{scroll_id}",
                style: "display:flex;height:100%;width:100%;",

                // Line number gutter.
                // `user-select:none` alone is insufficient on macOS WKWebView
                // (dioxus desktop via wry): WebKit needs the `-webkit-` prefix
                // to actually disable native text selection. Without it the
                // gutter numbers remain mouse-selectable, which lets a native
                // DOM selection compete with the terminal-owned (cell-based)
                // selection. The content div below uses both prefixes for the
                // same reason — keep them in sync.
                div {
                    style: "flex-shrink:0;width:{gutter_width}ch;padding-right:8px;text-align:right;color:#3b4261;user-select:none;-webkit-user-select:none;line-height:1.5;",
                    dangerous_inner_html: "{gutter_html}",
                }

                // Terminal content. Selection is terminal-owned (cell-based
                // drag → in-render highlight → copy on release), so the native
                // DOM selection must stay OFF here: `user-select:none` keeps
                // the two from competing. Re-copy with Ctrl+Shift+C / Cmd+C;
                // paste with right-click / Ctrl+Shift+V / Cmd+V.
                div {
                    style: "flex:1;min-width:0;overflow:hidden;user-select:none;-webkit-user-select:none;",

                    for (row_idx, row_html) in row_htmls.iter().enumerate() {
                        div {
                            key: "{session_id}-{row_idx}",
                            dangerous_inner_html: "{row_html}",
                        }
                    }
                }
            }

            // Scroll-position indicator: small thumb on the right edge showing
            // the visible window's relative position in scrollback+grid.
            if show_position_indicator {
                div {
                    style: "position:absolute;right:4px;top:8px;bottom:4px;width:3px;z-index:5;pointer-events:none;",
                    div {
                        style: "position:absolute;left:0;right:0;top:{thumb_top_str}%;height:{thumb_height_str}%;background:rgba(122,162,247,0.35);border-radius:2px;",
                    }
                }
            }

            // Suggestion panel (Atuin-style, positioned above the cursor line)
            if current_suggestion_visible && !current_suggestions.is_empty() {
                SuggestionPopup {
                    suggestions: current_suggestions.clone(),
                    selected_index: current_suggestion_selected,
                    on_select: move |cmd: String| {
                        on_suggestion_accept.call(cmd);
                    },
                    on_dismiss: move |_: ()| {
                        on_suggestion_dismiss.call(());
                    },
                    on_delete: move |cmd: String| {
                        on_suggestion_delete.call(cmd);
                    },
                }
            }

            if matches!(
                current_onekey_submission_feedback,
                Some(OneKeySubmissionFeedback::Submitted { .. })
            ) {
                div {
                    style: "position:absolute;right:10px;bottom:var(--suggestion-bottom, 2em);z-index:19;padding:5px 9px;background:#1a1b26;border:1px solid #9ece6a;border-radius:4px;color:#9ece6a;font-size:11px;pointer-events:none;",
                    "Credential sent · input hidden by remote"
                }
            }

            // OneKey autofill popup for this TerminalView's session. It is
            // positioned relative to this pane and grows above the cursor.
            if onekey_visible && !onekey_entries.is_empty() {
                OneKeyPopup {
                    entries: onekey_entries.clone(),
                    selected: onekey_selected.min(onekey_entries.len().saturating_sub(1)),
                    submission_feedback: onekey_submission_feedback.clone(),
                    on_highlight: move |index: usize| {
                        on_onekey_navigate.call(Some(index));
                    },
                    on_select: move |index: usize| {
                        on_onekey_select.call(index);
                    },
                    on_save: move |_: ()| {
                        on_onekey_save.call(());
                    },
                    on_dismiss: move |_: ()| {
                        on_onekey_dismiss.call(());
                    },
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ClipboardCopyOutcome, CopyShortcut, OneKeyKeyAction, TerminalOverlayKeyAction,
        TextSelection, copy_text_to_clipboard, event_cell_from_coords, onekey_popup_key_action,
        scroll_thumb_geometry, terminal_overlay_key_action, terminal_selection_text,
        word_range_in_row,
    };
    use dioxus::prelude::Key;
    use rusterm_core::terminal::{RenderCell, RenderRow};

    #[test]
    fn event_cell_maps_client_coords_to_grid() {
        // Content rect at (320, 47), cell 7.8×19.0: a pixel just inside the
        // second column of the second row maps to (row=1, col=1).
        assert_eq!(
            event_cell_from_coords(
                320.0 + 7.8 + 1.0,
                47.0 + 19.0 + 1.0,
                320.0,
                47.0,
                7.8,
                19.0,
                24,
                80
            ),
            (1, 1)
        );
        // Exactly on the origin → first cell.
        assert_eq!(
            event_cell_from_coords(320.0, 47.0, 320.0, 47.0, 7.8, 19.0, 24, 80),
            (0, 0)
        );
    }

    #[test]
    fn event_cell_clamps_outside_grid_to_edges() {
        // Left of the content div (over the gutter) → col 0, row 0
        // (negative offsets clamp, they don't wrap).
        assert_eq!(
            event_cell_from_coords(10.0, 10.0, 320.0, 47.0, 7.8, 19.0, 24, 80),
            (0, 0)
        );
        // Below the last row → last row; past the right edge → last col.
        assert_eq!(
            event_cell_from_coords(
                320.0 + 83.0 * 7.8,
                47.0 + 30.0 * 19.0,
                320.0,
                47.0,
                7.8,
                19.0,
                24,
                80
            ),
            (23, 79)
        );
    }

    // Helper: build a `Vec<RenderCell>` from a string, one cell per char.
    fn cells_from(s: &str) -> Vec<RenderCell> {
        s.chars()
            .map(|character| RenderCell {
                character,
                ..Default::default()
            })
            .collect()
    }

    fn row_from(s: &str) -> RenderRow {
        RenderRow {
            cells: cells_from(s),
            wrapped: false,
        }
    }

    // Helper: build a cell with `wide`/`wide_next` flags set, for CJK glyphs.
    fn wide_cell(c: char) -> RenderCell {
        RenderCell {
            character: c,
            wide: true,
            ..Default::default()
        }
    }
    fn wide_next_cell() -> RenderCell {
        RenderCell {
            character: '\u{0}', // placeholder — never emitted
            wide_next: true,
            ..Default::default()
        }
    }

    #[test]
    fn word_range_selects_full_word_run() {
        // "hello world" — clicking any cell of "hello" selects cols 0..=4,
        // clicking any cell of "world" selects cols 6..=10.
        let cells = cells_from("hello world");
        assert_eq!(word_range_in_row(&cells, 0), (0, 4)); // 'h'
        assert_eq!(word_range_in_row(&cells, 2), (0, 4)); // 'l'
        assert_eq!(word_range_in_row(&cells, 4), (0, 4)); // 'o'
        assert_eq!(word_range_in_row(&cells, 5), (5, 5)); // ' ' (space)
        assert_eq!(word_range_in_row(&cells, 6), (6, 10)); // 'w'
        assert_eq!(word_range_in_row(&cells, 10), (6, 10)); // 'd'
    }

    #[test]
    fn word_range_selects_punctuation_run() {
        // "foo===bar" — clicking '=' selects the three-char '===' run.
        let cells = cells_from("foo===bar");
        assert_eq!(word_range_in_row(&cells, 3), (3, 5)); // first '='
        assert_eq!(word_range_in_row(&cells, 4), (3, 5)); // middle '='
        assert_eq!(word_range_in_row(&cells, 5), (3, 5)); // last '='
        // 'f' and 'b' still select their word runs.
        assert_eq!(word_range_in_row(&cells, 0), (0, 2));
        assert_eq!(word_range_in_row(&cells, 6), (6, 8));
    }

    #[test]
    fn word_range_selects_whitespace_run() {
        // "a   b" — clicking the middle space selects the 3-space run.
        let cells = cells_from("a   b");
        assert_eq!(word_range_in_row(&cells, 1), (1, 3));
        assert_eq!(word_range_in_row(&cells, 2), (1, 3));
        assert_eq!(word_range_in_row(&cells, 3), (1, 3));
    }

    #[test]
    fn word_range_at_row_boundaries() {
        // Single word fills the whole row.
        let cells = cells_from("rust");
        assert_eq!(word_range_in_row(&cells, 0), (0, 3));
        assert_eq!(word_range_in_row(&cells, 3), (0, 3));
        // Single cell.
        let one = cells_from("x");
        assert_eq!(word_range_in_row(&one, 0), (0, 0));
    }

    #[test]
    fn word_range_clamps_out_of_range_col() {
        let cells = cells_from("ab cd");
        // col way past the end clamps to the last cell.
        assert_eq!(word_range_in_row(&cells, 99), (3, 4));
    }

    #[test]
    fn word_range_empty_row() {
        let cells: Vec<RenderCell> = vec![];
        assert_eq!(word_range_in_row(&cells, 0), (0, 0));
        assert_eq!(word_range_in_row(&cells, 5), (0, 0));
    }

    #[test]
    fn word_range_underscore_is_word_char() {
        // "my_var = 42" — clicking inside "my_var" selects the whole identifier
        // including the underscore (matches xterm/IDE convention).
        let cells = cells_from("my_var = 42");
        assert_eq!(word_range_in_row(&cells, 0), (0, 5)); // 'm'
        assert_eq!(word_range_in_row(&cells, 2), (0, 5)); // '_'
        assert_eq!(word_range_in_row(&cells, 5), (0, 5)); // 'r'
    }

    #[test]
    fn word_range_wide_char_does_not_fracture_selection() {
        // "ab中cd" where 中 occupies two cells (wide + wide_next):
        //   idx: 0='a' 1='b' 2=中(wide) 3=wide_next 4='c' 5='d'
        // Clicking the wide char (either half) selects just the wide glyph
        // (cols 2..=3), NOT the neighbouring ASCII letters.
        let cells = vec![
            RenderCell {
                character: 'a',
                ..Default::default()
            },
            RenderCell {
                character: 'b',
                ..Default::default()
            },
            wide_cell('中'),
            wide_next_cell(),
            RenderCell {
                character: 'c',
                ..Default::default()
            },
            RenderCell {
                character: 'd',
                ..Default::default()
            },
        ];
        assert_eq!(word_range_in_row(&cells, 2), (2, 3)); // clicking the wide first cell
        assert_eq!(word_range_in_row(&cells, 3), (2, 3)); // clicking the wide_next cell
        // Adjacent ASCII word runs are unaffected.
        assert_eq!(word_range_in_row(&cells, 0), (0, 1)); // "ab"
        assert_eq!(word_range_in_row(&cells, 4), (4, 5)); // "cd"
    }

    #[test]
    fn word_range_adjacent_wide_chars_same_class_extend() {
        // "世界" — two adjacent CJK glyphs (each wide + wide_next), both
        // Punct class, so the run extends across both glyphs (cols 0..=3).
        let cells = vec![
            wide_cell('世'),
            wide_next_cell(),
            wide_cell('界'),
            wide_next_cell(),
        ];
        assert_eq!(word_range_in_row(&cells, 0), (0, 3));
        assert_eq!(word_range_in_row(&cells, 1), (0, 3));
        assert_eq!(word_range_in_row(&cells, 2), (0, 3));
        assert_eq!(word_range_in_row(&cells, 3), (0, 3));
    }

    #[test]
    fn empty_copy_is_rejected_before_native_clipboard_access() {
        assert_eq!(
            copy_text_to_clipboard(String::new()),
            ClipboardCopyOutcome::SkippedEmpty
        );
    }

    #[test]
    fn terminal_selection_recomputes_empty_mouseup_cache_for_error_output() {
        let rows = vec![
            row_from("error: permission denied"),
            row_from("caused by: 无权限"),
        ];
        let selection = TextSelection {
            anchor: (1, rows[1].cells.len() - 1),
            head: (0, 0),
        };

        assert_eq!(
            terminal_selection_text("", Some(selection), &rows),
            "error: permission denied\ncaused by: 无权限"
        );
    }

    #[test]
    fn terminal_selection_recompute_preserves_wide_characters() {
        let rows = vec![RenderRow {
            cells: vec![
                wide_cell('错'),
                wide_next_cell(),
                wide_cell('误'),
                wide_next_cell(),
            ],
            wrapped: false,
        }];
        let selection = TextSelection {
            anchor: (0, 0),
            head: (0, 3),
        };

        assert_eq!(terminal_selection_text("", Some(selection), &rows), "错误");
    }

    #[test]
    fn terminal_selection_cache_and_panes_remain_independent() {
        let old_rows = vec![row_from("new output")];
        let first_rows = vec![row_from("pane one error")];
        let second_rows = vec![row_from("pane two warning")];
        let first_selection = TextSelection {
            anchor: (0, 0),
            head: (0, first_rows[0].cells.len() - 1),
        };
        let second_selection = TextSelection {
            anchor: (0, 0),
            head: (0, second_rows[0].cells.len() - 1),
        };

        assert_eq!(
            terminal_selection_text(
                "captured before output shifted",
                Some(first_selection),
                &old_rows
            ),
            "captured before output shifted"
        );
        assert_eq!(
            terminal_selection_text("", Some(first_selection), &first_rows),
            "pane one error"
        );
        assert_eq!(
            terminal_selection_text("", Some(second_selection), &second_rows),
            "pane two warning"
        );
    }

    #[test]
    fn copy_shortcuts_take_priority_while_onekey_popup_is_visible() {
        let key = Key::Character("c".into());

        assert_eq!(
            terminal_overlay_key_action(&key, false, false, true, false, true, 0, 1),
            TerminalOverlayKeyAction::Copy(CopyShortcut::Command)
        );
        assert_eq!(
            terminal_overlay_key_action(&key, true, false, false, true, true, 0, 1),
            TerminalOverlayKeyAction::Copy(CopyShortcut::CtrlShift)
        );

        let rows = vec![row_from("permission denied")];
        let selection = TextSelection {
            anchor: (0, 0),
            head: (0, rows[0].cells.len() - 1),
        };
        assert_eq!(
            terminal_selection_text("", Some(selection), &rows),
            "permission denied"
        );
    }

    #[test]
    fn non_copy_key_still_routes_to_visible_onekey_popup() {
        assert_eq!(
            terminal_overlay_key_action(
                &Key::Character("x".into()),
                false,
                false,
                false,
                false,
                true,
                0,
                1,
            ),
            TerminalOverlayKeyAction::OneKey(OneKeyKeyAction::DismissAndForward)
        );
    }

    #[test]
    fn onekey_popup_navigates_with_arrows_and_wraps() {
        assert_eq!(
            onekey_popup_key_action(&Key::ArrowDown, 0, 3),
            OneKeyKeyAction::Navigate(1)
        );
        assert_eq!(
            onekey_popup_key_action(&Key::ArrowDown, 2, 3),
            OneKeyKeyAction::Navigate(0)
        );
        assert_eq!(
            onekey_popup_key_action(&Key::ArrowUp, 0, 3),
            OneKeyKeyAction::Navigate(2)
        );
        assert_eq!(
            onekey_popup_key_action(&Key::ArrowUp, 2, 3),
            OneKeyKeyAction::Navigate(1)
        );
    }

    #[test]
    fn onekey_popup_tab_and_enter_select_escape_dismisses() {
        assert_eq!(
            onekey_popup_key_action(&Key::Tab, 0, 3),
            OneKeyKeyAction::Select
        );
        assert_eq!(
            onekey_popup_key_action(&Key::Enter, 0, 3),
            OneKeyKeyAction::Select
        );
        assert_eq!(
            onekey_popup_key_action(&Key::Escape, 0, 3),
            OneKeyKeyAction::Dismiss
        );
    }

    #[test]
    fn onekey_popup_handles_empty_and_out_of_range_selection() {
        assert_eq!(
            onekey_popup_key_action(&Key::Enter, 9, 3),
            OneKeyKeyAction::Select
        );
        assert_eq!(
            onekey_popup_key_action(&Key::ArrowDown, 9, 3),
            OneKeyKeyAction::Navigate(0)
        );
        assert_eq!(
            onekey_popup_key_action(&Key::Enter, 0, 0),
            OneKeyKeyAction::Dismiss
        );
    }

    #[test]
    fn onekey_popup_dismisses_and_forwards_when_user_types() {
        // The bug this guards: while the popup is visible, a typed character
        // (or Backspace) must dismiss the popup and fall through to the PTY.
        // Otherwise the popup stays open, hijacks the next Enter, and its saved
        // value gets concatenated onto the manually-typed credential — which is
        // exactly how a correct password ends up "Access denied".
        assert_eq!(
            onekey_popup_key_action(&Key::Character("x".into()), 0, 3),
            OneKeyKeyAction::DismissAndForward
        );
        assert_eq!(
            onekey_popup_key_action(&Key::Backspace, 0, 3),
            OneKeyKeyAction::DismissAndForward
        );
    }

    #[test]
    fn selection_highlight_wraps_cells_between_endpoints_only() {
        use super::{SELECTION_BG, row_to_html};
        let mk = |s: &str| RenderRow {
            cells: s
                .chars()
                .map(|character| RenderCell {
                    character,
                    ..Default::default()
                })
                .collect(),
            wrapped: false,
        };
        let row = mk("hello world");
        // Columns 6..=10 = "world".
        let html = row_to_html(&row, None, None, Some((6, 10)));
        assert!(
            html.contains(SELECTION_BG),
            "selection highlight style must be present: {html}"
        );
        // "hello " stays unhighlighted; "world" is inside a highlight span.
        let sel_span_start = html.find(SELECTION_BG).unwrap();
        let span_close = html[sel_span_start..].find("</span>").unwrap() + sel_span_start;
        let span_body = &html[sel_span_start..span_close];
        assert!(
            span_body.contains("world"),
            "highlight covers 'world': {html}"
        );
        assert!(
            !span_body.contains("hello"),
            "highlight must not cover cells before the start column: {html}"
        );
        // No selection → no highlight style anywhere.
        let plain = row_to_html(&row, None, None, None);
        assert!(!plain.contains(SELECTION_BG));
    }

    #[test]
    fn scroll_thumb_rests_at_bottom_and_rises_when_scrolling() {
        // No scrollback → indicator hidden.
        assert_eq!(scroll_thumb_geometry(0, 0, 24), (false, 0.0, 100.0));

        // 100 scrollback, 24 visible, at the bottom (offset 0): thumb near the
        // bottom. top = scrollback/(scrollback+visible), height = visible/total.
        let (vis, top, height) = scroll_thumb_geometry(100, 0, 24);
        assert!(vis);
        assert!((height - (24.0 / 124.0 * 100.0)).abs() < 0.01);
        assert!((top - (100.0 / 124.0 * 100.0)).abs() < 0.01);

        // Scrolled all the way up (offset == scrollback) → thumb at the top.
        let (vis, top, _) = scroll_thumb_geometry(100, 100, 24);
        assert!(vis);
        assert!(top.abs() < 0.01);

        // Tiny scrollback: thumb height clamped to >= 5% so it stays visible.
        let (_, _, height) = scroll_thumb_geometry(1, 0, 24);
        assert!(height >= 5.0);
    }
}
