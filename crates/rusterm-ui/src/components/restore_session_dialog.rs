use dioxus::prelude::*;

/// One row in the restore dialog's session list. Built by
/// `app::restore_prompt_items` from the loaded `SessionState` snapshot so the
/// user can see exactly what will come back before confirming.
#[derive(Clone, Debug, PartialEq)]
pub struct RestoreSessionSummary {
    /// Tab title, e.g. "user@host" or "Local".
    pub name: String,
    /// Kind + hostname detail, e.g. "SSH · jumpserver.example.com".
    pub detail: String,
    /// Whether recorded interactive establishment ops (jumpserver-style
    /// menu navigation) will be replayed to restore this session's state.
    pub has_replay: bool,
}

/// Modal shown after the app is unlocked if a saved `SessionState` was loaded
/// from disk. Asks the user whether to restore the previous sessions. It is
/// shown regardless of how the previous run ended: a normal exit persists the
/// snapshot on the close path, and a crash / force-kill leaves behind the 30 s
/// periodic save.
///
/// The restore is **non-destructive**: we only reconnect sessions, send a
/// single `cd '<last_cwd>'` per integrated-shell session, and replay the
/// recorded *establishment* ops for interactive (jumpserver-style) sessions.
/// We **never** re-execute past shell commands — the establishment replay is
/// capped, safety-filtered, and mutually exclusive with shell integration.
///
/// Two actions:
/// - 恢复 (Restore): reconnect each session + `cd <cwd>` / replay recorded ops
/// - 跳过 (Skip):    clear `restore_pending`, start with blank sessions
#[component]
pub fn RestoreSessionDialog(
    session_count: usize,
    saved_at: String,
    /// Per-session summary rows shown so the user knows what "恢复" brings
    /// back (name, kind/host, and whether interactive ops will be replayed).
    sessions: Vec<RestoreSessionSummary>,
    on_restore: EventHandler<()>,
    on_skip: EventHandler<()>,
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
                    width: 460px;
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

                // Session list — what exactly will be restored.
                div {
                    style: "
                        background: #1a1b26;
                        border-radius: 6px;
                        padding: 10px 12px;
                        margin-bottom: 14px;
                        max-height: 180px;
                        overflow-y: auto;
                        font-size: 13px;
                        line-height: 1.6;
                    ",
                    for (i, item) in sessions.iter().enumerate() {
                        div {
                            key: "{i}",
                            style: "
                                display: flex;
                                align-items: center;
                                gap: 8px;
                                padding: 4px 2px;
                            ",
                            span {
                                style: "color: #c0caf5; font-weight: 500; white-space: nowrap; overflow: hidden; text-overflow: ellipsis;",
                                { item.name.clone() }
                            }
                            span {
                                style: "color: #565f89; font-size: 12px; white-space: nowrap;",
                                { item.detail.clone() }
                            }
                            if item.has_replay {
                                span {
                                    style: "
                                        margin-left: auto;
                                        background: #2a2b3d;
                                        color: #e0af68;
                                        border-radius: 3px;
                                        padding: 1px 6px;
                                        font-size: 11px;
                                        white-space: nowrap;
                                    ",
                                    { crate::i18n::t("restore.replay_badge") }
                                }
                            }
                        }
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
                        style: "margin: 0 0 8px; color: #9ece6a;",
                        { crate::i18n::t("restore.will_replay") }
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
                        { crate::i18n::t("restore.restore") }
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
                }
            }
        }
    }
}
