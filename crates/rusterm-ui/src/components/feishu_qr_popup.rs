//! Floating status popup for the Feishu OAuth sign-in (issues #129/#130).
//!
//! The actual QR code is rendered by Feishu itself inside the embedded
//! browser window (`crate::feishu_browser`) — the phone scan must authorize
//! the desktop webview so the OAuth redirect can reach the loopback
//! listener. This popup only tracks flow status (scanning / delivered /
//! failed) and offers recovery actions. Once the backend obtains tokens,
//! the app event loop closes this popup automatically; a settings-initiated
//! attempt shows the success/failure banner inline and only closes on user
//! action.

use dioxus::prelude::*;

use crate::state::{FeishuQrPopup, FeishuQrPopupStatus};

/// Render the Feishu sign-in status popup. Never displays the authorize URL
/// itself in the UI — the embedded window / system browser carries it — but
/// the "open in browser" button offers a launcher-based fallback.
#[component]
pub fn FeishuQrPopupView(
    popup: FeishuQrPopup,
    /// Fires when the user cancels / dismisses the popup.
    on_close: EventHandler<()>,
    /// Fires when the user asks to reopen the embedded sign-in window.
    on_embedded: EventHandler<()>,
    /// Fires when the user asks to reopen the authorize URL in a browser.
    on_browser: EventHandler<()>,
    /// Fires when the user re-scans after expiration/failure.
    on_rescan: EventHandler<()>,
) -> Element {
    let status = popup.status.clone();
    let is_settings_session = popup.session.is_none();

    let (status_bg, status_fg, status_key) = match &status {
        FeishuQrPopupStatus::Scanning { started } => {
            let expired = crate::feishu_oauth_flow::qr_expired(&popup, std::time::Instant::now());
            if expired {
                ("background:#3b2430;", "#f7768e", "feishu.qr_expired")
            } else {
                let _ = started;
                ("background:#1a224a;", "#7aa2f7", "feishu.qr_fetching")
            }
        }
        FeishuQrPopupStatus::Delivered { .. } => {
            ("background:#163a2d;", "#9ece6a", "feishu.qr_delivered")
        }
        FeishuQrPopupStatus::Failed { .. } => {
            ("background:#3b2430;", "#f7768e", "feishu.qr_failed")
        }
    };
    let status_text = match &status {
        FeishuQrPopupStatus::Failed { reason, .. } => {
            crate::i18n::tf(status_key, &[("reason", reason)])
        }
        _ => crate::i18n::t(status_key),
    };

    rsx! {
        div {
            "data-rusterm-feishu-qr": "true",
            style: "
                position: fixed; inset: 0;
                background: rgba(0,0,0,0.55);
                display: flex; align-items: center; justify-content: center;
                z-index: 2200;
                font-family: 'Segoe UI', system-ui, sans-serif;
            ",
            onclick: move |_| {
                // Backdrop dismisses the popup.
                on_close.call(());
            },
            div {
                onclick: move |e| e.stop_propagation(),
                style: "
                    background: #1a1b26;
                    border: 1px solid #2a2b3d;
                    border-radius: 12px;
                    padding: 24px;
                    width: 340px;
                    color: #c0caf5;
                    box-shadow: 0 8px 40px rgba(0,0,0,0.6);
                ",
                // Title row
                div {
                    style: "display: flex; align-items: center; justify-content: space-between; margin-bottom: 6px;",
                    span {
                        style: "font-size: 15px; font-weight: 600;",
                        { crate::i18n::t("feishu.qr_title") }
                    }
                    button {
                        r#type: "button",
                        style: "background: transparent; border: none; color: #565f89; cursor: pointer; font-size: 16px; padding: 0;",
                        onclick: move |e| {
                            e.stop_propagation();
                            on_close.call(());
                        },
                        "×"
                    }
                }
                p {
                    style: "margin: 0 0 14px; font-size: 12px; color: #787c99; line-height: 1.5;",
                    { crate::i18n::t(if is_settings_session { "feishu.qr_subtitle_settings" } else { "feishu.qr_subtitle_session" }) }
                }

                // Instruction panel — the QR itself lives in the embedded
                // Feishu window; this block just points the user there.
                div {
                    style: "
                        background: #16161e;
                        border: 1px dashed #2a2b3d;
                        border-radius: 8px;
                        padding: 16px 14px;
                        margin-bottom: 14px;
                        text-align: center;
                    ",
                    div {
                        style: "font-size: 30px; margin-bottom: 8px;",
                        "\u{1F4F1}"
                    }
                    div {
                        style: "font-size: 12px; color: #a9b1d6; line-height: 1.7; white-space: pre-line;",
                        { crate::i18n::t("feishu.qr_embedded_hint") }
                    }
                }

                // Status banner (fetching/delivered/failed/expired)
                div {
                    style: "{status_bg} color: {status_fg}; font-size: 12px; padding: 8px 10px; border-radius: 6px; margin-bottom: 12px; line-height: 1.4;",
                    "{status_text}"
                }

                // Action row
                div {
                    style: "display: flex; flex-direction: column; gap: 8px;",
                    button {
                        r#type: "button",
                        style: "
                            background: #7aa2f7; color: #1a1b26; border: none; border-radius: 6px;
                            padding: 8px; font-size: 12px; font-weight: 600; cursor: pointer;
                        ",
                        onclick: move |e| {
                            e.stop_propagation();
                            on_embedded.call(());
                        },
                        { crate::i18n::t("feishu.qr_open_embedded") }
                    }
                    button {
                        r#type: "button",
                        style: "
                            background: transparent; color: #a9b1d6; border: 1px solid #2a2b3d;
                            border-radius: 6px; padding: 8px; font-size: 12px; cursor: pointer;
                        ",
                        onclick: move |e| {
                            e.stop_propagation();
                            on_browser.call(());
                        },
                        { crate::i18n::t("feishu.qr_open") }
                    }
                    button {
                        r#type: "button",
                        style: "
                            background: transparent; color: #a9b1d6; border: 1px solid #2a2b3d;
                            border-radius: 6px; padding: 8px; font-size: 12px; cursor: pointer;
                        ",
                        onclick: move |e| {
                            e.stop_propagation();
                            on_rescan.call(());
                        },
                        { crate::i18n::t("feishu.qr_rescan") }
                    }
                }

                // Help footer
                p {
                    style: "margin: 12px 0 0; font-size: 11px; color: #565f89; line-height: 1.6; white-space: pre-line;",
                    { crate::i18n::t("feishu.qr_scan_help") }
                }
            }
        }
    }
}
