use dioxus::html::input_data::MouseButton;
use dioxus::prelude::*;

/// Atuin-style suggestion panel rendered BELOW the current cursor line.
/// Shows matching history commands sorted by frequency, with the selected
/// item highlighted. Appears automatically as the user types.
///
/// The vertical position is set via a CSS variable `--suggestion-top`
/// on the parent terminal container, measured by JavaScript to sit exactly
/// below the cursor row. Falls back to `2em` if unset.
///
/// At most [`MAX_VISIBLE_ROWS`] items are shown (the most relevant matches)
/// so the panel stays compact and never blocks the terminal view.
///
/// Interactions:
///   - Click on an item        : accept it (same as Tab)
///   - Click on the × button   : delete that item from history (dirty-data
///                                cleanup — typos / broken commands)
///   - Tab                     : accept selected — completes the matching
///                                suffix only (parent)
///   - ArrowRight (→)          : fill selected — replaces the whole line
///                                with the selected command (parent, no run)
///   - Escape                  : dismiss (parent)
///   - Shift+Delete            : delete selected from history (parent)
///   - ArrowUp / ArrowDown     : move the selection within the list
///                                (consumed by the parent — never reach the
///                                PTY, so the shell cannot swap away the
///                                line being completed)
///   - ArrowLeft               : dismiss panel + forward to PTY (parent) —
///                                always moves the cursor left within the line.
///   - ArrowRight              : fill the selected suggestion directly into
///                                the line (whole-command replace, no run) and
///                                close the panel (parent). Distinct from Tab,
///                                which only completes the matching suffix —
///                                Right fills the entire selected command even
///                                when the typed prefix differs (contains
///                                matches from Alt+R history mode, etc.).
///
/// The × button is the discoverable affordance for deletion — it's always
/// visible on the selected item and on hover for the others. The
/// `Shift+Delete` shortcut is kept for power users but is awkward on macOS
/// MacBook keyboards (which have no dedicated forward-delete key, so it
/// requires Shift+Fn+Backspace), so the × button is the primary path.

/// Maximum number of suggestion rows the popup ever renders. Keeping this
/// small (3) prevents the panel from covering terminal output — the user
/// sees their top matches without losing sight of the session.
pub const MAX_VISIBLE_ROWS: usize = 3;
#[component]
pub fn SuggestionPopup(
    suggestions: Vec<String>,
    selected_index: usize,
    on_select: EventHandler<String>,
    on_dismiss: EventHandler<()>,
    on_delete: EventHandler<String>,
    /// Suggestions that correct a typo rather than complete command history.
    /// Correction rows are labelled and cannot be removed from history.
    #[props(default)]
    correction_suggestions: Vec<String>,
    /// Explicit Alt+R history mode. Enter inserts the selected command without
    /// sending a carriage return.
    #[props(default)]
    history_completion: bool,
    /// Maximum number of rows to display. Defaults to [`MAX_VISIBLE_ROWS`] (3)
    /// when not specified. The caller passes the user's configured value
    /// (3, 5, or 10) from `AppState::suggestion_count`.
    #[props(default)]
    max_rows: usize,
    /// CSS length used for `top` when the measured `--suggestion-top` /
    /// `--suggestion-popup-top` variables are not (yet) set. TerminalView
    /// passes a pixel offset computed from the cursor row so the popup opens
    /// below the prompt from its very first frame; defaults to `2em`.
    #[props(default = "2em".to_string())]
    fallback_top: String,
    /// CSS length used for `left` when the measured `--suggestion-left`
    /// variable is not (yet) set. TerminalView passes a pixel offset
    /// computed from the typed command's start column so the popup opens
    /// aligned under the text being typed; defaults to `0`. The measurement
    /// loop refreshes `--suggestion-left` every tick, so the popup follows
    /// the command dynamically instead of sticking to a fixed edge.
    #[props(default = "0".to_string())]
    fallback_left: String,
    /// Fired when the user presses the drag grip with the primary button.
    /// Payload: the viewport `clientY` of the press. TerminalView installs
    /// the document-level drag listeners that actually move the popup.
    #[props(default)]
    on_drag_start: EventHandler<f64>,
    /// Fired when the user double-clicks the grip: forget the remembered
    /// position and return to automatic placement.
    #[props(default)]
    on_position_reset: EventHandler<()>,
    /// Fired when the user picks "mute this session" in the hint row —
    /// hides the popup for the rest of this session only. New sessions see
    /// suggestions again (the mute is not persisted).
    #[props(default)]
    on_snooze: EventHandler<()>,
    /// Fired when the user picks "disable entirely" — turns the whole
    /// suggestion feature off and persists the choice to settings.json.
    /// Re-enable from the Settings dialog.
    #[props(default)]
    on_disable: EventHandler<()>,
) -> Element {
    let _lang = crate::i18n::LANGUAGE();
    if suggestions.is_empty() {
        return rsx! {};
    }

    // Cap to the most relevant `max_rows` matches so the popup stays
    // compact and never covers a large swath of terminal output.
    let limit = if max_rows > 0 {
        max_rows
    } else {
        MAX_VISIBLE_ROWS
    };
    let visible: Vec<&String> = suggestions.iter().take(limit).collect();
    let current_selected = selected_index.min(visible.len().saturating_sub(1));
    let has_corrections = visible
        .iter()
        .any(|command| correction_suggestions.contains(command));

    rsx! {
        div {
            "data-rusterm-terminal-popup": "true",
            style: "
                position: absolute;
                left: var(--suggestion-left, {fallback_left});
                min-width: 320px;
                max-width: calc(100% - var(--suggestion-left, {fallback_left}));
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
                scrollbar-width: thin;
                scrollbar-color: #2a2b3d transparent;
            ",
            // TerminalView captures a pointer on its root to make drag-selection
            // reliable. Stop the pointer before it reaches that root; otherwise
            // WebKit retargets the ensuing click to the terminal and the × never
            // receives its delete action. Preventing the default press also
            // keeps focus on the terminal, so blur-driven popup cleanup cannot
            // unmount the button before its click callback runs.
            onpointerdown: move |e| {
                e.prevent_default();
                e.stop_propagation();
            },
            onmousedown: move |e| {
                e.prevent_default();
                e.stop_propagation();
            },
            onmouseup: move |e| e.stop_propagation(),
            onclick: move |e| e.stop_propagation(),
            // Drag grip — lets the user move the popup out of the way. The
            // final position is remembered (persisted) as a habit and reapplied
            // whenever the popup opens; double-click restores automatic
            // placement. The actual drag uses document-level listeners
            // installed by TerminalView (element-level mousemove is unreliable
            // in this webview).
            div {
                style: "
                    display:flex;
                    align-items:center;
                    justify-content:center;
                    height:12px;
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
                onmouseup: move |e| e.stop_propagation(),
                onclick: move |e| e.stop_propagation(),
                ondoubleclick: move |e| {
                    e.stop_propagation();
                    on_position_reset.call(());
                },
                "•••"
            }
            if history_completion {
                div {
                    style: "display:flex;align-items:center;justify-content:space-between;padding:4px 12px;border-bottom:1px solid #2a2b3d;background:#24283b;color:#c0caf5;font-size:11px;",
                    span { { crate::i18n::t("suggestion.history_completion_title") } }
                    span { style: "color:#9aa5ce;", "Alt+R" }
                }
            }
            for (i, cmd) in visible.iter().enumerate() {
                {
                    let is_selected = i == current_selected;
                    let bg = if is_selected { "#283457" } else { "transparent" };
                    let fg = if is_selected { "#c0caf5" } else { "#a9b1d6" };
                    let left_border = if is_selected {
                        "border-left:2px solid #7aa2f7;"
                    } else {
                        "border-left:2px solid transparent;"
                    };
                    let is_correction = correction_suggestions.contains(*cmd);
                    let cmd_for_select = (*cmd).clone();
                    let cmd_for_delete = (*cmd).clone();
                    // × button color:
                    //   - selected item : bright green (always visible)
                    //   - non-selected   : muted gray, brightens on row hover
                    //     via the `.sug-row:hover .sug-del` CSS rule emitted below.
                    let del_color = if is_selected { "#9ece6a" } else { "#9aa5ce" };
                    rsx! {
                        div {
                            key: "{cmd}",
                            class: "sug-row",
                            style: "display:flex;align-items:center;padding:3px 12px;{left_border}background:{bg};color:{fg};cursor:pointer;white-space:pre;overflow:hidden;",
                            onclick: move |_| on_select.call(cmd_for_select.clone()),
                            span {
                                style: "flex:1;display:flex;gap:8px;min-width:0;overflow:hidden;",
                                if is_correction {
                                    span {
                                        style: "flex-shrink:0;color:#9ece6a;font-size:11px;",
                                        { crate::i18n::t("suggestion.correction_prefix") }
                                    }
                                }
                                span {
                                    style: "min-width:0;overflow:hidden;text-overflow:ellipsis;",
                                    "{cmd}"
                                }
                            }
                            if !is_correction {
                            button {
                                class: "sug-del",
                                r#type: "button",
                                style: "
                                    margin-left:8px;
                                    padding:0 6px;
                                    color:{del_color};
                                    font:inherit;
                                    font-size:14px;
                                    font-weight:700;
                                    line-height:1;
                                    cursor:pointer;
                                    user-select:none;
                                    -webkit-user-select:none;
                                    border:0;
                                    border-radius:3px;
                                    background:transparent;
                                    flex-shrink:0;
                                ",
                                title: crate::i18n::t("suggestion.remove_history_tooltip"),
                                aria_label: crate::i18n::t("suggestion.remove_history_aria"),
                                // Keep the event local even if this component is
                                // later rendered outside its current popup root.
                                onpointerdown: move |e| {
                                    e.prevent_default();
                                    e.stop_propagation();
                                },
                                onmousedown: move |e| {
                                    e.prevent_default();
                                    e.stop_propagation();
                                },
                                onmouseup: move |e| e.stop_propagation(),
                                onclick: move |e| {
                                    e.stop_propagation();
                                    on_delete.call(cmd_for_delete.clone());
                                },
                                "×"
                            }
                            }
                        }
                    }
                }
            }
            // Hint row — tells the user both affordances exist, like the
            // oh-my-zsh update prompt ("disable with DISABLE_AUTO_UPDATE").
            // Muted color so it doesn't compete with the suggestions.
            div {
                style: "
                    display:flex;
                    align-items:center;
                    justify-content:space-between;
                    gap:12px;
                    padding:2px 12px;
                    border-top:1px solid #2a2b3d;
                    color:#9aa5ce;
                    font-size:11px;
                    background:#1a1b26;
                ",
                span {
                    style: "min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;",
                    if history_completion {
                        { crate::i18n::t("suggestion.history_completion_hint") }
                    } else if has_corrections {
                        { crate::i18n::t("suggestion.correction_hint") }
                    } else {
                        { crate::i18n::t("suggestion.history_hint") }
                    }
                }
                // Temp/permanent dismissal only applies to the automatic
                // popup — the explicit Alt+R picker is user-invoked, so
                // offering to mute it would be misleading.
                if !history_completion {
                div {
                    style: "display:flex;align-items:center;gap:8px;flex-shrink:0;",
                    span {
                        class: "sug-act",
                        style: "color:#565f89;cursor:pointer;text-decoration:underline dotted;",
                        title: crate::i18n::t("suggestion.snooze_tooltip"),
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
                            on_snooze.call(());
                        },
                        { crate::i18n::t("suggestion.snooze") }
                    }
                    span { style: "color:#2a2b3d;user-select:none;", "·" }
                    span {
                        class: "sug-act",
                        style: "color:#565f89;cursor:pointer;text-decoration:underline dotted;",
                        title: crate::i18n::t("suggestion.disable_tooltip"),
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
                            on_disable.call(());
                        },
                        { crate::i18n::t("suggestion.disable") }
                    }
                }
                }
            }
            // Hover rule for the × button on non-selected rows. Selected rows
            // always show the × (handled inline above). The actual CSS rules
            // live in the global `<style>` block in `main.rs` (`with_custom_head`)
            // alongside the other Tokyo Night hover rules — keeps all theme
            // CSS in one place and avoids `<style>`-inside-`<div>` quirks.
            // Hidden dismiss anchor — kept for symmetry with OneKeyPopup so
            // future callers can wire an explicit dismiss target if needed.
            div { style: "display:none;", onclick: move |_| on_dismiss.call(()), "" }
        }
    }
}
