use dioxus::html::input_data::MouseButton;
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
    /// CSS length used for `top` when the measured `--suggestion-top` /
    /// `--suggestion-popup-top` variables are not (yet) set. TerminalView
    /// passes a pixel offset computed from the cursor row so the popup opens
    /// below the prompt from its very first frame; defaults to `2em`.
    #[props(default = "2em".to_string())]
    fallback_top: String,
    /// Fired when the user presses the drag grip with the primary button.
    /// Payload: the viewport `clientY` of the press. TerminalView installs
    /// the document-level drag listeners that actually move the popup.
    #[props(default)]
    on_drag_start: EventHandler<f64>,
    /// Fired when the user double-clicks the grip: forget the remembered
    /// position and return to automatic placement.
    #[props(default)]
    on_position_reset: EventHandler<()>,
) -> Element {
    let _lang = crate::i18n::LANGUAGE();
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
                top: var(--suggestion-popup-top, var(--suggestion-top, {fallback_top}));
                bottom: var(--suggestion-popup-bottom, auto);
                max-height: var(--suggestion-popup-max-height, calc(100% - var(--suggestion-top, {fallback_top})));
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

            // Drag grip — same drag-to-move + remembered-position behavior as
            // SuggestionPopup. Kept clear of the absolutely-positioned ×
            // cancel button on the right so both stay clickable.
            div {
                style: "
                    display:flex;
                    align-items:center;
                    justify-content:center;
                    height:12px;
                    margin-right:26px;
                    cursor:grab;
                    background:#1a1b26;
                    border-bottom:1px solid #2a2b3d;
                    color:#565f89;
                    font-size:9px;
                    letter-spacing:3px;
                    line-height:1;
                    user-select:none;
                    -webkit-user-select:none;
                ",
                title: crate::i18n::t("popup.drag_grip_tooltip"),
                onpointerdown: move |e| {
                    e.prevent_default();
                    e.stop_propagation();
                },
                onmousedown: move |e| {
                    e.prevent_default();
                    e.stop_propagation();
                    if e.trigger_button() == Some(MouseButton::Primary) {
                        on_drag_start.call(e.client_coordinates().y);
                    }
                },
                onmouseup: move |e: Event<MouseData>| e.stop_propagation(),
                onclick: move |e: Event<MouseData>| e.stop_propagation(),
                ondoubleclick: move |e| {
                    e.stop_propagation();
                    on_position_reset.call(());
                },
                "•••"
            }

            button {
                r#type: "button",
                aria_label: crate::i18n::t("onekey.popup.cancel"),
                title: crate::i18n::t("onekey.popup.cancel_tooltip"),
                style: "position:absolute;right:6px;top:4px;z-index:1;border:0;background:transparent;color:#9aa5ce;font:inherit;font-size:14px;font-weight:700;cursor:pointer;padding:0 5px;",
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
                    { crate::i18n::t("onekey.popup.rejected") }
                }
            }

            for (i, m) in visible.iter().enumerate() {
                {
                    let is_sel = i == selected;
                    let bg = if is_sel { "#283457" } else { "transparent" };
                    let fg = if is_sel { "#c0caf5" } else { "#a9b1d6" };
                    let border_left = if is_sel { "border-left:2px solid #9ece6a;" } else { "border-left:2px solid transparent;" };
                    let raw_label = m.label.trim();
                    let label = match raw_label {
                        "" | "Credential" => crate::i18n::t("onekey.credential"),
                        "Password" => crate::i18n::t("onekey.credential_password"),
                        "Token" => crate::i18n::t("onekey.credential_token"),
                        "Username" => crate::i18n::t("onekey.credential_username"),
                        _ => raw_label.to_string(),
                    };
                    let credential_hint = format!("{} {}", raw_label, m.matched_expect).to_lowercase();
                    let (credential_key, badge_color) = if credential_hint.contains("password")
                        || credential_hint.contains("passwd")
                        || credential_hint.contains("secret")
                        || credential_hint.contains("passphrase")
                        || credential_hint.contains("pwd")
                    {
                        ("onekey.credential_password", "#f7768e")
                    } else if credential_hint.contains("token") || credential_hint.contains("otp") {
                        ("onekey.credential_token", "#e0af68")
                    } else {
                        ("onekey.credential_username", "#9ece6a")
                    };
                    let badge_title = crate::i18n::t(credential_key);
                    let badge = badge_title.chars().next().unwrap_or('?');
                    let use_title = crate::i18n::tf(
                        "onekey.popup.use_credential",
                        &[("name", &m.name), ("label", &label)],
                    );
                    rsx! {
                        div {
                            key: "{i}",
                            style: "display:flex;align-items:center;padding:5px 32px 5px 12px;{border_left}background:{bg};color:{fg};cursor:pointer;overflow:hidden;",
                            title: "{use_title}",
                            onmouseenter: move |_| on_highlight.call(i),
                            onclick: move |_| on_select.call(i),
                            span {
                                style: "display:flex;flex:1;min-width:0;align-items:baseline;gap:8px;",
                                span { style: "overflow:hidden;text-overflow:ellipsis;white-space:nowrap;", "{m.name}" }
                                span { style: "color:#9aa5ce;font-size:11px;white-space:nowrap;", "{label}" }
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
                style: "display:flex;align-items:center;padding:4px 12px;border-top:1px solid #2a2b3d;color:#9aa5ce;cursor:pointer;",
                onclick: move |_| on_save.call(()),
                span { style: "flex:1;", { crate::i18n::t("onekey.popup.save") } }
                span { style: "color:#7aa2f7;", "+" }
            }
            div { style: "display:none;", onclick: move |_| on_dismiss.call(()), "" }
        }
    }
}
