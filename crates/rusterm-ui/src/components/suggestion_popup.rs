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

/// Large Atuin-style suggestion panel rendered at the bottom of the terminal.
/// Shows matching history commands sorted by frequency, with the selected
/// item highlighted. Appears automatically as the user types.
#[component]
pub fn SuggestionPopup(
    suggestions: Vec<String>,
    selected_index: usize,
    on_select: EventHandler<String>,
    on_dismiss: EventHandler<()>,
) -> Element {
    if suggestions.is_empty() {
        return rsx! {};
    }

    let current_selected = selected_index;

    // Build rows HTML
    let items_html = suggestions.iter().enumerate().map(|(i, cmd)| {
        let is_selected = i == current_selected;
        let bg = if is_selected { "#283457" } else { "transparent" };
        let fg = if is_selected { "#c0caf5" } else { "#a9b1d6" };
        let left_border = if is_selected { "border-left:2px solid #7aa2f7;" } else { "border-left:2px solid transparent;" };
        let escaped = html_escape(cmd);

        format!(
            "<div style=\"display:flex;align-items:center;padding:3px 12px;{left_border}background:{bg};color:{fg};cursor:pointer;white-space:pre;overflow:hidden;text-overflow:ellipsis;\">{escaped}</div>"
        )
    }).collect::<Vec<_>>().join("");

    rsx! {
        div {
            style: "
                position: absolute;
                left: 0; right: 0; bottom: 0;
                max-height: 40%;
                overflow-y: auto;
                background: #16161e;
                border-top: 1px solid #2a2b3d;
                font-family: 'JetBrains Mono', 'Fira Code', 'Cascadia Code', monospace;
                font-size: 13px;
                line-height: 1.5;
                z-index: 20;
                scrollbar-width: thin;
                scrollbar-color: #2a2b3d transparent;
            ",
            onclick: move |e| e.stop_propagation(),
            dangerous_inner_html: "{items_html}",
        }
    }
}
