use dioxus::prelude::*;

use rusterm_core::terminal::{CellColor, CellFlags, RenderOutput, RenderRow};

// ── Terminal key encoding helpers ──────────────────────────────────

/// Build a CSI sequence: ESC [ <param> (; <modifier>)? <final_byte>
fn csi_seq(param: u8, modifier: Option<u8>, final_byte: u8) -> Vec<u8> {
    let mut buf = vec![0x1b, 0x5b]; // ESC [
    buf.extend_from_slice(param.to_string().as_bytes());
    if let Some(m) = modifier {
        buf.push(b';');
        buf.extend_from_slice(m.to_string().as_bytes());
    }
    buf.push(final_byte);
    buf
}

/// Build a cursor key sequence with DECCKM support.
/// In normal mode: ESC [ <param> <final>  (or with modifier: ESC [ <param> ;<mod> <final>)
/// In application mode (DECCKM, no modifier): ESC O <final>
fn cursor_key_seq(param: u8, final_byte: u8, app_cursor: bool, modifier: Option<u8>) -> Vec<u8> {
    if modifier.is_some() {
        // When modifiers are present, always use CSI form with modifier
        csi_seq(param, modifier, final_byte)
    } else if app_cursor {
        // DECCKM: ESC O <final>
        vec![0x1b, 0x4f, final_byte]
    } else {
        // Normal: ESC [ <param> <final>
        csi_seq(param, None, final_byte)
    }
}

/// Map a character to its Ctrl control character (0x00-0x1F)
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
        "[" => vec![0x1b],   // ESC
        "\\" => vec![0x1c],  // FS
        "]" => vec![0x1d],   // GS
        "^" => vec![0x1e],   // RS
        "_" => vec![0x1f],   // US
        "2" | "@" => vec![0x00],  // NUL
        "3" => vec![0x1b],        // ESC
        "4" => vec![0x1c],        // FS
        "5" => vec![0x1d],        // GS
        "6" => vec![0x1e],        // RS
        "7" | "/" => vec![0x1f],  // US
        "8" => vec![0x7f],        // DEL
        " " => vec![0x00],        // Ctrl+Space = NUL
        _ => vec![],
    }
}

/// Map a physical key Code to a base character for CSI sequences.
/// Used when the logical Key value is affected by Shift (e.g., Shift+1 = "!")
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

// Named color → CSS hex mapping (Tokyo Night theme)
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
        // First 16 colors match the named colors
        match idx {
            0 => "#414868".to_string(),
            1 => "#f7768e".to_string(),
            2 => "#9ece6a".to_string(),
            3 => "#e0af68".to_string(),
            4 => "#7aa2f7".to_string(),
            5 => "#bb9af7".to_string(),
            6 => "#7dcfff".to_string(),
            7 => "#c0caf5".to_string(),
            8 => "#565f89".to_string(),
            9 => "#f7768e".to_string(),
            10 => "#9ece6a".to_string(),
            11 => "#e0af68".to_string(),
            12 => "#7aa2f7".to_string(),
            13 => "#bb9af7".to_string(),
            14 => "#7dcfff".to_string(),
            15 => "#c0caf5".to_string(),
            _ => "#c0caf5".to_string(),
        }
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

fn flags_to_style(flags: CellFlags) -> String {
    let mut parts = Vec::new();
    if flags.contains(CellFlags::BOLD) {
        parts.push("font-weight:700");
    }
    if flags.contains(CellFlags::ITALIC) {
        parts.push("font-style:italic");
    }
    if flags.contains(CellFlags::UNDERLINE) {
        parts.push("text-decoration:underline");
    }
    if flags.contains(CellFlags::STRIKETHROUGH) {
        parts.push("text-decoration:line-through");
    }
    parts.join(";")
}

struct StyledRun {
    text: String,
    fg_css: String,
    bg_css: String,
    flag_style: String,
    is_cursor: bool,
}

fn row_to_runs(row: &RenderRow, cursor_col: Option<usize>) -> Vec<StyledRun> {
    let mut runs = Vec::new();
    let mut current = StyledRun {
        text: String::new(),
        fg_css: String::new(),
        bg_css: String::new(),
        flag_style: String::new(),
        is_cursor: false,
    };

    for (i, cell) in row.cells.iter().enumerate() {
        if cell.wide_next {
            continue; // Skip continuation cells of wide chars
        }

        let fg = color_to_css(&cell.fg);
        let bg = color_to_css(&cell.bg);
        let fs = flags_to_style(cell.flags);
        let is_cursor = cursor_col == Some(i);

        let same_style = fg == current.fg_css
            && bg == current.bg_css
            && fs == current.flag_style
            && is_cursor == current.is_cursor;

        if !current.text.is_empty() && !same_style {
            runs.push(std::mem::replace(
                &mut current,
                StyledRun {
                    text: String::new(),
                    fg_css: String::new(),
                    bg_css: String::new(),
                    flag_style: String::new(),
                    is_cursor: false,
                },
            ));
        }

        current.fg_css = fg;
        current.bg_css = bg;
        current.flag_style = fs;
        current.is_cursor = is_cursor;
        current.text.push(cell.character);

        // For wide chars, also push a space for visual width
        if cell.wide {
            current.text.push(' ');
        }
    }

    if !current.text.is_empty() {
        runs.push(current);
    }

    runs
}

#[component]
pub fn TerminalView(
    session_id: String,
    render_output: RenderOutput,
    version: u64,
    suggestion: Option<String>,
    on_input: EventHandler<Vec<u8>>,
    on_command: EventHandler<String>,
    on_resize: EventHandler<(u16, u16, u32, u32)>,
    on_scroll_up: EventHandler<usize>,
    on_scroll_down: EventHandler<usize>,
    on_scroll_to_bottom: EventHandler<()>,
) -> Element {
    let mut focused = use_signal(|| false);
    let mut search_visible = use_signal(|| false);
    let mut search_query = use_signal(String::new);
    let mut search_match_index = use_signal(|| 0usize);
    let mut search_matches: Signal<Vec<(usize, usize)>> = use_signal(Vec::new); // (row, col) pairs

    let handle_keydown = move |e: KeyboardEvent| {
        let key = e.key();
        let code = e.code();
        let mods = e.modifiers();
        let ctrl = mods.ctrl();
        let alt = mods.alt();
        let meta = mods.meta();
        let shift = mods.shift();

        // Don't consume OS shortcuts (Cmd on macOS, Win on Windows, Super on Linux)
        if meta {
            return;
        }

        // Always prevent default to stop WebView from intercepting shortcuts
        e.prevent_default();

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

        // If search bar is visible, handle search navigation
        if search_visible() {
            if matches!(key, Key::Enter) {
                // Enter in search: go to next match
                let matches = search_matches();
                if !matches.is_empty() {
                    let idx = search_match_index();
                    let next = (idx + 1) % matches.len();
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
            // Don't process other keys when search is focused (let the input handle them)
            return;
        }

        // Ctrl+Shift+C: copy selection to clipboard
        if ctrl && shift && matches!(key, Key::Character(ref s) if s == "c" || s == "C") {
            spawn(async move {
                let _ = dioxus::document::eval(
                    "navigator.clipboard.writeText(window.getSelection().toString())"
                ).await;
            });
            return;
        }

        // Ctrl+Shift+V: paste from clipboard (with bracketed paste support)
        if ctrl && shift && matches!(key, Key::Character(ref s) if s == "v" || s == "V") {
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

        // Shift+Insert: paste from clipboard
        if shift && matches!(key, Key::Insert) {
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

        // Detect Enter key to capture command
        let is_enter = !ctrl && !alt && matches!(key, Key::Enter);

        let app_cursor = render_output.mode_cursor_keys;

        // Compute xterm CSI modifier number from modifier key state
        let modifier: Option<u8> = match (ctrl, alt, shift) {
            (false, false, false) => None,
            (false, false, true)  => Some(2),  // Shift
            (false, true,  false) => Some(3),  // Alt
            (false, true,  true)  => Some(4),  // Alt+Shift
            (true,  false, false) => Some(5),  // Ctrl
            (true,  false, true)  => Some(6),  // Ctrl+Shift
            (true,  true,  false) => Some(7),  // Ctrl+Alt
            (true,  true,  true)  => Some(8),  // Ctrl+Alt+Shift
        };

        let data: Vec<u8> = match key {
            // ── Arrow keys (with DECCKM and modifier support) ──
            Key::ArrowUp    => cursor_key_seq(1, b'A', app_cursor, modifier),
            Key::ArrowDown  => cursor_key_seq(1, b'B', app_cursor, modifier),
            Key::ArrowRight => cursor_key_seq(1, b'C', app_cursor, modifier),
            Key::ArrowLeft  => cursor_key_seq(1, b'D', app_cursor, modifier),

            // ── Navigation keys (with modifier support) ──
            Key::Home     => csi_seq(1, modifier, b'H'),
            Key::End      => csi_seq(1, modifier, b'F'),
            Key::Insert   => csi_seq(2, modifier, b'~'),
            Key::Delete   => csi_seq(3, modifier, b'~'),
            Key::PageUp   => csi_seq(5, modifier, b'~'),
            Key::PageDown => csi_seq(6, modifier, b'~'),

            // ── Function keys F1-F4 (with DECCKM and modifier support) ──
            Key::F1 => cursor_key_seq(1, b'P', app_cursor, modifier),
            Key::F2 => cursor_key_seq(1, b'Q', app_cursor, modifier),
            Key::F3 => cursor_key_seq(1, b'R', app_cursor, modifier),
            Key::F4 => cursor_key_seq(1, b'S', app_cursor, modifier),

            // ── Function keys F5-F12 (CSI form with modifier support) ──
            Key::F5  => csi_seq(15, modifier, b'~'),
            Key::F6  => csi_seq(17, modifier, b'~'),
            Key::F7  => csi_seq(18, modifier, b'~'),
            Key::F8  => csi_seq(19, modifier, b'~'),
            Key::F9  => csi_seq(20, modifier, b'~'),
            Key::F10 => csi_seq(21, modifier, b'~'),
            Key::F11 => csi_seq(23, modifier, b'~'),
            Key::F12 => csi_seq(24, modifier, b'~'),

            // ── Ctrl-only + character keys: generate control characters ──
            Key::Character(ref s) if ctrl && !alt && !shift => {
                ctrl_char(s)
            }

            // ── Alt+character: ESC prefix ──
            Key::Character(ref s) if alt && !ctrl => {
                let mut buf = vec![0x1b];
                buf.extend_from_slice(s.as_bytes());
                buf
            }

            // ── Ctrl+Shift+character: CSI 1;6 form ──
            Key::Character(ref s) if ctrl && shift && !alt => {
                let c = s.chars().next().unwrap_or('A');
                if c.is_ascii_alphabetic() {
                    csi_seq(1, Some(6), c as u8)
                } else {
                    // Use physical key code for numbers/symbols
                    let base = code_to_char(&code);
                    csi_seq(1, Some(6), base)
                }
            }

            // ── Ctrl+Alt+character: ESC + control character ──
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

            // ── Special keys with modifier support ──
            Key::Enter     => if alt { vec![0x1b, 0x0d] } else { vec![0x0d] },
            Key::Backspace => if alt { vec![0x1b, 0x7f] } else { vec![0x7f] },
            Key::Tab       => vec![0x09],
            Key::Escape    => vec![0x1b],

            // ── Plain character input ──
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

    // Auto-focus when session changes
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

    // Window focus/blur handler: re-focus terminal only when no other
    // interactive element (input, button, dialog) has focus
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
                        active.tagName === 'INPUT' ||
                        active.tagName === 'BUTTON' ||
                        active.tagName === 'SELECT' ||
                        active.tagName === 'TEXTAREA' ||
                        active.closest('[role="dialog"]')
                    );
                    if (!isInteractive) {{
                        el.focus();
                    }}
                }};
                el._windowBlurHandler = function() {{
                    if (document.activeElement === el) {{
                        el.blur();
                    }}
                }};
                window.addEventListener('focus', el._windowFocusHandler);
                window.addEventListener('blur', el._windowBlurHandler);
            }})()
        "#);
        spawn(async move {
            let _ = dioxus::document::eval(&script).await;
        });
    });

    // Resize: poll DOM for container size and call on_resize directly
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
                    "return (function() {{ const el = document.getElementById('{measure_cid}'); if (!el) return 'no-el'; const rect = el.getBoundingClientRect(); if (rect.width <= 0 || rect.height <= 0) return 'zero'; const cs = getComputedStyle(el); const padH = parseFloat(cs.paddingLeft) + parseFloat(cs.paddingRight); const padV = parseFloat(cs.paddingTop) + parseFloat(cs.paddingBottom); const bw = parseFloat(cs.borderLeftWidth) + parseFloat(cs.borderRightWidth); const bh = parseFloat(cs.borderTopWidth) + parseFloat(cs.borderBottomWidth); const w = rect.width - padH - bw; const h = rect.height - padV - bh; if (w <= 0 || h <= 0) return 'small'; const test = document.createElement('span'); test.textContent = 'M'; test.style.cssText = 'font-family:JetBrains Mono,Fira Code,Cascadia Code,monospace;font-size:13px;line-height:1.5;position:absolute;visibility:hidden;white-space:pre;'; document.body.appendChild(test); const tr = test.getBoundingClientRect(); document.body.removeChild(test); const cw = Math.max(1, tr.width); const ch = Math.max(1, tr.height); const cols = Math.max(1, Math.floor(w / cw)); const rows = Math.max(1, Math.floor(h / ch)); return cols + ',' + rows + ',' + cw.toFixed(2) + ',' + ch.toFixed(2); }})()"
                )).await;
                if let Ok(value) = result {
                    if let Some(s) = value.as_str() {
                        if s == "no-el" || s == "zero" || s == "small" || s.is_empty() {
                            continue;
                        }
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

    // Auto-scroll to cursor only when at bottom (scroll_offset == 0) and new output arrives
    let sid_for_scroll = session_id.clone();
    let current_scroll_offset = render_output.scrollback_offset;
    use_effect(move || {
        let _ = version;
        // Only auto-scroll when user is at the bottom (not scrolled up)
        if current_scroll_offset > 0 {
            return;
        }
        let sid = sid_for_scroll.clone();
        let cid = format!("terminal-input-{sid}");
        spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(16)).await;
            let _ = dioxus::document::eval(&format!(
                "document.getElementById('{cid}')?.scrollTo({{ top: 999999, behavior: 'smooth' }})"
            )).await;
        });
    });

    let cursor_row = render_output.cursor_row;
    let cursor_col = render_output.cursor_col;
    let cursor_visible = render_output.cursor_visible;

    // Recompute search matches when query or render output changes
    {
        let query = search_query();
        let _ = version; // depend on render output changes
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

    rsx! {
        div {
            id: "{container_id}",
            style: "
                position: absolute;
                left: 0; right: 0; top: 0; bottom: 0;
                background: #1a1b26;
                padding: 8px 12px;
                overflow-y: auto;
                font-family: 'JetBrains Mono', 'Fira Code', 'Cascadia Code', monospace;
                font-size: 13px;
                line-height: 1.5;
                color: #c0caf5;
                outline: none;
                cursor: text;
                box-sizing: border-box;
                -webkit-appearance: none;
                appearance: none;
            ",
            // Force-remove macOS focus ring via inline JS after mount
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
                                position: sticky;
                                top: 0;
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
                                style: "
                                    background: none;
                                    border: none;
                                    color: #565f89;
                                    cursor: pointer;
                                    font-size: 14px;
                                    padding: 0 4px;
                                ",
                                onclick: move |_| {
                                    let matches = search_matches();
                                    if !matches.is_empty() {
                                        let next = (search_match_index() + 1) % matches.len();
                                        search_match_index.set(next);
                                    }
                                },
                                "\u{25BC}" // ▼
                            }
                            button {
                                style: "
                                    background: none;
                                    border: none;
                                    color: #565f89;
                                    cursor: pointer;
                                    font-size: 14px;
                                    padding: 0 4px;
                                ",
                                onclick: move |_| {
                                    let matches = search_matches();
                                    if !matches.is_empty() {
                                        let prev = if search_match_index() == 0 { matches.len() - 1 } else { search_match_index() - 1 };
                                        search_match_index.set(prev);
                                    }
                                },
                                "\u{25B2}" // ▲
                            }
                            button {
                                style: "
                                    background: none;
                                    border: none;
                                    color: #565f89;
                                    cursor: pointer;
                                    font-size: 14px;
                                    padding: 0 4px;
                                ",
                                onclick: move |_| {
                                    search_visible.set(false);
                                    search_query.set(String::new());
                                    search_matches.set(Vec::new());
                                    search_match_index.set(0);
                                },
                                "\u{2715}" // ✕
                            }
                        }
                    }
                }
            }

            div {
                id: "{scroll_id}",
                style: "",

                for (row_idx, row) in render_output.rows.iter().enumerate() {
                    {
                        let is_cursor_row = row_idx == cursor_row && cursor_visible;
                        let cur_col = if is_cursor_row { Some(cursor_col) } else { None };
                        let runs = row_to_runs(row, cur_col);
                        let row_key = format!("{session_id}-{row_idx}");
                        let inline_suggestion = if is_cursor_row { suggestion.clone() } else { None };

                        // Check if this row has a search match
                        let sm = search_matches();
                        let sidx = search_match_index();
                        let is_search_match = sm.iter().any(|(r, _)| *r == row_idx);
                        let is_current_match = sm.get(sidx).map(|(r, _)| *r == row_idx).unwrap_or(false);
                        let row_bg = if is_current_match {
                            "background:rgba(122,162,247,0.2);"
                        } else if is_search_match {
                            "background:rgba(122,162,247,0.08);"
                        } else {
                            ""
                        };

                        rsx! {
                            div {
                                key: "{row_key}",
                                style: "white-space: pre; line-height: 1.5; width: 100%;{row_bg}",

                                for (run_idx, run) in runs.iter().enumerate() {
                                    {
                                        let mut style_parts = vec![];
                                        if !run.fg_css.is_empty() {
                                            style_parts.push(format!("color:{}", run.fg_css));
                                        }
                                        if !run.bg_css.is_empty() {
                                            style_parts.push(format!("background:{}", run.bg_css));
                                        }
                                        if !run.flag_style.is_empty() {
                                            style_parts.push(run.flag_style.clone());
                                        }
                                        if run.is_cursor {
                                            style_parts.push("border-left:2px solid #c0caf5;margin-left:-1px".to_string());
                                        }
                                        let style = style_parts.join(";");
                                        let text = run.text.clone();
                                        let run_key = format!("{row_key}-{run_idx}");

                                        rsx! {
                                            span {
                                                key: "{run_key}",
                                                style: "{style}",
                                                "{text}"
                                            }
                                        }
                                    }
                                }

                                // Inline suggestion (Fish-shell style)
                                if let Some(ref sug) = inline_suggestion {
                                    span {
                                        style: "color:#565f89;opacity:0.6;",
                                        "{sug}"
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
