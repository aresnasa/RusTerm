use dioxus::prelude::*;

use crate::components::suggestion_popup::MAX_VISIBLE_ROWS;
use crate::state::{OneKeyMatch, OneKeySubmissionFeedback};

/// OneKey autofill popup. Renders below the current cursor row using the
/// same pane-relative coordinate as command suggestions. Secrets are never
/// rendered: the popup only emits the selected index, which the owning
/// session resolves back to the current match before sending it to the PTY.
///
/// At most [`MAX_VISIBLE_ROWS`] entries are shown so the popup stays compact
/// and never blocks the terminal view.
#[component]
pub fn OneKeyPopup(
    entries: Vec<OneKeyMatch>,
    selected: usize,
    submission_feedback: Option<OneKeySubmissionFeedback>,
    on_highlight: EventHandler<usize>,
    on_select: EventHandler<usize>,
    on_save: EventHandler<()>,
    on_dismiss: EventHandler<()>,
) -> Element {
    let rejected = matches!(
        submission_feedback,
        Some(OneKeySubmissionFeedback::Rejected { .. })
    );

    // Cap to the first MAX_VISIBLE_ROWS entries so the popup stays compact.
    // TerminalView applies the same cap when resolving the selected index.
    let visible: Vec<&OneKeyMatch> = entries.iter().take(MAX_VISIBLE_ROWS).collect();
    let selected = selected.min(visible.len().saturating_sub(1));

    rsx! {
        div {
            "data-rusterm-terminal-popup": "true",
            style: "
                position: absolute;
                left: 0; right: 0;
                top: var(--suggestion-top, 2em);
                max-height: calc(100% - var(--suggestion-top, 2em));
                overflow-y: auto;
                background: #16161e;
                border: 1px solid #2a2b3d;
                border-top: none;
                box-shadow: 0 4px 16px rgba(0,0,0,0.4);
                font-family: 'JetBrains Mono', 'Fira Code', 'Cascadia Code', monospace;
                font-size: 13px;
                line-height: 1.5;
                z-index: 20;
            ",
            // Keep popup clicks out of TerminalView's selection/mouse-reporting
            // handlers. In mouse-reporting mode a leaked click would otherwise
            // be sent to the remote application before the credential.
            onpointerdown: move |e: Event<PointerData>| {
                e.prevent_default();
                e.stop_propagation();
            },
            onmousedown: move |e: Event<MouseData>| {
                e.prevent_default();
                e.stop_propagation();
            },
            onclick: move |e: Event<MouseData>| e.stop_propagation(),

            button {
                r#type: "button",
                aria_label: "Cancel credential popup",
                title: "Cancel credential popup (Escape)",
                style: "position:absolute;right:6px;top:4px;z-index:1;border:0;background:transparent;color:#565f89;font:inherit;font-size:14px;font-weight:700;cursor:pointer;padding:0 5px;",
                onpointerdown: move |e| {
                    e.prevent_default();
                    e.stop_propagation();
                },
                onmousedown: move |e| {
                    e.prevent_default();
                    e.stop_propagation();
                },
                onclick: move |e| {
                    e.stop_propagation();
                    on_dismiss.call(());
                },
                "×"
            }

            if rejected {
                div {
                    style: "padding:6px 12px;color:#f7768e;background:rgba(247,118,142,0.08);border-bottom:1px solid #2a2b3d;font-size:11px;",
                    "Credential was sent, but the remote requested it again. Verify the saved value."
                }
            }

            for (i, m) in visible.iter().enumerate() {
                {
                    let is_sel = i == selected;
                    let bg = if is_sel { "#283457" } else { "transparent" };
                    let fg = if is_sel { "#c0caf5" } else { "#a9b1d6" };
                    let border_left = if is_sel { "border-left:2px solid #9ece6a;" } else { "border-left:2px solid transparent;" };
                    let label = if m.label.trim().is_empty() { "Credential" } else { m.label.trim() };
                    let label_lower = label.to_lowercase();
                    let (badge, badge_color, badge_title) = if label_lower.contains("password")
                        || label_lower.contains("passwd")
                        || label_lower.contains("secret")
                        || label_lower.contains("passphrase")
                        || label_lower.contains("pwd")
                    {
                        ("P", "#f7768e", "Password / secret")
                    } else if label_lower.contains("token") || label_lower.contains("otp") {
                        ("T", "#e0af68", "Token / OTP")
                    } else {
                        ("U", "#9ece6a", "Username / account")
                    };
                    rsx! {
                        div {
                            key: "{i}",
                            style: "display:flex;align-items:center;padding:5px 32px 5px 12px;{border_left}background:{bg};color:{fg};cursor:pointer;overflow:hidden;",
                            title: "Use {m.name} · {label} (Enter or Tab)",
                            onmouseenter: move |_| on_highlight.call(i),
                            onclick: move |_| on_select.call(i),
                            span {
                                style: "display:flex;flex:1;min-width:0;align-items:baseline;gap:8px;",
                                span { style: "overflow:hidden;text-overflow:ellipsis;white-space:nowrap;", "{m.name}" }
                                span { style: "color:#565f89;font-size:11px;white-space:nowrap;", "{label}" }
                            }
                            span {
                                style: "color:{badge_color};font-size:10px;margin-left:8px;font-weight:700;border:1px solid {badge_color};border-radius:3px;padding:0 4px;",
                                title: "{badge_title}",
                                "{badge}"
                            }
                        }
                    }
                }
            }
            div {
                style: "display:flex;align-items:center;padding:4px 12px;border-top:1px solid #2a2b3d;color:#565f89;cursor:pointer;",
                onclick: move |_| on_save.call(()),
                span { style: "flex:1;", "Save In OneKeys" }
                span { style: "color:#7aa2f7;", "+" }
            }
            div { style: "display:none;", onclick: move |_| on_dismiss.call(()), "" }
        }
    }
}
