use dioxus::prelude::*;

/// Escape special HTML characters to prevent injection via `dangerous_inner_html`.
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

/// Dropdown list of command suggestions rendered below the cursor position.
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

    let top_px = (cursor_row + 1) * 20;
    let left_px = cursor_col * 8;

    let mut items_html = String::new();
    for (i, suggestion) in suggestions.iter().enumerate() {
        let is_selected = i == selected_index;
        let bg = if is_selected { "#283457" } else { "transparent" };
        let color = if is_selected { "#c0caf5" } else { "#a9b1d6" };
        let escaped = html_escape(suggestion);
        items_html.push_str(&format!(
            "<div style=\"padding:2px 8px;white-space:pre;overflow:hidden;text-overflow:ellipsis;background:{};color:{}\">{}</div>",
            bg, color, escaped
        ));
    }

    rsx! {
        div {
            style: "
                position: absolute;
                top: {top_px}px;
                left: {left_px}px;
                min-width: 300px;
                max-width: 600px;
                max-height: 192px;
                overflow-y: auto;
                background: #1a1b26;
                border: 1px solid #2a2b3d;
                border-radius: 4px;
                box-shadow: 0 4px 12px rgba(0,0,0,0.4);
                font-family: 'JetBrains Mono', 'Fira Code', 'Cascadia Code', monospace;
                font-size: 13px;
                line-height: 1.5;
                z-index: 20;
            ",
            onclick: move |e| e.stop_propagation(),
            dangerous_inner_html: "{items_html}",
        }
    }
}
