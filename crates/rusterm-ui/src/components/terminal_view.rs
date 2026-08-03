use dioxus::prelude::*;
// `MouseButton` lives in `dioxus::html::input_data` (not re-exported by
// dioxus::prelude) — same import pattern as tab_bar.rs.
use dioxus::html::input_data::MouseButton;

use rusterm_core::config::{KeybindingAction, Keybindings};
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

/// Finalize a drag at the button-release cell. Browsers may coalesce or omit
/// the last mousemove before mouseup, so the cached head can otherwise stop
/// short of the position where the user actually released the pointer.
fn finalize_selection_on_mouse_up(
    selection: TextSelection,
    release_cell: Option<(usize, usize)>,
) -> TextSelection {
    release_cell.map_or(selection, |head| TextSelection { head, ..selection })
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
        vec![0x1b, 0x5b, final_byte]
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

fn terminal_key_bytes(
    key: &Key,
    code: &Code,
    ctrl: bool,
    alt: bool,
    shift: bool,
    app_cursor: bool,
) -> Vec<u8> {
    let modifier = match (ctrl, alt, shift) {
        (false, false, false) => None,
        (false, false, true) => Some(2),
        (false, true, false) => Some(3),
        (false, true, true) => Some(4),
        (true, false, false) => Some(5),
        (true, false, true) => Some(6),
        (true, true, false) => Some(7),
        (true, true, true) => Some(8),
    };

    if matches!(key, Key::Unidentified) {
        let numpad_navigation = match code {
            Code::Numpad8 => Some(cursor_key_seq(1, b'A', app_cursor, modifier)),
            Code::Numpad2 => Some(cursor_key_seq(1, b'B', app_cursor, modifier)),
            Code::Numpad6 => Some(cursor_key_seq(1, b'C', app_cursor, modifier)),
            Code::Numpad4 => Some(cursor_key_seq(1, b'D', app_cursor, modifier)),
            Code::Numpad7 => Some(csi_seq(1, modifier, b'H')),
            Code::Numpad1 => Some(csi_seq(1, modifier, b'F')),
            Code::Numpad0 => Some(csi_seq(2, modifier, b'~')),
            Code::NumpadDecimal => Some(csi_seq(3, modifier, b'~')),
            Code::Numpad9 => Some(csi_seq(5, modifier, b'~')),
            Code::Numpad3 => Some(csi_seq(6, modifier, b'~')),
            _ => None,
        };
        if let Some(data) = numpad_navigation {
            return data;
        }
    }

    match key {
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

        Key::Character(s) if ctrl && !alt && !shift => ctrl_char(s),
        Key::Character(s) if alt && !ctrl => {
            let mut buf = vec![0x1b];
            buf.extend_from_slice(s.as_bytes());
            buf
        }
        Key::Character(s) if ctrl && shift && !alt => {
            let c = s.chars().next().unwrap_or('A');
            if c.is_ascii_alphabetic() {
                csi_seq(1, Some(6), c as u8)
            } else {
                csi_seq(1, Some(6), code_to_char(code))
            }
        }
        Key::Character(s) if ctrl && alt && !shift => {
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

        Key::Character(s) => s.as_bytes().to_vec(),
        _ => vec![],
    }
}

fn accepts_inline_suggestion(key: &Key, ctrl: bool, alt: bool, meta: bool, shift: bool) -> bool {
    !ctrl && !alt && !meta && !shift && matches!(key, Key::End | Key::Tab)
}

fn is_history_completion_shortcut(
    key: &Key,
    ctrl: bool,
    alt: bool,
    meta: bool,
    shift: bool,
) -> bool {
    alt && !ctrl
        && !meta
        && !shift
        && matches!(key, Key::Character(value) if value.eq_ignore_ascii_case("r"))
}

fn accepts_history_completion(key: &Key, ctrl: bool, alt: bool, meta: bool, shift: bool) -> bool {
    !ctrl && !alt && !meta && !shift && matches!(key, Key::Enter | Key::End | Key::Tab)
}

fn suggestion_navigation_index(
    key: &Key,
    ctrl: bool,
    alt: bool,
    meta: bool,
    shift: bool,
    selected: usize,
    suggestion_count: usize,
) -> Option<usize> {
    if suggestion_count == 0 || !ctrl || alt || meta || shift {
        return None;
    }

    match key {
        Key::Character(value) if value.eq_ignore_ascii_case("n") => {
            Some((selected + 1) % suggestion_count)
        }
        Key::Character(value) if value.eq_ignore_ascii_case("p") => Some(
            selected
                .checked_sub(1)
                .unwrap_or(suggestion_count.saturating_sub(1)),
        ),
        _ => None,
    }
}

/// What the OneKey autofill popup should do with a key while it is visible.
/// Extracted as a pure function so the routing — especially "typing dismisses
/// the popup and falls through to the PTY" — is unit-testable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OneKeyKeyAction {
    /// Send the selected entry's value + Enter (autofill).
    Select,
    /// Close the popup without sending anything (Escape).
    Dismiss,
    /// Keep a password popup visible while forwarding the key to the PTY.
    Forward,
    /// Close a non-password popup and forward the key to the PTY.
    DismissAndForward,
}

/// Decide what the OneKey popup does for `key` while visible. Arrow keys always
/// belong to the terminal. Password popups are persistent: focus changes and
/// forwarded input cannot hide them; only an explicit cancel or submission can.
fn onekey_popup_key_action(
    key: &Key,
    len: usize,
    requires_explicit_cancel: bool,
) -> OneKeyKeyAction {
    if len == 0 {
        return OneKeyKeyAction::Dismiss;
    }
    match key {
        // Both Enter and Tab confirm the highlighted credential. The caller
        // returns immediately after Select, so the Enter used for confirmation
        // cannot fall through and reach the PTY a second time.
        Key::Enter | Key::Tab => OneKeyKeyAction::Select,
        Key::Escape => OneKeyKeyAction::Dismiss,
        _ if requires_explicit_cancel => OneKeyKeyAction::Forward,
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
    _onekey_selected: usize,
    onekey_len: usize,
    onekey_requires_explicit_cancel: bool,
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
            onekey_len,
            onekey_requires_explicit_cancel,
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
    let mut fg_css = color_to_css(fg);
    let mut bg_css = color_to_css(bg);

    if flags.contains(CellFlags::INVERSE) {
        // A default color needs an explicit value after swapping; otherwise the
        // browser would inherit the original terminal foreground/background.
        if fg_css.is_empty() {
            fg_css = "#c0caf5".to_string();
        }
        if bg_css.is_empty() {
            bg_css = "#1a1b26".to_string();
        }
        std::mem::swap(&mut fg_css, &mut bg_css);
    }

    if !fg_css.is_empty() {
        parts.push(format!("color:{fg_css}"));
    }
    if !bg_css.is_empty() {
        parts.push(format!("background:{bg_css}"));
    }
    if flags.contains(CellFlags::BOLD) {
        parts.push("font-weight:700".to_string());
    }
    if flags.contains(CellFlags::DIM) {
        parts.push("opacity:0.65".to_string());
    }
    if flags.contains(CellFlags::ITALIC) {
        parts.push("font-style:italic".to_string());
    }

    let mut decorations = Vec::new();
    if flags.contains(CellFlags::UNDERLINE) || flags.contains(CellFlags::DOUBLE_UNDERLINE) {
        decorations.push("underline");
    }
    if flags.contains(CellFlags::STRIKETHROUGH) {
        decorations.push("line-through");
    }
    if !decorations.is_empty() {
        parts.push(format!("text-decoration-line:{}", decorations.join(" ")));
    }
    if flags.contains(CellFlags::DOUBLE_UNDERLINE) {
        parts.push("text-decoration-style:double".to_string());
    }
    if flags.contains(CellFlags::HIDDEN) {
        parts.push("color:transparent".to_string());
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SearchMatch {
    row: usize,
    start_col: usize,
    end_col: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum CellOverlay {
    #[default]
    None,
    SearchMatch,
    CurrentSearchMatch,
    Selection,
}

const SEARCH_MATCH_BG: &str = "background:rgba(224,175,104,0.26)";
const SEARCH_CURRENT_BG: &str = "background:rgba(122,162,247,0.48);outline:1px solid rgba(192,202,245,0.72);outline-offset:-1px";

fn overlay_for_col(
    col: usize,
    selection: Option<(usize, usize)>,
    search_ranges: &[(usize, usize, bool)],
) -> CellOverlay {
    if selection.is_some_and(|(start, end)| col >= start && col <= end) {
        return CellOverlay::Selection;
    }

    if search_ranges
        .iter()
        .any(|(start, end, current)| *current && col >= *start && col <= *end)
    {
        CellOverlay::CurrentSearchMatch
    } else if search_ranges
        .iter()
        .any(|(start, end, _)| col >= *start && col <= *end)
    {
        CellOverlay::SearchMatch
    } else {
        CellOverlay::None
    }
}

fn overlay_style(overlay: CellOverlay) -> &'static str {
    match overlay {
        CellOverlay::None => "",
        CellOverlay::SearchMatch => SEARCH_MATCH_BG,
        CellOverlay::CurrentSearchMatch => SEARCH_CURRENT_BG,
        CellOverlay::Selection => SELECTION_BG,
    }
}

/// Find case-insensitive matches while preserving terminal cell columns.
///
/// Matching through a folded `Vec<char>` avoids confusing UTF-8 byte offsets
/// with grid columns. Each folded character retains the source glyph's cell
/// range, so CJK/wide cells and lowercase expansions are highlighted correctly.
fn find_search_matches(rows: &[RenderRow], query: &str) -> Vec<SearchMatch> {
    let needle: Vec<char> = query.chars().flat_map(char::to_lowercase).collect();
    if needle.is_empty() {
        return Vec::new();
    }

    let mut matches = Vec::new();
    for (row_idx, row) in rows.iter().enumerate() {
        let mut folded = Vec::new();
        let mut source_cols = Vec::new();
        for (col, cell) in row.cells.iter().enumerate() {
            if cell.wide_next {
                continue;
            }
            let end_col = if cell.wide {
                (col + 1).min(row.cells.len().saturating_sub(1))
            } else {
                col
            };
            for ch in cell.character.to_lowercase() {
                folded.push(ch);
                source_cols.push((col, end_col));
            }
        }

        if needle.len() > folded.len() {
            continue;
        }
        for start in 0..=folded.len() - needle.len() {
            if folded[start..start + needle.len()] == needle {
                matches.push(SearchMatch {
                    row: row_idx,
                    start_col: source_cols[start].0,
                    end_col: source_cols[start + needle.len() - 1].1,
                });
            }
        }
    }
    matches
}

fn search_query_from_selection(
    cached: &str,
    selection: Option<TextSelection>,
    rows: &[RenderRow],
) -> Option<String> {
    let text = terminal_selection_text(cached, selection, rows);
    let text = text.trim();
    (!text.is_empty() && !text.contains(['\r', '\n']) && text.chars().count() <= 256)
        .then(|| text.to_owned())
}

fn percent_encode_query(text: &str) -> String {
    let mut encoded = String::with_capacity(text.len());
    for byte in text.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(*byte as char);
            }
            _ => {
                use std::fmt::Write as _;
                let _ = write!(encoded, "%{byte:02X}");
            }
        }
    }
    encoded
}

fn online_search_url(text: &str) -> Option<String> {
    let text = text.trim();
    (!text.is_empty()).then(|| {
        format!(
            "https://www.google.com/search?q={}",
            percent_encode_query(text)
        )
    })
}

fn open_online_search(text: &str) {
    let Some(url) = online_search_url(text) else {
        return;
    };

    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(&url).spawn();
    #[cfg(target_os = "windows")]
    let result = std::process::Command::new("rundll32")
        .args(["url.dll,FileProtocolHandler", &url])
        .spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let result = std::process::Command::new("xdg-open").arg(&url).spawn();
    #[cfg(not(any(unix, target_os = "windows")))]
    let result: std::io::Result<std::process::Child> = Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "opening URLs is unsupported on this platform",
    ));

    match result {
        Ok(mut child) => {
            // Reap the short-lived platform launcher without blocking the UI.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(error) => tracing::warn!("[SEARCH] failed to open browser: {error}"),
    }
}

/// Application mouse tracking owns live-view pointer events unless Shift is
/// held. Scrolled-back content is always local because its coordinates no
/// longer correspond to the application's current screen.
fn app_owns_mouse(mouse_reporting: bool, scrollback_offset: usize, shift: bool) -> bool {
    mouse_reporting && scrollback_offset == 0 && !shift
}

fn is_find_shortcut(
    key: &Key,
    ctrl: bool,
    alt: bool,
    meta: bool,
    shift: bool,
    is_macos: bool,
) -> bool {
    let find_key = matches!(key, Key::Character(s) if s.eq_ignore_ascii_case("f"));
    let platform_find = if is_macos {
        meta && !ctrl && !shift
    } else {
        ctrl && !meta && !shift
    };
    let legacy_find = ctrl && shift && !meta;
    !alt && find_key && (platform_find || legacy_find)
}

/// Render a terminal row to an HTML string. Uses `dangerous_inner_html`
/// for fast DOM updates — avoids Dioxus per-span VDOM diffing overhead.
///
/// When a suggestion is shown, we only render cells up to the cursor position,
/// then append the suggestion right after it. Cells after the cursor are
/// typically empty spaces and would push the suggestion to the end of the row.
///
/// `sel` is the inclusive cell-column range `(start, end)` inside the active
/// mouse selection for this row. `search_ranges` contains exact match ranges;
/// selection takes precedence over the current match, then other matches.
fn row_to_html(
    row: &RenderRow,
    cursor_col: Option<usize>,
    cursor_color: &CellColor,
    suggestion: Option<&str>,
    sel: Option<(usize, usize)>,
    search_ranges: &[(usize, usize, bool)],
) -> String {
    let mut html = String::with_capacity(row.cells.len() * 4);

    let mut cur_fg = CellColor::Default;
    let mut cur_bg = CellColor::Default;
    let mut cur_flags = CellFlags::empty();
    let mut cur_overlay = CellOverlay::None;
    let mut cur_text = String::new();

    let flush = |html: &mut String,
                 text: &str,
                 fg: &CellColor,
                 bg: &CellColor,
                 flags: CellFlags,
                 overlay: CellOverlay| {
        if text.is_empty() {
            return;
        }
        let mut style = cell_style(fg, bg, flags);
        let overlay_css = overlay_style(overlay);
        if !overlay_css.is_empty() {
            if !style.is_empty() {
                style.push(';');
            }
            style.push_str(overlay_css);
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
            flush(
                &mut html,
                &cur_text,
                &cur_fg,
                &cur_bg,
                cur_flags,
                cur_overlay,
            );
            cur_text.clear();

            let ch = if cell.character == ' ' {
                "&nbsp;"
            } else {
                &html_escape(&cell.character.to_string())
            };
            let base_style = cell_style(&cell.fg, &cell.bg, cell.flags);
            let cursor_border = color_to_css(cursor_color);
            let cursor_border = if cursor_border.is_empty() {
                "#c0caf5"
            } else {
                &cursor_border
            };
            let cursor_overlay = overlay_for_col(i, sel, search_ranges);
            let overlay_css = overlay_style(cursor_overlay);
            let cursor_style = if base_style.is_empty() {
                format!("{overlay_css};border-left:2px solid {cursor_border};margin-left:-1px")
            } else {
                format!(
                    "{};{};border-left:2px solid {};margin-left:-1px",
                    base_style, overlay_css, cursor_border
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
            cur_overlay = CellOverlay::None;
            continue;
        }

        let overlay = overlay_for_col(i, sel, search_ranges);
        let same_style = cell.fg == cur_fg
            && cell.bg == cur_bg
            && cell.flags == cur_flags
            && overlay == cur_overlay;
        if !cur_text.is_empty() && !same_style {
            flush(
                &mut html,
                &cur_text,
                &cur_fg,
                &cur_bg,
                cur_flags,
                cur_overlay,
            );
            cur_text.clear();
        }

        cur_fg = cell.fg.clone();
        cur_bg = cell.bg.clone();
        cur_flags = cell.flags;
        cur_overlay = overlay;
        cur_text.push(cell.character);
    }

    flush(
        &mut html,
        &cur_text,
        &cur_fg,
        &cur_bg,
        cur_flags,
        cur_overlay,
    );

    // Insert suggestion right after the cursor content
    if let Some(sug) = suggestion {
        html.push_str("<span style=\"color:#565f89;opacity:0.55\">");
        html.push_str(&html_escape(sug));
        html.push_str("</span>");
    }

    html
}

// ── TerminalView component ─────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PopupDirection {
    Above,
    Below,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PopupLayout {
    direction: PopupDirection,
    max_height_px: u32,
}

/// Choose the cursor side that can fit the popup, falling back to the larger
/// side and constraining the popup so its own scroll area handles overflow.
fn popup_layout(space_above: f64, space_below: f64, desired_height: f64) -> PopupLayout {
    let space_above = space_above.max(0.0);
    let space_below = space_below.max(0.0);
    let direction = if space_below >= desired_height || space_below >= space_above {
        PopupDirection::Below
    } else {
        PopupDirection::Above
    };
    let available = match direction {
        PopupDirection::Above => space_above,
        PopupDirection::Below => space_below,
    };
    PopupLayout {
        direction,
        max_height_px: available.floor().max(1.0) as u32,
    }
}

#[component]
pub fn TerminalView(
    session_id: String,
    render_output: RenderOutput,
    version: u64,
    suggestion: Option<String>,
    suggestions: Vec<String>,
    #[props(default)] suggestion_corrections: Vec<String>,
    suggestion_selected: usize,
    suggestion_visible: bool,
    /// True while the explicit Alt+R history picker is open. In this mode,
    /// Enter replaces the current command line instead of executing it.
    history_completion_visible: bool,
    #[props(default)] keybindings: Keybindings,
    #[props(default)] on_keybinding: EventHandler<KeybindingAction>,
    on_input: EventHandler<Vec<u8>>,
    on_command: EventHandler<String>,
    on_resize: EventHandler<(u16, u16, u32, u32)>,
    on_scroll_up: EventHandler<usize>,
    on_scroll_down: EventHandler<usize>,
    on_scroll_to_bottom: EventHandler<()>,
    on_suggestion_navigate: EventHandler<Option<usize>>,
    on_suggestion_accept: EventHandler<String>,
    on_history_completion: EventHandler<()>,
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
    /// Password prompts remain visible across focus changes and forwarded keys.
    onekey_requires_explicit_cancel: bool,
    onekey_submission_feedback: Option<OneKeySubmissionFeedback>,
    on_onekey_navigate: EventHandler<Option<usize>>,
    on_onekey_select: EventHandler<usize>,
    on_onekey_save: EventHandler<()>,
    on_onekey_dismiss: EventHandler<()>,
    on_focus_lost: EventHandler<()>,
    /// True when the session's SSH/shell channel has dropped. While set, Enter
    /// triggers `on_reconnect` and all other keys are ignored (no live PTY).
    disconnected: bool,
    on_reconnect: EventHandler<()>,
    /// Per-row diff status for comparison-mode highlighting. `None` when
    /// comparison mode is off (no highlight). When `Some`, rows marked
    /// [`RowDiff::Different`] get a muted red background so the user can
    /// spot where outputs diverge across panes.
    row_diffs: Option<Vec<crate::comparison::RowDiff>>,
    /// Maximum number of suggestion rows to display (from the user's
    /// configured `suggestion_count`). Passed through to `SuggestionPopup`.
    #[props(default)]
    suggestion_max_rows: usize,
) -> Element {
    let _lang = crate::i18n::LANGUAGE();
    let mut focused = use_signal(|| false);
    let mut search_visible = use_signal(|| false);
    let mut search_query = use_signal(String::new);
    let mut search_match_index = use_signal(|| 0usize);
    let mut search_matches: Signal<Vec<SearchMatch>> = use_signal(Vec::new);
    let mut search_highlight_pinned = use_signal(|| false);

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
    let current_suggestion_corrections = suggestion_corrections.clone();
    let current_suggestion_visible = suggestion_visible;
    let current_suggestion_selected = suggestion_selected;
    let current_history_completion_visible = history_completion_visible;

    let current_onekey_visible = onekey_visible;
    // Cap the entries used for selection and rendering to MAX_VISIBLE_ROWS so
    // the selected index always addresses a visible popup item. Arrow keys are
    // reserved for terminal cursor/history movement.
    let current_onekey_entries: Vec<OneKeyMatch> = onekey_entries
        .iter()
        .take(crate::components::suggestion_popup::MAX_VISIBLE_ROWS)
        .cloned()
        .collect();
    // Snapshot the count for the keydown closure (which is a `move` closure).
    // This avoids moving `current_onekey_entries` into the closure, leaving it
    // available for the rsx! rendering block below.
    let current_onekey_len = current_onekey_entries.len();
    let current_onekey_selected = onekey_selected;
    let current_onekey_requires_explicit_cancel = onekey_requires_explicit_cancel;
    let current_onekey_submission_feedback = onekey_submission_feedback.clone();
    let current_disconnected = disconnected;

    let closure_suggestions = current_suggestions.clone();
    let closure_suggestion_count = closure_suggestions.len().min(if suggestion_max_rows == 0 {
        crate::components::suggestion_popup::MAX_VISIBLE_ROWS
    } else {
        suggestion_max_rows
    });
    let closure_suggestion_corrections = current_suggestion_corrections.clone();
    let sid_for_keydown_log = session_id.clone();
    let sid_for_copy = session_id.clone();
    let search_input_id = format!("terminal-search-{session_id}");
    let search_input_id_for_keydown = search_input_id.clone();
    let copy_rows = render_output.rows.clone();
    let handle_keydown = move |e: KeyboardEvent| {
        let key = e.key();
        let code = e.code();
        let mods = e.modifiers();
        let ctrl = mods.ctrl();
        let alt = mods.alt();
        let meta = mods.meta();
        let shift = mods.shift();
        // trace (not info): fires on every keydown; info-level logging of
        // every key would dominate the trace output during fast typing and
        // add measurable overhead in debug builds.
        tracing::trace!(
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
                    e.stop_propagation();
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
            current_onekey_len,
            current_onekey_requires_explicit_cancel,
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
                e.stop_propagation();
                return;
            }
            TerminalOverlayKeyAction::Copy(CopyShortcut::CtrlShift) => {
                e.prevent_default();
                e.stop_propagation();
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

        // Standard terminal find shortcuts. Cmd+F is used on macOS; Ctrl+F is
        // used on other desktop platforms. Keep Ctrl+Shift+F as the established
        // RusTerm shortcut. Opening find seeds the query from a local terminal
        // selection when possible (the "find selection" workflow).
        let find_shortcut =
            is_find_shortcut(&key, ctrl, alt, meta, shift, cfg!(target_os = "macos"));
        if find_shortcut {
            e.prevent_default();
            e.stop_propagation();
            if let Some(selected) =
                search_query_from_selection(&selection_text(), selection(), &copy_rows)
            {
                if search_query() != selected {
                    search_query.set(selected);
                    search_match_index.set(0);
                }
            }
            search_visible.set(true);
            let input_id = search_input_id_for_keydown.clone();
            spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                let _ = dioxus::document::eval(&format!(
                    "const el=document.getElementById('{}'); if(el){{el.focus();el.select();}}",
                    input_id
                ))
                .await;
            });
            return;
        }

        // Screenshot-compatible navigation: Alt+F3 moves through the current
        // query; Cmd+F3 first replaces the query with the terminal selection.
        if matches!(key, Key::F3) && (alt || meta) {
            e.prevent_default();
            e.stop_propagation();
            if meta {
                if let Some(selected) =
                    search_query_from_selection(&selection_text(), selection(), &copy_rows)
                {
                    let found = find_search_matches(&copy_rows, &selected);
                    search_query.set(selected);
                    search_matches.set(found.clone());
                    search_match_index.set(if shift && !found.is_empty() {
                        found.len() - 1
                    } else {
                        0
                    });
                }
            } else {
                let matches = search_matches();
                if !matches.is_empty() {
                    let current = search_match_index().min(matches.len() - 1);
                    search_match_index.set(if shift {
                        current.checked_sub(1).unwrap_or(matches.len() - 1)
                    } else {
                        (current + 1) % matches.len()
                    });
                }
            }
            return;
        }

        if let Some(action) =
            crate::keybindings::action_for_event(&keybindings, &key, ctrl, alt, meta, shift)
        {
            e.prevent_default();
            e.stop_propagation();
            on_keybinding.call(action);
            return;
        }

        if meta {
            return;
        }
        e.prevent_default();
        // A focused terminal owns non-Command keyboard input. Prevent the root
        // application handler from also acting on bytes already sent to the PTY
        // (notably Ctrl+W, which would otherwise delete a word and close a pane).
        e.stop_propagation();

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
                OneKeyKeyAction::Select => {
                    let selected =
                        current_onekey_selected.min(current_onekey_len.saturating_sub(1));
                    on_onekey_select.call(selected);
                    return;
                }
                OneKeyKeyAction::Dismiss => {
                    on_onekey_dismiss.call(());
                    return;
                }
                OneKeyKeyAction::Forward => {
                    // Password prompts are persistent. Forward terminal input,
                    // but keep the popup until Escape/× or credential submission.
                }
                OneKeyKeyAction::DismissAndForward => {
                    on_onekey_dismiss.call(());
                }
            }
        }

        if search_visible() {
            if matches!(key, Key::Enter) {
                let matches = search_matches();
                if !matches.is_empty() {
                    let current = search_match_index().min(matches.len() - 1);
                    let next = if shift {
                        current.checked_sub(1).unwrap_or(matches.len() - 1)
                    } else {
                        (current + 1) % matches.len()
                    };
                    search_match_index.set(next);
                }
                return;
            }
            if matches!(key, Key::Escape) {
                search_visible.set(false);
                if !search_highlight_pinned() {
                    search_query.set(String::new());
                    search_matches.set(Vec::new());
                    search_match_index.set(0);
                }
                return;
            }
            return;
        }

        if is_history_completion_shortcut(&key, ctrl, alt, meta, shift) {
            on_history_completion.call(());
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
            if let Some(next) = suggestion_navigation_index(
                &key,
                ctrl,
                alt,
                meta,
                shift,
                current_suggestion_selected,
                closure_suggestion_count,
            ) {
                on_suggestion_navigate.call(Some(next));
                return;
            }
            match &key {
                _ if current_history_completion_visible
                    && accepts_history_completion(&key, ctrl, alt, meta, shift) =>
                {
                    if let Some(cmd) = closure_suggestions.get(current_suggestion_selected) {
                        on_suggestion_accept.call(cmd.clone());
                    }
                    return;
                }
                // Arrow keys dismiss the panel and fall through to the PTY
                // so the shell handles cursor movement / history navigation.
                Key::ArrowDown | Key::ArrowUp | Key::ArrowLeft | Key::ArrowRight => {
                    on_suggestion_dismiss.call(());
                    // Don't return — let the key continue to the PTY.
                }
                Key::Tab | Key::End if !ctrl && !alt && !meta && !shift => {
                    // Unmodified Tab/End accepts the selected suggestion.
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
                    if let Some(cmd) = closure_suggestions.get(current_suggestion_selected)
                        && !closure_suggestion_corrections.contains(cmd)
                    {
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

        // ── Auto-completion: accept inline suggestion with End/Tab ──
        //
        // ArrowRight is intentionally excluded — in a terminal, Right moves
        // the cursor forward within the command line. If an inline ghost-text
        // suggestion were accepted on Right, the user could never move the
        // cursor rightward without accidentally swallowing the suggestion.
        // End and Tab remain explicit acceptance keys. Ctrl+E must reach the
        // PTY because readline and shells use it to move to end-of-line.
        if current_suggestion.is_some() {
            let is_accept = accepts_inline_suggestion(&key, ctrl, alt, meta, shift);
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

        let data = terminal_key_bytes(&key, &code, ctrl, alt, shift, app_cursor);

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
                    "return (function() {{ const el = document.getElementById('{measure_cid}'); if (!el) return 'no-el'; const rect = el.getBoundingClientRect(); if (rect.width <= 0 || rect.height <= 0) return 'zero'; const cs = getComputedStyle(el); const padH = parseFloat(cs.paddingLeft) + parseFloat(cs.paddingRight); const padV = parseFloat(cs.paddingTop) + parseFloat(cs.paddingBottom); const bw = parseFloat(cs.borderLeftWidth) + parseFloat(cs.borderRightWidth); const bh = parseFloat(cs.borderTopWidth) + parseFloat(cs.borderBottomWidth); const h = rect.height - padV - bh; if (h <= 0) return 'small'; const sd = document.getElementById('{scroll_cid}'); if (!sd) return 'no-scroll'; const sdRect = sd.getBoundingClientRect(); if (sdRect.width <= 0) return 'small'; let w = sdRect.width; if (sd.firstElementChild) {{ const gutterW = sd.firstElementChild.getBoundingClientRect().width; w = Math.max(0, sdRect.width - gutterW); }} if (w <= 0) return 'small'; const test = document.createElement('span'); test.textContent = 'M'; test.style.cssText = 'font-family:JetBrains Mono,Fira Code,Cascadia Code,monospace;font-size:13px;line-height:1.5;position:absolute;visibility:hidden;white-space:pre;'; document.body.appendChild(test); const tr = test.getBoundingClientRect(); document.body.removeChild(test); const cw = Math.max(1, tr.width); const ch = Math.max(1, tr.height); const cols = Math.max(1, Math.floor(w / cw)); const rows = Math.max(1, Math.floor(h / ch)); let popupAbove = -1; let popupBelow = -1; let popupDesired = -1; const cr_sug = el.querySelector('[data-cursor-row=\"1\"]'); if (cr_sug) {{ const tr_sug = el.getBoundingClientRect(); const cr_r_sug = cr_sug.getBoundingClientRect(); el.style.setProperty('--suggestion-bottom', (tr_sug.bottom - cr_r_sug.top) + 'px'); el.style.setProperty('--suggestion-top', (cr_r_sug.bottom - tr_sug.top) + 'px'); const popup = el.querySelector('[data-rusterm-terminal-popup=\"true\"]'); if (popup) {{ popupAbove = Math.max(0, cr_r_sug.top - tr_sug.top); popupBelow = Math.max(0, tr_sug.bottom - cr_r_sug.bottom); popupDesired = Math.max(1, popup.scrollHeight); }} }} return cols + ',' + rows + ',' + cw.toFixed(2) + ',' + ch.toFixed(2) + ',' + popupAbove.toFixed(2) + ',' + popupBelow.toFixed(2) + ',' + popupDesired.toFixed(2); }})()"
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
                                if let (Some(space_above), Some(space_below), Some(desired)) = (
                                    parts.get(4).and_then(|value| value.parse::<f64>().ok()),
                                    parts.get(5).and_then(|value| value.parse::<f64>().ok()),
                                    parts.get(6).and_then(|value| value.parse::<f64>().ok()),
                                ) && space_above >= 0.0
                                    && space_below >= 0.0
                                    && desired >= 0.0
                                {
                                    let layout = popup_layout(space_above, space_below, desired);
                                    let (top, bottom) = match layout.direction {
                                        PopupDirection::Above => {
                                            ("auto", "var(--suggestion-bottom, 2em)")
                                        }
                                        PopupDirection::Below => {
                                            ("var(--suggestion-top, 2em)", "auto")
                                        }
                                    };
                                    let popup_cid = cid.clone();
                                    let _ = dioxus::document::eval(&format!(
                                        "(function() {{ const el = document.getElementById('{popup_cid}'); if (!el) return; el.style.setProperty('--suggestion-popup-top', '{top}'); el.style.setProperty('--suggestion-popup-bottom', '{bottom}'); el.style.setProperty('--suggestion-popup-max-height', '{}px'); }})()",
                                        layout.max_height_px
                                    ))
                                    .await;
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

    // The `--suggestion-top` / `--suggestion-bottom` CSS variables (used by
    // SuggestionPopup / OneKeyPopup to sit below/above the cursor row) are
    // kept current by the resize future above, which re-measures every
    // 100ms. (A use_effect here would only run once on mount — version is a
    // plain prop, not a tracked Signal — leaving the value stale.)

    // Recompute exact cell ranges whenever the query or terminal output changes.
    // The helper is Unicode/cell aware; never use UTF-8 byte offsets as columns.
    {
        let query = search_query();
        let _ = version;
        let found = find_search_matches(&render_output.rows, &query);
        if search_matches() != found {
            let next_index = if found.is_empty() {
                0
            } else {
                search_match_index().min(found.len() - 1)
            };
            search_matches.set(found);
            search_match_index.set(next_index);
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

        if app_owns_mouse(
            render_output.mode_mouse_reporting,
            render_output.scrollback_offset,
            shift,
        ) {
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
    let move_any_motion = render_output.mode_mouse_any_motion;
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

        // Application motion reporting: 1002 reports only while a button is
        // held; 1003 reports every cell transition and uses pseudo-button 3
        // when no button is down.
        let pressed_button = mouse_button_down();
        let mods = e.modifiers();
        let shift_forces_local = mods.shift() && pressed_button.is_none();
        let should_report_motion = move_reporting
            && !shift_forces_local
            && (move_any_motion || (move_button_motion && pressed_button.is_some()));
        if should_report_motion {
            let Some((row, col)) = event_cell(&e) else {
                return;
            };
            if last_motion_cell() != Some((row, col)) {
                last_motion_cell.set(Some((row, col)));
                move_on_input.call(encode_mouse_report(
                    MouseReportKind::Motion,
                    pressed_button.unwrap_or(3),
                    col,
                    row,
                    mods.shift(),
                    mods.alt(),
                    mods.ctrl(),
                    move_sgr,
                ));
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
                return;
            }
            // No app-owned press was armed: this was Shift-forced local
            // selection, so continue into the local mouseup path below.
        }

        if !selecting() {
            return;
        }
        selecting.set(false);
        let Some(ts) = selection() else { return };
        let ts = finalize_selection_on_mouse_up(ts, event_cell(&e));
        selection.set(Some(ts));
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
    // (vim `set mouse=a`, tmux, htop), the two ordinary mousedown/mouseup pairs
    // are already forwarded; the later DOM `dblclick` notification only skips
    // local selection. Shift forces local word selection even under app
    // reporting (same override as single-click drag selection).
    let dbl_reporting = render_output.mode_mouse_reporting;
    let dbl_scrollback_offset = render_output.scrollback_offset;
    let dbl_rows = render_output.rows.clone();
    let on_double_click = move |e: MouseEvent| {
        if current_disconnected {
            return;
        }
        let mods = e.modifiers();
        let shift = mods.shift();

        if app_owns_mouse(dbl_reporting, dbl_scrollback_offset, shift) {
            // Both ordinary mousedown events have already been forwarded to the
            // application. The synthetic `dblclick` notification must not emit
            // a third press report; it only suppresses local word selection.
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
            let show_search_highlights = search_visible() || search_highlight_pinned();
            let search_ranges: Vec<(usize, usize, bool)> = if show_search_highlights {
                sm.iter()
                    .enumerate()
                    .filter(|(_, found)| found.row == row_idx)
                    .map(|(index, found)| (found.start_col, found.end_col, index == sidx))
                    .collect()
            } else {
                Vec::new()
            };

            // Diff status remains a row-level background; exact search spans
            // are rendered above it so both signals stay visible.
            let is_diff_row = row_diffs.as_ref().and_then(|d| d.get(row_idx))
                == Some(&crate::comparison::RowDiff::Different);

            let row_bg = if is_diff_row {
                crate::comparison::DIFF_ROW_BG
            } else {
                ""
            };

            let content_html = row_to_html(
                row,
                cur_col,
                &render_output.cursor_color,
                sug,
                sel_for_row(row_idx),
                &search_ranges,
            );

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
                        "(function() {{ const el = document.getElementById('{cid}'); if (!el) return; el.style.caretColor = 'transparent'; el.style.webkitTapHighlightColor = 'transparent'; el.addEventListener('focus', function() {{ this.style.outline = 'none'; this.style.boxShadow = 'none'; }}); if (el.dataset.rustermPointerCapture !== '1') {{ el.dataset.rustermPointerCapture = '1'; el.addEventListener('pointerdown', function(event) {{ if (event.target && event.target.closest && event.target.closest('[data-rusterm-terminal-popup=\"true\"]')) return; if (typeof this.setPointerCapture === 'function') {{ try {{ this.setPointerCapture(event.pointerId); }} catch (_) {{}} }} }}); }} }})()"
                    )).await;
                });
            },
            tabindex: "0",
            onclick: onclick_focus,
            oncontextmenu: oncontextmenu_reconnect,
            onfocus: move |_| focused.set(true),
            onblur: move |_| {
                focused.set(false);
                on_focus_lost.call(());
            },
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

            // Search overlay bar: find next/previous, find selected text,
            // online-search only the explicit selection, and optionally keep
            // exact-match highlights after the bar closes.
            if search_visible() {
                {
                    let query = search_query();
                    let matches = search_matches();
                    let match_idx = search_match_index();
                    let match_info = if matches.is_empty() {
                        crate::i18n::t("terminal_search.no_matches")
                    } else {
                        crate::i18n::tf(
                            "terminal_search.match_count",
                            &[("current", &(match_idx + 1)), ("total", &matches.len())],
                        )
                    };
                    let selected_text = search_query_from_selection(
                        &selection_text(),
                        selection(),
                        &render_output.rows,
                    );
                    let selected_for_find = selected_text.clone().unwrap_or_default();
                    let selected_for_online = selected_text.clone().unwrap_or_default();
                    let has_selection = selected_text.is_some();
                    let highlight_active = search_highlight_pinned();
                    let highlight_style = if highlight_active {
                        "background:#7aa2f7;border:1px solid #7aa2f7;color:#1a1b26;"
                    } else {
                        "background:#1a1b26;border:1px solid #2a2b3d;color:#9aa5ce;"
                    };
                    rsx! {
                        div {
                            "data-rusterm-terminal-popup": "true",
                            style: "
                                position: absolute;
                                top: 0; left: 0; right: 0;
                                z-index: 10;
                                display: flex;
                                align-items: center;
                                flex-wrap: wrap;
                                gap: 6px;
                                padding: 7px 10px;
                                background: #24283b;
                                border-bottom: 1px solid #2a2b3d;
                                border-radius: 4px 4px 0 0;
                                box-shadow: 0 4px 12px rgba(0,0,0,0.28);
                            ",
                            onclick: move |e: MouseEvent| e.stop_propagation(),
                            onmousedown: move |e: MouseEvent| e.stop_propagation(),
                            span {
                                style: "color:#9aa5ce;font-size:12px;white-space:nowrap;",
                                { crate::i18n::t("terminal_search.find") }
                            }
                            input {
                                id: "{search_input_id}",
                                r#type: "text",
                                value: "{query}",
                                autofocus: true,
                                placeholder: crate::i18n::t("terminal_search.placeholder"),
                                style: "
                                    flex: 1;
                                    min-width: 120px;
                                    background: #1a1b26;
                                    border: 1px solid #2a2b3d;
                                    border-radius: 4px;
                                    color: #c0caf5;
                                    padding: 4px 8px;
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
                                        e.prevent_default();
                                        search_visible.set(false);
                                        if !search_highlight_pinned() {
                                            search_query.set(String::new());
                                            search_matches.set(Vec::new());
                                            search_match_index.set(0);
                                        }
                                    } else if matches!(e.key(), Key::Enter) {
                                        e.prevent_default();
                                        let matches = search_matches();
                                        if !matches.is_empty() {
                                            let current = search_match_index().min(matches.len() - 1);
                                            let next = if e.modifiers().shift() {
                                                current.checked_sub(1).unwrap_or(matches.len() - 1)
                                            } else {
                                                (current + 1) % matches.len()
                                            };
                                            search_match_index.set(next);
                                        }
                                    }
                                },
                            }
                            span {
                                style: "color:#9aa5ce;font-size:11px;white-space:nowrap;min-width:58px;text-align:right;",
                                "{match_info}"
                            }
                            button {
                                title: crate::i18n::t("terminal_search.previous"),
                                style: "background:#1a1b26;border:1px solid #2a2b3d;border-radius:4px;color:#c0caf5;cursor:pointer;font-size:13px;padding:3px 7px;",
                                onclick: move |_| {
                                    let matches = search_matches();
                                    if !matches.is_empty() {
                                        let current = search_match_index().min(matches.len() - 1);
                                        search_match_index.set(current.checked_sub(1).unwrap_or(matches.len() - 1));
                                    }
                                },
                                "▲"
                            }
                            button {
                                title: crate::i18n::t("terminal_search.next"),
                                style: "background:#1a1b26;border:1px solid #2a2b3d;border-radius:4px;color:#c0caf5;cursor:pointer;font-size:13px;padding:3px 7px;",
                                onclick: move |_| {
                                    let matches = search_matches();
                                    if !matches.is_empty() {
                                        search_match_index.set((search_match_index().min(matches.len() - 1) + 1) % matches.len());
                                    }
                                },
                                "▼"
                            }
                            button {
                                disabled: !has_selection,
                                title: crate::i18n::t("terminal_search.find_selection"),
                                style: if has_selection { "background:#1a1b26;border:1px solid #2a2b3d;border-radius:4px;color:#c0caf5;cursor:pointer;font-size:11px;padding:4px 7px;" } else { "background:#1a1b26;border:1px solid #2a2b3d;border-radius:4px;color:#565f89;cursor:not-allowed;font-size:11px;padding:4px 7px;" },
                                onclick: move |_| {
                                    if !selected_for_find.is_empty() {
                                        search_query.set(selected_for_find.clone());
                                        search_match_index.set(0);
                                    }
                                },
                                { crate::i18n::t("terminal_search.selection") }
                            }
                            button {
                                disabled: !has_selection,
                                title: crate::i18n::t("terminal_search.online_search_tip"),
                                style: if has_selection { "background:#1a1b26;border:1px solid #2a2b3d;border-radius:4px;color:#c0caf5;cursor:pointer;font-size:11px;padding:4px 7px;" } else { "background:#1a1b26;border:1px solid #2a2b3d;border-radius:4px;color:#565f89;cursor:not-allowed;font-size:11px;padding:4px 7px;" },
                                onclick: move |_| {
                                    if !selected_for_online.is_empty() {
                                        open_online_search(&selected_for_online);
                                    }
                                },
                                { crate::i18n::t("terminal_search.online") }
                            }
                            button {
                                title: if highlight_active { crate::i18n::t("terminal_search.highlight_on") } else { crate::i18n::t("terminal_search.highlight") },
                                style: "{highlight_style}border-radius:4px;cursor:pointer;font-size:11px;padding:4px 7px;",
                                onclick: move |_| search_highlight_pinned.toggle(),
                                { crate::i18n::t("terminal_search.highlight_label") }
                            }
                            button {
                                title: crate::i18n::t("terminal_search.close"),
                                style: "background:none;border:none;color:#9aa5ce;cursor:pointer;font-size:14px;padding:0 4px;",
                                onclick: move |_| {
                                    search_visible.set(false);
                                    if !search_highlight_pinned() {
                                        search_query.set(String::new());
                                        search_matches.set(Vec::new());
                                        search_match_index.set(0);
                                    }
                                },
                                "✕"
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

            // Suggestion panel (Atuin-style, positioned below the cursor line)
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
                    correction_suggestions: current_suggestion_corrections.clone(),
                    history_completion: current_history_completion_visible,
                    max_rows: suggestion_max_rows,
                }
            }

            if matches!(
                current_onekey_submission_feedback,
                Some(OneKeySubmissionFeedback::Submitted { .. })
            ) {
                div {
                    style: "position:absolute;right:10px;top:var(--suggestion-top, 2em);z-index:19;padding:5px 9px;background:var(--skin-bg);border:1px solid var(--skin-success);border-radius:4px;color:var(--skin-success);font-size:11px;pointer-events:none;",
                    { crate::i18n::t("onekey.submission_feedback") }
                }
            }

            // OneKey autofill popup for this TerminalView's session. It is
            // positioned relative to this pane and grows below the cursor.
            if onekey_visible && !current_onekey_entries.is_empty() {
                OneKeyPopup {
                    entries: current_onekey_entries.clone(),
                    selected: current_onekey_selected.min(current_onekey_entries.len().saturating_sub(1)),
                    submission_feedback: current_onekey_submission_feedback.clone(),
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
        ClipboardCopyOutcome, CopyShortcut, OneKeyKeyAction, PopupDirection, SEARCH_CURRENT_BG,
        SEARCH_MATCH_BG, TerminalOverlayKeyAction, TextSelection, accepts_history_completion,
        accepts_inline_suggestion, app_owns_mouse, cell_style, color_to_css,
        copy_text_to_clipboard, cursor_key_seq, event_cell_from_coords,
        finalize_selection_on_mouse_up, find_search_matches, is_find_shortcut,
        is_history_completion_shortcut, onekey_popup_key_action, online_search_url, popup_layout,
        scroll_thumb_geometry, search_query_from_selection, suggestion_navigation_index,
        terminal_key_bytes, terminal_overlay_key_action, terminal_selection_text,
        word_range_in_row,
    };
    use dioxus::prelude::{Code, Key};
    use rusterm_core::terminal::{CellColor, RenderCell, RenderRow};

    #[test]
    fn popup_uses_space_below_when_it_fits() {
        let layout = popup_layout(40.0, 160.0, 120.0);
        assert_eq!(layout.direction, PopupDirection::Below);
        assert_eq!(layout.max_height_px, 160);
    }

    #[test]
    fn popup_flips_above_when_bottom_dock_reduces_space() {
        let layout = popup_layout(180.0, 45.0, 120.0);
        assert_eq!(layout.direction, PopupDirection::Above);
        assert_eq!(layout.max_height_px, 180);
    }

    #[test]
    fn popup_uses_larger_side_and_scrolls_when_neither_side_fits() {
        let layout = popup_layout(70.0, 50.0, 140.0);
        assert_eq!(layout.direction, PopupDirection::Above);
        assert_eq!(layout.max_height_px, 70);

        let resized = popup_layout(35.0, 90.0, 140.0);
        assert_eq!(resized.direction, PopupDirection::Below);
        assert_eq!(resized.max_height_px, 90);
    }

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
    fn mouseup_release_cell_completes_full_visible_window_selection() {
        let rows = vec![
            row_from("first"),
            row_from(""),
            RenderRow {
                cells: cells_from("soft-"),
                wrapped: true,
            },
            RenderRow {
                cells: vec![
                    wide_cell('错'),
                    wide_next_cell(),
                    wide_cell('误'),
                    wide_next_cell(),
                ],
                wrapped: false,
            },
        ];
        let stale_selection = TextSelection {
            anchor: (0, 0),
            // Simulates the final mousemove arriving before the pointer reaches
            // the bottom-right release position.
            head: (1, 0),
        };

        let finalized = finalize_selection_on_mouse_up(stale_selection, Some((3, 3)));

        assert_eq!(
            terminal_selection_text("", Some(finalized), &rows),
            "first\n\nsoft-错误"
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
            terminal_overlay_key_action(&key, false, false, true, false, true, 0, 1, false),
            TerminalOverlayKeyAction::Copy(CopyShortcut::Command)
        );
        assert_eq!(
            terminal_overlay_key_action(&key, true, false, false, true, true, 0, 1, false),
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
                false,
            ),
            TerminalOverlayKeyAction::OneKey(OneKeyKeyAction::DismissAndForward)
        );
    }

    #[test]
    fn onekey_popup_does_not_consume_terminal_arrow_keys() {
        for key in [
            Key::ArrowDown,
            Key::ArrowUp,
            Key::ArrowLeft,
            Key::ArrowRight,
        ] {
            assert_eq!(
                onekey_popup_key_action(&key, 3, false),
                OneKeyKeyAction::DismissAndForward,
                "{key:?} must reach the PTY instead of navigating the popup"
            );
        }
    }

    #[test]
    fn unmodified_cursor_keys_use_standard_csi_sequences() {
        assert_eq!(cursor_key_seq(1, b'A', false, None), b"\x1b[A");
        assert_eq!(cursor_key_seq(1, b'B', false, None), b"\x1b[B");
        assert_eq!(cursor_key_seq(1, b'C', false, None), b"\x1b[C");
        assert_eq!(cursor_key_seq(1, b'D', false, None), b"\x1b[D");
        assert_eq!(cursor_key_seq(1, b'D', true, None), b"\x1bOD");
        assert_eq!(cursor_key_seq(1, b'D', false, Some(2)), b"\x1b[1;2D");
    }

    #[test]
    fn numpad_navigation_falls_back_to_physical_code_when_key_is_unidentified() {
        for (code, expected) in [
            (Code::Numpad8, b"\x1b[A".as_slice()),
            (Code::Numpad2, b"\x1b[B".as_slice()),
            (Code::Numpad6, b"\x1b[C".as_slice()),
            (Code::Numpad4, b"\x1b[D".as_slice()),
        ] {
            assert_eq!(
                terminal_key_bytes(&Key::Unidentified, &code, false, false, false, false),
                expected,
                "{code:?} must navigate when NumLock-off key is unidentified"
            );
        }
    }

    #[test]
    fn numpad_digits_remain_digits_when_num_lock_is_on() {
        assert_eq!(
            terminal_key_bytes(
                &Key::Character("4".into()),
                &Code::Numpad4,
                false,
                false,
                false,
                false,
            ),
            b"4"
        );
    }

    #[test]
    fn linux_readline_control_keys_encode_as_control_bytes() {
        for (text, code, expected) in [
            ("a", Code::KeyA, 0x01),
            ("e", Code::KeyE, 0x05),
            ("r", Code::KeyR, 0x12),
            ("w", Code::KeyW, 0x17),
            ("x", Code::KeyX, 0x18),
            ("z", Code::KeyZ, 0x1a),
            ("[", Code::BracketLeft, 0x1b),
            ("\\", Code::Backslash, 0x1c),
            ("]", Code::BracketRight, 0x1d),
            ("^", Code::Digit6, 0x1e),
            ("_", Code::Minus, 0x1f),
            (" ", Code::Space, 0x00),
        ] {
            assert_eq!(
                terminal_key_bytes(
                    &Key::Character(text.into()),
                    &code,
                    true,
                    false,
                    false,
                    false,
                ),
                vec![expected],
                "Ctrl+{text:?} must reach the PTY"
            );
        }
    }

    #[test]
    fn only_unmodified_tab_and_end_accept_inline_suggestions() {
        assert!(!accepts_inline_suggestion(
            &Key::Character("e".into()),
            true,
            false,
            false,
            false,
        ));
        assert!(accepts_inline_suggestion(
            &Key::End,
            false,
            false,
            false,
            false,
        ));
        assert!(accepts_inline_suggestion(
            &Key::Tab,
            false,
            false,
            false,
            false,
        ));
        assert!(!accepts_inline_suggestion(
            &Key::End,
            false,
            false,
            false,
            true,
        ));
        assert!(!accepts_inline_suggestion(
            &Key::Tab,
            true,
            false,
            false,
            false,
        ));
    }

    #[test]
    fn alt_r_opens_history_completion_and_enter_only_accepts_in_that_mode() {
        let r = Key::Character("r".into());
        assert!(is_history_completion_shortcut(
            &r, false, true, false, false
        ));
        assert!(!is_history_completion_shortcut(
            &r, true, true, false, false
        ));
        assert!(accepts_history_completion(
            &Key::Enter,
            false,
            false,
            false,
            false,
        ));
        assert!(!accepts_history_completion(
            &Key::Enter,
            true,
            false,
            false,
            false,
        ));
    }

    #[test]
    fn ctrl_n_and_ctrl_p_cycle_visible_suggestions() {
        let ctrl_n = Key::Character("n".into());
        let ctrl_p = Key::Character("p".into());
        assert_eq!(
            suggestion_navigation_index(&ctrl_n, true, false, false, false, 0, 3),
            Some(1)
        );
        assert_eq!(
            suggestion_navigation_index(&ctrl_n, true, false, false, false, 2, 3),
            Some(0)
        );
        assert_eq!(
            suggestion_navigation_index(&ctrl_p, true, false, false, false, 0, 3),
            Some(2)
        );
        assert_eq!(
            suggestion_navigation_index(&ctrl_p, true, false, false, true, 0, 3),
            None
        );
    }

    #[test]
    fn onekey_popup_tab_and_enter_select_escape_dismisses() {
        assert_eq!(
            onekey_popup_key_action(&Key::Tab, 3, false),
            OneKeyKeyAction::Select
        );
        assert_eq!(
            onekey_popup_key_action(&Key::Enter, 3, false),
            OneKeyKeyAction::Select
        );
        assert_eq!(
            onekey_popup_key_action(&Key::Escape, 3, false),
            OneKeyKeyAction::Dismiss
        );
    }

    #[test]
    fn onekey_popup_handles_empty_entries() {
        assert_eq!(
            onekey_popup_key_action(&Key::Enter, 0, false),
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
            onekey_popup_key_action(&Key::Character("x".into()), 3, false),
            OneKeyKeyAction::DismissAndForward
        );
        assert_eq!(
            onekey_popup_key_action(&Key::Backspace, 3, false),
            OneKeyKeyAction::DismissAndForward
        );
    }

    #[test]
    fn password_popup_stays_visible_while_forwarding_terminal_input() {
        for key in [Key::ArrowLeft, Key::ArrowUp, Key::Backspace] {
            assert_eq!(
                onekey_popup_key_action(&key, 1, true),
                OneKeyKeyAction::Forward
            );
        }
        assert_eq!(
            onekey_popup_key_action(&Key::Escape, 1, true),
            OneKeyKeyAction::Dismiss
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
        let html = row_to_html(&row, None, &CellColor::Default, None, Some((6, 10)), &[]);
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
        let plain = row_to_html(&row, None, &CellColor::Default, None, None, &[]);
        assert!(!plain.contains(SELECTION_BG));
    }

    #[test]
    fn search_matches_exact_ascii_cell_ranges_and_overlaps() {
        let rows = vec![row_from("banana BANANA")];
        let matches = find_search_matches(&rows, "ana");
        assert_eq!(matches.len(), 4);
        assert_eq!(
            (matches[0].row, matches[0].start_col, matches[0].end_col),
            (0, 1, 3)
        );
        assert_eq!((matches[1].start_col, matches[1].end_col), (3, 5));
        assert_eq!((matches[2].start_col, matches[2].end_col), (8, 10));
        assert_eq!((matches[3].start_col, matches[3].end_col), (10, 12));
    }

    #[test]
    fn search_matches_wide_char_cells_without_byte_offset_drift() {
        let rows = vec![RenderRow {
            cells: vec![
                RenderCell {
                    character: 'a',
                    ..Default::default()
                },
                wide_cell('中'),
                wide_next_cell(),
                RenderCell {
                    character: 'b',
                    ..Default::default()
                },
            ],
            wrapped: false,
        }];
        let matches = find_search_matches(&rows, "中b");
        assert_eq!(matches.len(), 1);
        assert_eq!((matches[0].start_col, matches[0].end_col), (1, 3));
    }

    #[test]
    fn search_html_highlights_only_matches_and_marks_current_match() {
        use super::row_to_html;
        let row = row_from("alpha beta alpha");
        let matches = find_search_matches(std::slice::from_ref(&row), "alpha");
        let ranges = matches
            .iter()
            .enumerate()
            .map(|(index, found)| (found.start_col, found.end_col, index == 1))
            .collect::<Vec<_>>();
        let html = row_to_html(&row, None, &CellColor::Default, None, None, &ranges);
        assert_eq!(html.matches(SEARCH_MATCH_BG).count(), 1);
        assert_eq!(html.matches(SEARCH_CURRENT_BG).count(), 1);
        assert!(html.contains("beta"));
    }

    #[test]
    fn selection_can_seed_find_but_multiline_selection_cannot() {
        let rows = vec![row_from("alpha beta"), row_from("gamma")];
        let one_line = TextSelection {
            anchor: (0, 6),
            head: (0, 9),
        };
        assert_eq!(
            search_query_from_selection("", Some(one_line), &rows).as_deref(),
            Some("beta")
        );

        let multiline = TextSelection {
            anchor: (0, 6),
            head: (1, 4),
        };
        assert_eq!(
            search_query_from_selection("", Some(multiline), &rows),
            None
        );
    }

    #[test]
    fn online_search_encodes_only_the_explicit_query() {
        assert_eq!(
            online_search_url("Rust 终端 & SSH").as_deref(),
            Some("https://www.google.com/search?q=Rust%20%E7%BB%88%E7%AB%AF%20%26%20SSH")
        );
        assert_eq!(online_search_url("   "), None);
    }

    #[test]
    fn find_shortcuts_preserve_macos_readline_and_layout_zoom() {
        let f = Key::Character("f".into());
        assert!(is_find_shortcut(&f, false, false, true, false, true));
        assert!(!is_find_shortcut(&f, true, false, false, false, true));
        assert!(!is_find_shortcut(&f, false, false, true, true, true));
        assert!(is_find_shortcut(&f, true, false, false, true, true));
        assert!(is_find_shortcut(&f, true, false, false, false, false));
        assert!(!is_find_shortcut(&f, false, false, true, false, false));
    }

    #[test]
    fn application_mouse_ownership_respects_shift_and_scrollback() {
        assert!(app_owns_mouse(true, 0, false));
        assert!(!app_owns_mouse(true, 0, true));
        assert!(!app_owns_mouse(true, 12, false));
        assert!(!app_owns_mouse(false, 0, false));
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

    #[test]
    fn xterm_indexed_and_truecolor_cells_emit_valid_foreground_css() {
        assert_eq!(
            color_to_css(&rusterm_core::terminal::CellColor::Indexed(196)),
            "#ff0000"
        );
        assert_eq!(
            color_to_css(&rusterm_core::terminal::CellColor::Indexed(21)),
            "#0000ff"
        );
        assert_eq!(
            color_to_css(&rusterm_core::terminal::CellColor::Spec(vte::ansi::Rgb {
                r: 12,
                g: 34,
                b: 56,
            })),
            "#0c2238"
        );
        assert_eq!(
            cell_style(
                &rusterm_core::terminal::CellColor::Indexed(196),
                &rusterm_core::terminal::CellColor::Indexed(22),
                rusterm_core::terminal::CellFlags::empty(),
            ),
            "color:#ff0000;background:#005f00"
        );
    }

    #[test]
    fn sgr_effects_compose_without_discarding_colors() {
        let flags = rusterm_core::terminal::CellFlags::DIM
            | rusterm_core::terminal::CellFlags::DOUBLE_UNDERLINE
            | rusterm_core::terminal::CellFlags::STRIKETHROUGH
            | rusterm_core::terminal::CellFlags::INVERSE;
        let style = cell_style(
            &rusterm_core::terminal::CellColor::Indexed(196),
            &rusterm_core::terminal::CellColor::Indexed(22),
            flags,
        );

        assert!(style.contains("color:#005f00"));
        assert!(style.contains("background:#ff0000"));
        assert!(style.contains("opacity:0.65"));
        assert!(style.contains("text-decoration-line:underline line-through"));
        assert!(style.contains("text-decoration-style:double"));
    }
}
