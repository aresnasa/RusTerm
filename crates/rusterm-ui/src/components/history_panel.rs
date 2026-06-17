use dioxus::prelude::*;

use crate::state::HistoryPanelEntry;

#[component]
pub fn HistoryPanel(
    entries: Vec<HistoryPanelEntry>,
    on_accept: EventHandler<String>,
    on_close: EventHandler<()>,
) -> Element {
    let mut query = use_signal(String::new);
    let mut selected = use_signal(|| 0usize);

    // Filter entries by search query (owned to avoid borrow issues)
    let filtered: Vec<HistoryPanelEntry> = {
        let q = query().to_lowercase();
        entries.into_iter()
            .filter(|entry| {
                if q.is_empty() {
                    true
                } else {
                    entry.command.to_lowercase().contains(&q)
                        || entry.cwd.as_ref().map_or(false, |c| c.to_lowercase().contains(&q))
                }
            })
            .collect()
    };

    let filtered_len = filtered.len();

    // Clamp selected index
    if selected() >= filtered_len && filtered_len > 0 {
        selected.set(filtered_len.saturating_sub(1));
    }

    // Build rows HTML
    let current_selected = selected();
    let rows_html = filtered.iter().enumerate().map(|(i, entry)| {
        let is_selected = i == current_selected;

        let (dur_text, dur_color) = match entry.duration_ms {
            Some(ms) if ms < 1000 => (format!("{}ms", ms), "#9ece6a"),
            Some(ms) if ms < 10000 => (format!("{}s", ms / 1000), "#e0af68"),
            Some(ms) => (format!("{}s", ms / 1000), "#f7768e"),
            None => (String::from("-"), "#565f89"),
        };

        let time_text = format_relative_time(entry.timestamp.as_deref());

        let cmd_escaped = html_escape(&entry.command);
        let cwd_text = entry.cwd.as_deref().unwrap_or("");
        let cwd_escaped = html_escape(cwd_text);

        let bg = if is_selected { "#283457" } else { "transparent" };
        let left_border = if is_selected { "border-left:2px solid #7aa2f7;" } else { "border-left:2px solid transparent;" };

        format!(
            "<div style=\"display:flex;align-items:center;gap:12px;padding:4px 12px;{left_border}background:{bg};cursor:pointer;\">\
                <span style=\"color:{dur_color};min-width:60px;text-align:right;font-size:12px;\">{dur_text}</span>\
                <span style=\"color:#7aa2f7;min-width:70px;font-size:12px;\">{time_text}</span>\
                <span style=\"color:#c0caf5;flex:1;overflow:hidden;text-overflow:ellipsis;white-space:pre;\">{cmd_escaped}</span>\
                <span style=\"color:#565f89;font-size:11px;max-width:200px;overflow:hidden;text-overflow:ellipsis;white-space:pre;\">{cwd_escaped}</span>\
            </div>"
        )
    }).collect::<Vec<_>>().join("");

    // Capture for closures
    let filtered_for_keys = filtered.clone();
    let filtered_count = filtered_len;

    let handle_keydown = move |e: KeyboardEvent| {
        e.prevent_default();
        e.stop_propagation();
        let key = e.key();
        match key {
            Key::ArrowUp => {
                let cur = selected();
                if cur > 0 {
                    selected.set(cur - 1);
                }
            }
            Key::ArrowDown => {
                let cur = selected();
                if cur + 1 < filtered_count {
                    selected.set(cur + 1);
                }
            }
            Key::Enter => {
                if let Some(entry) = filtered_for_keys.get(selected()) {
                    on_accept.call(entry.command.clone());
                }
            }
            Key::Escape => {
                on_close.call(());
            }
            Key::Character(ref s) if e.modifiers().ctrl() && s.eq_ignore_ascii_case("r") => {
                on_close.call(());
            }
            _ => {}
        }
    };

    let query_val = query();

    rsx! {
        div {
            style: "
                position: fixed;
                top: 0; left: 0; right: 0; bottom: 0;
                z-index: 100;
                display: flex;
                justify-content: center;
                align-items: flex-start;
                padding-top: 10vh;
                background: rgba(0,0,0,0.6);
                backdrop-filter: blur(4px);
            ",
            onclick: move |_| {
                on_close.call(());
            },

            div {
                style: "
                    width: 90%;
                    max-width: 900px;
                    max-height: 75vh;
                    background: #1a1b26;
                    border: 1px solid #2a2b3d;
                    border-radius: 8px;
                    box-shadow: 0 8px 32px rgba(0,0,0,0.5);
                    display: flex;
                    flex-direction: column;
                    overflow: hidden;
                    font-family: 'JetBrains Mono', 'Fira Code', 'Cascadia Code', monospace;
                    font-size: 13px;
                    color: #c0caf5;
                ",
                onclick: move |e| e.stop_propagation(),

                // Header: search input
                div {
                    style: "
                        display: flex;
                        align-items: center;
                        gap: 8px;
                        padding: 10px 16px;
                        border-bottom: 1px solid #2a2b3d;
                        background: #16161e;
                    ",
                    span {
                        style: "color: #7aa2f7; font-size: 12px; white-space: nowrap;",
                        "Ctrl+R"
                    }
                    input {
                        r#type: "text",
                        value: "{query_val}",
                        placeholder: "Search command history...",
                        autofocus: true,
                        style: "
                            flex: 1;
                            background: transparent;
                            border: none;
                            color: #c0caf5;
                            font-family: 'JetBrains Mono', 'Fira Code', 'Cascadia Code', monospace;
                            font-size: 14px;
                            outline: none;
                        ",
                        oninput: move |e: FormEvent| {
                            query.set(e.value());
                            selected.set(0);
                        },
                        onkeydown: handle_keydown,
                    }
                    span {
                        style: "color: #565f89; font-size: 11px; white-space: nowrap;",
                        "{filtered_count} results"
                    }
                }

                // Command list
                div {
                    style: "
                        flex: 1;
                        overflow-y: auto;
                        max-height: 60vh;
                        scrollbar-width: thin;
                        scrollbar-color: #2a2b3d transparent;
                    ",
                    dangerous_inner_html: "{rows_html}",
                }

                // Footer
                div {
                    style: "
                        display: flex;
                        align-items: center;
                        gap: 16px;
                        padding: 6px 16px;
                        border-top: 1px solid #2a2b3d;
                        background: #16161e;
                        font-size: 11px;
                        color: #565f89;
                    ",
                    span { "Up/Down: navigate" }
                    span { "Enter: accept" }
                    span { "Esc: close" }
                    span {
                        style: "margin-left: auto; color: #9ece6a; border: 1px solid #9ece6a; border-radius: 3px; padding: 0 4px; font-size: 10px; letter-spacing: 0.5px;",
                        "LOCAL ONLY"
                    }
                }
            }
        }
    }
}

fn format_relative_time(timestamp: Option<&str>) -> String {
    let ts = match timestamp {
        Some(t) => t,
        None => return "-".to_string(),
    };

    let parsed = match chrono::DateTime::parse_from_rfc3339(ts) {
        Ok(p) => p.with_timezone(&chrono::Utc),
        Err(_) => return "-".to_string(),
    };

    let now = chrono::Utc::now();
    let diff = now.signed_duration_since(parsed);

    if diff.num_seconds() < 60 {
        "just now".to_string()
    } else if diff.num_minutes() < 60 {
        format!("{}m ago", diff.num_minutes())
    } else if diff.num_hours() < 24 {
        format!("{}h ago", diff.num_hours())
    } else if diff.num_days() < 30 {
        format!("{}d ago", diff.num_days())
    } else if diff.num_days() < 365 {
        format!("{}mo ago", diff.num_days() / 30)
    } else {
        format!("{}y ago", diff.num_days() / 365)
    }
}

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
