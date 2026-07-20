use dioxus::prelude::*;

/// Modal shown when the user closes the last window (the OS close button,
/// Cmd+Q on macOS with a single window, Alt+F4 on Linux/Windows).
///
/// Asks "是否确实要关闭本软件？" with:
///   - a checkbox "下次关闭时不再询问" (default CHECKED — see below)
///   - a [取消] button (the DEFAULT/primary action — pressing it keeps the
///     app running)
///   - a [确认关闭] button (actually exits the app)
///
/// Both buttons are always visible ("都要显示给用户选择").
///
/// ## Checkbox semantics
///
/// The checkbox is checked by default ("默认勾选"). When checked, the user's
/// button choice is remembered via `confirm_close_on_exit`:
///   - checked + 取消 → `confirm_close_on_exit = false` (next close is
///     auto-cancelled — the close button does nothing visible)
///   - checked + 确认 → `confirm_close_on_exit = false` (next close exits
///     immediately without asking)
///
/// When unchecked, the user is asked again next time regardless of which
/// button they pick. This matches the user's request: "增加一个默认勾选按钮
/// 和取消默认关闭" — a default-checked checkbox, and Cancel as the default
/// close behavior.
///
/// ## Why Cancel is the default (primary) button
///
/// "取消默认关闭" = the close is cancelled by default. The user must
/// explicitly click 确认关闭 to actually exit. This prevents accidental
/// exits when the user mis-clicks the close button.
#[component]
pub fn CloseConfirmationDialog(
    /// Current state of the "下次关闭时不再询问" checkbox. The parent owns
    /// this so it can persist the choice when the dialog closes.
    dont_ask_again: bool,
    /// Fired when the user toggles the checkbox. The parent updates its
    /// state + persists.
    on_toggle_dont_ask: EventHandler<bool>,
    /// Fired when the user clicks 确认关闭. The parent flips the close
    /// behaviour to WindowCloses and calls `window.close()` to actually exit.
    on_confirm: EventHandler<()>,
    /// Fired when the user clicks 取消. The parent just hides the dialog
    /// (the window has already been re-shown by the reshow future).
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
                z-index: 2200;
            ",

            div {
                style: "
                    background: #24283b;
                    border: 1px solid #7aa2f7;
                    border-radius: 8px;
                    padding: 28px;
                    width: 440px;
                    color: #c0caf5;
                    box-shadow: 0 8px 32px rgba(0, 0, 0, 0.5);
                ",

                // Header
                div {
                    style: "text-align: center; margin-bottom: 20px;",
                    h2 {
                        style: "margin: 0 0 8px; font-size: 18px; font-weight: 600; color: #7aa2f7;",
                        "关闭确认"
                    }
                    p {
                        style: "margin: 0; font-size: 13px; color: #565f89;",
                        "即将关闭最后一个窗口"
                    }
                }

                // Message
                div {
                    style: "
                        text-align: center;
                        font-size: 15px;
                        color: #c0caf5;
                        line-height: 1.6;
                        margin-bottom: 20px;
                    ",
                    "是否确实要关闭本软件？"
                }

                // Checkbox (default checked — "默认勾选")
                div {
                    style: "
                        display: flex;
                        align-items: center;
                        justify-content: center;
                        gap: 8px;
                        margin-bottom: 24px;
                        font-size: 13px;
                        color: #9aa5ce;
                        cursor: pointer;
                        user-select: none;
                    ",
                    onclick: move |_| on_toggle_dont_ask.call(!dont_ask_again),
                    input {
                        r#type: "checkbox",
                        checked: dont_ask_again,
                        style: "
                            width: 16px;
                            height: 16px;
                            cursor: pointer;
                            accent-color: #7aa2f7;
                        ",
                        // This is a controlled checkbox — the parent div's
                        // onclick handles the toggle by calling
                        // on_toggle_dont_ask. We don't need onclick/onchange
                        // here; letting the click bubble up lets the user
                        // click either the checkbox or the label text to
                        // toggle.
                    }
                    span { "下次关闭时不再询问" }
                }

                // Buttons — both visible ("都要显示给用户选择").
                // 取消 is the DEFAULT/primary (leftmost, neutral background)
                // — "取消默认关闭" = cancel is the default close behavior.
                // 确认关闭 is the destructive action (rightmost, red).
                div {
                    style: "display: flex; gap: 12px;",

                    // 取消 (Cancel) — primary, default. Keeps the app running.
                    button {
                        style: "
                            flex: 1;
                            background: #7aa2f7;
                            color: #1a1b26;
                            border: none;
                            border-radius: 4px;
                            padding: 12px;
                            font-size: 14px;
                            font-weight: 600;
                            cursor: pointer;
                            transition: background 0.15s;
                        ",
                        onclick: move |_| on_cancel.call(()),
                        "取消"
                    }

                    // 确认关闭 (Confirm Close) — destructive, exits the app.
                    button {
                        style: "
                            flex: 1;
                            background: #1a1b26;
                            color: #f7768e;
                            border: 1px solid #f7768e;
                            border-radius: 4px;
                            padding: 12px;
                            font-size: 14px;
                            font-weight: 500;
                            cursor: pointer;
                            transition: background 0.15s;
                        ",
                        onclick: move |_| on_confirm.call(()),
                        "确认关闭"
                    }
                }
            }
        }
    }
}
