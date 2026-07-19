use dioxus::prelude::*;

/// Modal shown when the user presses Enter on a command that the
/// `CommandSafetyChecker` flagged as potentially destructive (e.g.
/// `rm -rf /`, `dd ... of=/dev/sda`, fork bomb, etc.).
///
/// Two actions:
/// - 继续 (Proceed): send the original Enter to the PTY (the user confirmed
///                   they really want to run this)
/// - 取消 (Cancel):  discard the Enter, keep the user's input line intact so
///                   they can edit or backspace
///
/// We deliberately use `Warn` (user can proceed) rather than `Block` (refuse
/// outright) for everything — even `rm -rf /`. The user might be in a
/// chroot, a container, or a VM where `rm -rf /` is the correct cleanup.
/// Refusing outright would just train them to bypass us. The point is to
/// force a conscious decision, not to nanny.
#[component]
pub fn DangerousCommandDialog(
    command: String,
    reason: String,
    on_proceed: EventHandler<()>,
    on_cancel: EventHandler<()>,
) -> Element {
    rsx! {
        div {
            style: "
                position: fixed;
                top: 0; left: 0; right: 0; bottom: 0;
                background: rgba(0, 0, 0, 0.7);
                display: flex;
                justify-content: center;
                align-items: center;
                z-index: 2100;
            ",

            div {
                style: "
                    background: #24283b;
                    border: 1px solid #f7768e;
                    border-radius: 8px;
                    padding: 28px;
                    width: 480px;
                    color: #c0caf5;
                    box-shadow: 0 8px 32px rgba(0, 0, 0, 0.5);
                ",

                // Warning header
                div {
                    style: "text-align: center; margin-bottom: 20px;",
                    h2 {
                        style: "margin: 0 0 8px; font-size: 18px; font-weight: 600; color: #f7768e;",
                        "⚠ 高危命令确认"
                    }
                    p {
                        style: "margin: 0; font-size: 12px; color: #565f89;",
                        "此命令可能造成不可逆破坏，请确认后继续"
                    }
                }

                // Command preview (monospace, scrollable if long)
                div {
                    style: "
                        background: #1a1b26;
                        border: 1px solid #2a2b3d;
                        border-radius: 4px;
                        padding: 12px;
                        margin-bottom: 16px;
                        font-family: 'SF Mono', 'Menlo', 'Consolas', monospace;
                        font-size: 13px;
                        color: #e0af68;
                        word-break: break-all;
                        max-height: 120px;
                        overflow-y: auto;
                    ",
                    "{command}"
                }

                // Reason
                div {
                    style: "
                        background: rgba(247, 118, 142, 0.1);
                        border-left: 3px solid #f7768e;
                        padding: 10px 14px;
                        margin-bottom: 24px;
                        font-size: 13px;
                        color: #c0caf5;
                        line-height: 1.5;
                    ",
                    "{reason}"
                }

                // Buttons
                div {
                    style: "display: flex; gap: 12px;",

                    // Cancel (primary, safer default)
                    button {
                        style: "
                            flex: 1;
                            background: #1a1b26;
                            color: #c0caf5;
                            border: 1px solid #2a2b3d;
                            border-radius: 4px;
                            padding: 12px;
                            font-size: 14px;
                            font-weight: 500;
                            cursor: pointer;
                            transition: background 0.15s;
                        ",
                        onclick: move |_| on_cancel.call(()),
                        "取消"
                    }

                    // Proceed (danger, red)
                    button {
                        style: "
                            flex: 1;
                            background: #f7768e;
                            color: #1a1b26;
                            border: none;
                            border-radius: 4px;
                            padding: 12px;
                            font-size: 14px;
                            font-weight: 600;
                            cursor: pointer;
                            transition: background 0.15s;
                        ",
                        onclick: move |_| on_proceed.call(()),
                        "仍然继续"
                    }
                }
            }
        }
    }
}
