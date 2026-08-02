use dioxus::prelude::*;

/// Modal shown after the app is unlocked if a saved `SessionState` was loaded
/// from disk. Asks the user whether to restore the previous sessions.
///
/// The restore is **non-destructive**: we only reconnect sessions and send a
/// single `cd '<last_cwd>'` per session. We **never** re-execute any past
/// command or script — the user explicitly asked us not to, because doing so
/// could cause destructive side effects on next launch.
///
/// Three actions:
/// - 恢复 (Restore):    reconnect each session + `cd <cwd>`
/// - 跳过 (Skip):       clear `restore_pending`, do nothing
/// - 不再询问 (Never):  clear `restore_pending` + set `restore_disabled = true`
///                       in settings.json so we never re-prompt (and stop
///                       saving session state entirely)
#[component]
pub fn RestoreSessionDialog(
    session_count: usize,
    saved_at: String,
    on_restore: EventHandler<()>,
    on_skip: EventHandler<()>,
    on_never_ask: EventHandler<()>,
) -> Element {
    let _lang = crate::i18n::LANGUAGE();
    rsx! {
        div {
            style: "
                position: fixed;
                top: 0; left: 0; right: 0; bottom: 0;
                background: rgba(0, 0, 0, 0.6);
                display: flex;
                justify-content: center;
                align-items: center;
                z-index: 1900;
            ",

            div {
                style: "
                    background: #24283b;
                    border-radius: 8px;
                    padding: 32px;
                    width: 440px;
                    color: #c0caf5;
                    box-shadow: 0 8px 32px rgba(0, 0, 0, 0.5);
                ",

                // Title + icon
                div {
                    style: "text-align: center; margin-bottom: 20px;",
                    h2 {
                        style: "margin: 0 0 8px; font-size: 20px; font-weight: 600; color: #7aa2f7;",
                        { crate::i18n::t("restore.title") }
                    }
                    p {
                        style: "margin: 0; font-size: 13px; color: #9aa5ce; line-height: 1.5;",
                        { crate::i18n::tf("restore.detected", &[("session_count", &session_count), ("saved_at", &saved_at)]) }
                    }
                }

                // Description of what restore will do
                div {
                    style: "
                        background: #1a1b26;
                        border-radius: 6px;
                        padding: 14px 16px;
                        margin-bottom: 24px;
                        font-size: 13px;
                        color: #a9b1d6;
                        line-height: 1.6;
                    ",
                    p {
                        style: "margin: 0 0 8px; color: #9ece6a; font-weight: 500;",
                        { crate::i18n::t("restore.will_cd") }
                    }
                    p {
                        style: "margin: 0 0 8px; color: #e0af68;",
                        { crate::i18n::t("restore.no_history_run") }
                    }
                    p {
                        style: "margin: 0; color: #9aa5ce; font-size: 12px;",
                        { crate::i18n::t("restore.skip_hint") }
                    }
                }

                // Buttons
                div {
                    style: "display: flex; flex-direction: column; gap: 10px;",

                    // Restore (primary, green)
                    button {
                        style: "
                            width: 100%;
                            background: #9ece6a;
                            color: #1a1b26;
                            border: none;
                            border-radius: 4px;
                            padding: 12px;
                            font-size: 14px;
                            font-weight: 600;
                            cursor: pointer;
                            transition: background 0.15s;
                        ",
                        onclick: move |_| on_restore.call(()),
                        { crate::i18n::t("restore.title") }
                    }

                    // Skip (secondary, neutral)
                    button {
                        style: "
                            width: 100%;
                            background: #1a1b26;
                            color: #c0caf5;
                            border: 1px solid #2a2b3d;
                            border-radius: 4px;
                            padding: 12px;
                            font-size: 14px;
                            cursor: pointer;
                            transition: background 0.15s;
                        ",
                        onclick: move |_| on_skip.call(()),
                        { crate::i18n::t("restore.skip_blank") }
                    }

                    // Never ask (tertiary, muted)
                    button {
                        style: "
                            width: 100%;
                            background: transparent;
                            color: #9aa5ce;
                            border: none;
                            border-radius: 4px;
                            padding: 8px;
                            font-size: 12px;
                            cursor: pointer;
                            transition: color 0.15s;
                        ",
                        onclick: move |_| on_never_ask.call(()),
                        { crate::i18n::t("common.dont_ask_again") }
                    }
                }
            }
        }
    }
}
