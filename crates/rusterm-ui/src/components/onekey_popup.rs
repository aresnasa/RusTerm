use dioxus::prelude::*;

use crate::state::OneKeyMatch;

/// OneKey autofill popup. Renders BELOW the current cursor row (via the
/// `--suggestion-top` CSS variable, kept current by the resize future) so it
/// doesn't obscure the command line the user is typing on. Lists matching
/// entries (different users/accounts); selecting one sends its `send` value + Enter.
///
/// `--suggestion-top` is the Y coordinate (in px, relative to the terminal
/// container's top-left) of the cursor row's BOTTOM edge. The popup's top edge
/// is placed there, so the popup grows downward into the space below the
/// cursor row. `max-height: 45%` keeps it from overflowing the terminal if the
/// cursor is near the bottom; the popup becomes scrollable in that case.
/// Falls back to `top: auto` (i.e. flow-positioned) if `--suggestion-top`
/// hasn't been set yet (first render before the resize future runs).
#[component]
pub fn OneKeyPopup(
    entries: Vec<OneKeyMatch>,
    selected: usize,
    on_select: EventHandler<String>,
    on_save: EventHandler<()>,
    on_dismiss: EventHandler<()>,
) -> Element {
    rsx! {
        div {
            style: "
                position: absolute;
                left: 0; right: 0;
                top: var(--suggestion-top, auto);
                bottom: auto;
                max-height: 45%;
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
            for (i, m) in entries.iter().enumerate() {
                {
                    let is_sel = i == selected;
                    let bg = if is_sel { "#283457" } else { "transparent" };
                    let fg = if is_sel { "#c0caf5" } else { "#a9b1d6" };
                    let border_left = if is_sel { "border-left:2px solid #9ece6a;" } else { "border-left:2px solid transparent;" };
                    let send_val = m.send.clone();
                    // Badge reflects the kind of credential this entry carries.
                    // Heuristic on the entry's name: "password"/"pass"/"pwd" → P
                    // (secret, masked), "token" → T, otherwise U (username-like).
                    // Saves the user from accidentally sending a password into a
                    // username field when both kinds of entries match a prompt.
                    let name_lower = m.name.to_lowercase();
                    let (badge, badge_color, badge_title) = if name_lower.contains("password")
                        || name_lower.contains("passwd")
                        || name_lower.contains(" pass")
                        || name_lower.ends_with(" pass")
                        || name_lower.contains("pwd")
                    {
                        ("P", "#f7768e", "Password / secret")
                    } else if name_lower.contains("token") || name_lower.contains("otp") {
                        ("T", "#e0af68", "Token / OTP")
                    } else {
                        ("U", "#9ece6a", "Username / account")
                    };
                    rsx! {
                        div {
                            key: "{i}",
                            style: "display:flex;align-items:center;padding:4px 12px;{border_left}background:{bg};color:{fg};cursor:pointer;white-space:pre;overflow:hidden;text-overflow:ellipsis;",
                            onclick: move |_| on_select.call(send_val.clone()),
                            span { style: "flex:1;", "{m.name}" }
                            span {
                                style: "color:{badge_color};font-size:10px;margin-left:8px;font-weight:700;border:1px solid {badge_color};border-radius:3px;padding:0 4px;",
                                title: "{badge_title}",
                                "{badge}"
                            }
                        }
                    }
                }
            }
            // Save In OneKeys row
            div {
                style: "display:flex;align-items:center;padding:4px 12px;border-top:1px solid #2a2b3d;color:#565f89;cursor:pointer;",
                onclick: move |_| on_save.call(()),
                span { style: "flex:1;", "Save In OneKeys" }
                span { style: "color:#7aa2f7;", "+" }
            }
            // Hidden dismiss hint (Escape handled in TerminalView)
            div { style: "display:none;", onclick: move |_| on_dismiss.call(()), "" }
        }
    }
}
