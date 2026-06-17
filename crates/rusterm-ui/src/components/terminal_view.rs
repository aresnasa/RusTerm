use dioxus::prelude::*;

use rusterm_core::terminal::{CellColor, CellFlags, RenderOutput, RenderRow};

use crate::components::SuggestionPopup;

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
        "a" => vec![0x01], "b" => vec![0x02], "c" => vec![0x03],
        "d" => vec![0x04], "e" => vec![0x05], "f" => vec![0x06],
        "g" => vec![0x07], "h" => vec![0x08], "i" => vec![0x09],
        "j" => vec![0x0a], "k" => vec![0x0b], "l" => vec![0x0c],
        "m" => vec![0x0d], "n" => vec![0x0e], "o" => vec![0x0f],
        "p" => vec![0x10], "q" => vec![0x11], "r" => vec![0x12],
        "s" => vec![0x13], "t" => vec![0x14], "u" => vec![0x15],
        "v" => vec![0x16], "w" => vec![0x17], "x" => vec![0x18],
        "y" => vec![0x19], "z" => vec![0x1a],
        "[" => vec![0x1b], "\\" => vec![0x1c], "]" => vec![0x1d],
        "^" => vec![0x1e], "_" => vec![0x1f],
        "2" | "@" => vec![0x00], "3" => vec![0x1b], "4" => vec![0x1c],
        "5" => vec![0x1d], "6" => vec![0x1e], "7" | "/" => vec![0x1f],
        "8" => vec![0x7f], " " => vec![0x00],
        _ => vec![],
    }
}

fn code_to_char(code: &Code) -> u8 {
    match code {
        Code::Digit0 => b'0', Code::Digit1 => b'1', Code::Digit2 => b'2',
        Code::Digit3 => b'3', Code::Digit4 => b'4', Code::Digit5 => b'5',
        Code::Digit6 => b'6', Code::Digit7 => b'7', Code::Digit8 => b'8',
        Code::Digit9 => b'9',
        Code::Minus => b'-', Code::Equal => b'=',
        Code::BracketLeft => b'[', Code::BracketRight => b']',
        Code::Backslash => b'\\', Code::Semicolon => b';',
        Code::Quote => b'\'', Code::Backquote => b'`',
        Code::Comma => b',', Code::Period => b'.', Code::Slash => b'/',
        _ => 0,
    }
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
        vte::ansi::NamedColor::BrightBlack => "#565f89",
        vte::ansi::NamedColor::BrightRed => "#f7768e",
        vte::ansi::NamedColor::BrightGreen => "#9ece6a",
        vte::ansi::NamedColor::BrightYellow => "#e0af68",
        vte::ansi::NamedColor::BrightBlue => "#7aa2f7",
        vte::ansi::NamedColor::BrightMagenta => "#bb9af7",
        vte::ansi::NamedColor::BrightCyan => "#7dcfff",
        vte::ansi::NamedColor::BrightWhite => "#c0caf5",
        vte::ansi::NamedColor::Foreground => "#c0caf5",
        vte::ansi::NamedColor::Background => "#1a1b26",
        vte::ansi::NamedColor::Cursor => "#c0caf5",
        _ => "#c0caf5",
    }
}

fn indexed_color_hex(idx: u8) -> String {
    if idx < 16 {
        match idx {
            0 => "#414868", 1 => "#f7768e", 2 => "#9ece6a", 3 => "#e0af68",
            4 => "#7aa2f7", 5 => "#bb9af7", 6 => "#7dcfff", 7 => "#c0caf5",
            8 => "#565f89", 9 => "#f7768e", 10 => "#9ece6a", 11 => "#e0af68",
            12 => "#7aa2f7", 13 => "#bb9af7", 14 => "#7dcfff", 15 => "#c0caf5",
            _ => "#c0caf5",
        }.to_string()
    } else if idx < 232 {
        let i = (idx - 16) as u32;
        let r_val = if i / 36 > 0 { 55 + (i / 36) * 40 } else { 0 };
        let g_val = if (i % 36) / 6 > 0 { 55 + ((i % 36) / 6) * 40 } else { 0 };
        let b_val = if i % 6 > 0 { 55 + (i % 6) * 40 } else { 0 };
        format!("#{:02x}{:02x}{:02x}", r_val.min(255), g_val.min(255), b_val.min(255))
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
    if !fg_css.is_empty() { parts.push(fg_css); }
    let bg_css = color_to_css(bg);
    if !bg_css.is_empty() { parts.push(format!("background:{}", bg_css)); }
    if flags.contains(CellFlags::BOLD) { parts.push("font-weight:700".to_string()); }
    if flags.contains(CellFlags::ITALIC) { parts.push("font-style:italic".to_string()); }
    if flags.contains(CellFlags::UNDERLINE) { parts.push("text-decoration:underline".to_string()); }
    if flags.contains(CellFlags::STRIKETHROUGH) { parts.push("text-decoration:line-through".to_string()); }
    parts.join(";")
}

/// Render a terminal row to an HTML string. Uses `dangerous_inner_html`
/// for fast DOM updates — avoids Dioxus per-span VDOM diffing overhead.
///
/// When a suggestion is shown, we only render cells up to the cursor position,
/// then append the suggestion right after it. Cells after the cursor are
/// typically empty spaces and would push the suggestion to the end of the row.
fn row_to_html(row: &RenderRow, cursor_col: Option<usize>, suggestion: Option<&str>) -> String {
    let mut html = String::with_capacity(row.cells.len() * 4);

    let mut cur_fg = CellColor::Default;
    let mut cur_bg = CellColor::Default;
    let mut cur_flags = CellFlags::empty();
    let mut cur_text = String::new();

    let flush = |html: &mut String, text: &str, fg: &CellColor, bg: &CellColor, flags: CellFlags| {
        if text.is_empty() { return; }
        let style = cell_style(fg, bg, flags);
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
            if i >= stop { break; }
        }

        if cell.wide_next { continue; }

        let is_cursor = cursor_col == Some(i);
        if is_cursor {
            flush(&mut html, &cur_text, &cur_fg, &cur_bg, cur_flags);
            cur_text.clear();

            let ch = if cell.character == ' ' { "&nbsp;" } else { &html_escape(&cell.character.to_string()) };
            let base_style = cell_style(&cell.fg, &cell.bg, cell.flags);
            let cursor_style = if base_style.is_empty() {
                "border-left:2px solid #c0caf5;margin-left:-1px".to_string()
            } else {
                format!("{};border-left:2px solid #c0caf5;margin-left:-1px", base_style)
            };
            html.push_str("<span style=\"");
            html.push_str(&cursor_style);
            html.push_str("\">");
            html.push_str(ch);
            html.push_str("</span>");

            cur_fg = CellColor::Default;
            cur_bg = CellColor::Default;
            cur_flags = CellFlags::empty();
            continue;
        }

        let same_style = cell.fg == cur_fg && cell.bg == cur_bg && cell.flags == cur_flags;
        if !cur_text.is_empty() && !same_style {
            flush(&mut html, &cur_text, &cur_fg, &cur_bg, cur_flags);
            cur_text.clear();
        }

        cur_fg = cell.fg.clone();
        cur_bg = cell.bg.clone();
        cur_flags = cell.flags;
        cur_text.push(cell.character);
    }

    flush(&mut html, &cur_text, &cur_fg, &cur_bg, cur_flags);

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
    on_history_search: EventHandler<()>,
) -> Element {
    let mut focused = use_signal(|| false);
    let mut search_visible = use_signal(|| false);
    let mut search_query = use_signal(String::new);
    let mut search_match_index = use_signal(|| 0usize);
    let mut search_matches: Signal<Vec<(usize, usize)>> = use_signal(Vec::new);

    let current_suggestion = suggestion.clone();
    let current_suggestions = suggestions.clone();
    let current_suggestion_visible = suggestion_visible;
    let current_suggestion_selected = suggestion_selected;

    let closure_suggestions = current_suggestions.clone();
    let handle_keydown = move |e: KeyboardEvent| {
        let key = e.key();
        let code = e.code();
        let mods = e.modifiers();
        let ctrl = mods.ctrl();
        let alt = mods.alt();
        let meta = mods.meta();
        let shift = mods.shift();

        if meta { return; }
        e.prevent_default();

        // Ctrl+R: open history search panel
        if ctrl && !shift && !alt && matches!(key, Key::Character(ref s) if s.eq_ignore_ascii_case("r")) {
            on_history_search.call(());
            return;
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

        // Ctrl+Shift+C: copy selection
        if ctrl && shift && matches!(key, Key::Character(ref s) if s == "c" || s == "C") {
            spawn(async move {
                let _ = dioxus::document::eval(
                    "navigator.clipboard.writeText(window.getSelection().toString())"
                ).await;
            });
            return;
        }

        // Ctrl+Shift+V / Shift+Insert: paste from clipboard
        if (ctrl && shift && matches!(key, Key::Character(ref s) if s == "v" || s == "V"))
            || (shift && matches!(key, Key::Insert))
        {
            let input_cb = on_input;
            let bracketed = render_output.mode_bracketed_paste;
            spawn(async move {
                if let Ok(result) = dioxus::document::eval("navigator.clipboard.readText()").await {
                    if let Some(text) = result.as_str() {
                        if !text.is_empty() {
                            let data = if bracketed {
                                let mut buf = Vec::with_capacity(text.len() + 12);
                                buf.extend_from_slice(b"\x1b[200~");
                                buf.extend_from_slice(text.as_bytes());
                                buf.extend_from_slice(b"\x1b[201~");
                                buf
                            } else {
                                text.as_bytes().to_vec()
                            };
                            input_cb.call(data);
                        }
                    }
                }
            });
            return;
        }

        // ── Suggestion dropdown navigation ──
        if current_suggestion_visible && !closure_suggestions.is_empty() {
            match &key {
                Key::ArrowDown => {
                    let next = if current_suggestion_selected + 1 >= closure_suggestions.len() {
                        0
                    } else {
                        current_suggestion_selected + 1
                    };
                    on_suggestion_navigate.call(Some(next));
                    return;
                }
                Key::ArrowUp => {
                    let prev = if current_suggestion_selected == 0 {
                        closure_suggestions.len().saturating_sub(1)
                    } else {
                        current_suggestion_selected - 1
                    };
                    on_suggestion_navigate.call(Some(prev));
                    return;
                }
                Key::Tab | Key::Enter => {
                    if let Some(cmd) = closure_suggestions.get(current_suggestion_selected) {
                        on_suggestion_accept.call(cmd.clone());
                    }
                    return;
                }
                Key::Escape => {
                    on_suggestion_dismiss.call(());
                    return;
                }
                _ => {}
            }
        }

        // ── Auto-completion: accept inline suggestion with Right/End/Ctrl+E ──
        if current_suggestion.is_some() && !current_suggestion_visible {
            let is_accept = match &key {
                Key::ArrowRight => true,
                Key::End => true,
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
                Key::PageUp => { on_scroll_up.call(10); return; }
                Key::PageDown => { on_scroll_down.call(10); return; }
                Key::Home => { on_scroll_up.call(render_output.scrollback_total); return; }
                Key::End => { on_scroll_to_bottom.call(()); return; }
                _ => {}
            }
        }

        let is_enter = !ctrl && !alt && matches!(key, Key::Enter);
        let app_cursor = render_output.mode_cursor_keys;

        let modifier: Option<u8> = match (ctrl, alt, shift) {
            (false, false, false) => None,
            (false, false, true)  => Some(2),
            (false, true,  false) => Some(3),
            (false, true,  true)  => Some(4),
            (true,  false, false) => Some(5),
            (true,  false, true)  => Some(6),
            (true,  true,  false) => Some(7),
            (true,  true,  true)  => Some(8),
        };

        let data: Vec<u8> = match key {
            Key::ArrowUp    => cursor_key_seq(1, b'A', app_cursor, modifier),
            Key::ArrowDown  => cursor_key_seq(1, b'B', app_cursor, modifier),
            Key::ArrowRight => cursor_key_seq(1, b'C', app_cursor, modifier),
            Key::ArrowLeft  => cursor_key_seq(1, b'D', app_cursor, modifier),

            Key::Home     => csi_seq(1, modifier, b'H'),
            Key::End      => csi_seq(1, modifier, b'F'),
            Key::Insert   => csi_seq(2, modifier, b'~'),
            Key::Delete   => csi_seq(3, modifier, b'~'),
            Key::PageUp   => csi_seq(5, modifier, b'~'),
            Key::PageDown => csi_seq(6, modifier, b'~'),

            Key::F1 => cursor_key_seq(1, b'P', app_cursor, modifier),
            Key::F2 => cursor_key_seq(1, b'Q', app_cursor, modifier),
            Key::F3 => cursor_key_seq(1, b'R', app_cursor, modifier),
            Key::F4 => cursor_key_seq(1, b'S', app_cursor, modifier),

            Key::F5  => csi_seq(15, modifier, b'~'),
            Key::F6  => csi_seq(17, modifier, b'~'),
            Key::F7  => csi_seq(18, modifier, b'~'),
            Key::F8  => csi_seq(19, modifier, b'~'),
            Key::F9  => csi_seq(20, modifier, b'~'),
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

            Key::Enter     => if alt { vec![0x1b, 0x0d] } else { vec![0x0d] },
            Key::Backspace => if alt { vec![0x1b, 0x7f] } else { vec![0x7f] },
            Key::Tab       => vec![0x09],
            Key::Escape    => vec![0x1b],

            Key::Character(ref s) => s.as_bytes().to_vec(),
            _ => vec![],
        };

        if !data.is_empty() {
            if is_enter { on_command.call(version.to_string()); }
            on_input.call(data);
        }
    };

    let container_id = format!("terminal-input-{session_id}");
    let scroll_id = format!("terminal-scroll-{session_id}");

    let sid_for_focus = session_id.clone();
    use_effect(move || {
        let focus_sid = sid_for_focus.clone();
        let cid = format!("terminal-input-{focus_sid}");
        spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let _ = dioxus::document::eval(&format!(
                "document.getElementById('{cid}')?.focus()"
            )).await;
        });
    });

    let sid_for_window_focus = session_id.clone();
    use_effect(move || {
        let cid = format!("terminal-input-{sid_for_window_focus}");
        let script = format!(r#"
            (function() {{
                const el = document.getElementById('{cid}');
                if (!el) return;
                if (el._windowFocusHandler) {{
                    window.removeEventListener('focus', el._windowFocusHandler);
                    window.removeEventListener('blur', el._windowBlurHandler);
                }}
                el._windowFocusHandler = function() {{
                    const active = document.activeElement;
                    const isInteractive = active && (
                        active.tagName === 'INPUT' || active.tagName === 'BUTTON' ||
                        active.tagName === 'SELECT' || active.tagName === 'TEXTAREA' ||
                        active.closest('[role="dialog"]')
                    );
                    if (!isInteractive) el.focus();
                }};
                el._windowBlurHandler = function() {{
                    if (document.activeElement === el) el.blur();
                }};
                window.addEventListener('focus', el._windowFocusHandler);
                window.addEventListener('blur', el._windowBlurHandler);
            }})()
        "#);
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
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                let measure_cid = cid.clone();
                let result = dioxus::document::eval(&format!(
                    "return (function() {{ const el = document.getElementById('{measure_cid}'); if (!el) return 'no-el'; const rect = el.getBoundingClientRect(); if (rect.width <= 0 || rect.height <= 0) return 'zero'; const cs = getComputedStyle(el); const padH = parseFloat(cs.paddingLeft) + parseFloat(cs.paddingRight); const padV = parseFloat(cs.paddingTop) + parseFloat(cs.paddingBottom); const bw = parseFloat(cs.borderLeftWidth) + parseFloat(cs.borderRightWidth); const bh = parseFloat(cs.borderTopWidth) + parseFloat(cs.borderBottomWidth); const w = rect.width - padH - bw; const h = rect.height - padV - bh; if (w <= 0 || h <= 0) return 'small'; const test = document.createElement('span'); test.textContent = 'M'; test.style.cssText = 'font-family:JetBrains Mono,Fira Code,Cascadia Code,monospace;font-size:13px;line-height:1.5;position:absolute;visibility:hidden;white-space:pre;'; document.body.appendChild(test); const tr = test.getBoundingClientRect(); document.body.removeChild(test); const cw = Math.max(1, tr.width); const ch = Math.max(1, tr.height); const cols = Math.max(1, Math.floor(w / cw)); const rows = Math.max(1, Math.floor((h - 1) / ch)); return cols + ',' + rows + ',' + cw.toFixed(2) + ',' + ch.toFixed(2); }})()"
                )).await;
                if let Ok(value) = result {
                    if let Some(s) = value.as_str() {
                        if s == "no-el" || s == "zero" || s == "small" || s.is_empty() { continue; }
                        let parts: Vec<&str> = s.split(',').collect();
                        if parts.len() >= 2 {
                            if let (Ok(cols), Ok(rows)) = (parts[0].parse::<u16>(), parts[1].parse::<u16>()) {
                                if cols != last_cols || rows != last_rows {
                                    last_cols = cols;
                                    last_rows = rows;
                                    let char_w: f64 = parts.get(2).and_then(|v| v.parse().ok()).unwrap_or(0.0);
                                    let char_h: f64 = parts.get(3).and_then(|v| v.parse().ok()).unwrap_or(0.0);
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

    // Recompute search matches
    {
        let query = search_query();
        let _ = version;
        if !query.is_empty() {
            let q = query.to_lowercase();
            let mut found = Vec::new();
            for (row_idx, row) in render_output.rows.iter().enumerate() {
                let line: String = row.cells.iter().filter(|c| !c.wide_next).map(|c| c.character).collect();
                let lower = line.to_lowercase();
                let mut start = 0;
                while let Some(pos) = lower[start..].find(&q) {
                    found.push((row_idx, start + pos));
                    start = start + pos + 1;
                    if start >= lower.len() { break; }
                }
            }
            search_matches.set(found);
        }
    }

    let focus_container_id = container_id.clone();
    let onclick_focus = move |_| {
        focused.set(true);
        let cid = focus_container_id.clone();
        spawn(async move {
            let _ = dioxus::document::eval(&format!(
                "document.getElementById('{cid}')?.focus()"
            )).await;
        });
    };

    // Gutter width in ch units based on max line number digit count
    let max_line_num = line_number_start + total_rows;
    let gutter_width = if max_line_num < 10 { 3 }
        else if max_line_num < 100 { 4 }
        else if max_line_num < 1000 { 5 }
        else if max_line_num < 10000 { 6 }
        else { 7 };

    // Pre-render line numbers as a single HTML block (gutter column)
    let gutter_html = render_output.rows.iter().enumerate().map(|(row_idx, _)| {
        let line_num = line_number_start + row_idx;
        format!("<div style=\"height:1.5em;line-height:1.5\">{}</div>", line_num)
    }).collect::<Vec<_>>().join("");

    // Pre-render content rows to HTML (no line numbers, no flex per-row)
    let row_htmls: Vec<String> = render_output.rows.iter().enumerate().map(|(row_idx, row)| {
        let is_cursor_row = row_idx == cursor_row && cursor_visible;
        let cur_col = if is_cursor_row { Some(cursor_col) } else { None };
        let sug = if is_cursor_row { suggestion.as_deref() } else { None };

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

        let content_html = row_to_html(row, cur_col, sug);

        let mut html = String::with_capacity(content_html.len() + 60);
        html.push_str("<div style=\"white-space:pre;line-height:1.5;");
        html.push_str(row_bg);
        html.push_str("\">");
        html.push_str(&content_html);
        html.push_str("</div>");
        html
    }).collect();

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
                        "(function() {{ const el = document.getElementById('{cid}'); if (!el) return; el.style.caretColor = 'transparent'; el.style.webkitTapHighlightColor = 'transparent'; el.addEventListener('focus', function() {{ this.style.outline = 'none'; this.style.boxShadow = 'none'; }}); }})()"
                    )).await;
                });
            },
            tabindex: "0",
            onclick: onclick_focus,
            onfocus: move |_| focused.set(true),
            onblur: move |_| focused.set(false),
            onkeydown: handle_keydown,
            onwheel: move |e: WheelEvent| {
                e.prevent_default();
                let v = e.delta().strip_units();
                if v.y < 0.0 {
                    let rows = ((-v.y / 40.0).ceil() as usize).max(1);
                    on_scroll_up.call(rows);
                } else if v.y > 0.0 {
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
                style: "display:flex;",

                // Line number gutter
                div {
                    style: "flex-shrink:0;width:{gutter_width}ch;padding-right:8px;text-align:right;color:#3b4261;user-select:none;line-height:1.5;",
                    dangerous_inner_html: "{gutter_html}",
                }

                // Terminal content
                div {
                    style: "flex:1;min-width:0;overflow:hidden;",

                    for (row_idx, row_html) in row_htmls.iter().enumerate() {
                        div {
                            key: "{session_id}-{row_idx}",
                            dangerous_inner_html: "{row_html}",
                        }
                    }
                }
            }

            // Suggestion dropdown
            if current_suggestion_visible && !current_suggestions.is_empty() {
                SuggestionPopup {
                    suggestions: current_suggestions.clone(),
                    selected_index: current_suggestion_selected,
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
        }
    }
}
