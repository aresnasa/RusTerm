use dioxus::prelude::*;

/// Atuin-style suggestion panel rendered ABOVE the current cursor line.
/// Shows matching history commands sorted by frequency, with the selected
/// item highlighted. Appears automatically as the user types.
///
/// The vertical position is set via a CSS variable `--suggestion-bottom`
/// on the parent terminal container, measured by JavaScript to sit exactly
/// above the cursor row. Falls back to `2em` if unset.
///
/// Interactions:
///   - Click on an item        : accept it (same as Tab)
///   - Click on the × button   : delete that item from history (dirty-data
///                                cleanup — typos / broken commands)
///   - ArrowUp / ArrowDown     : navigate (handled by parent `TerminalView`)
///   - Tab                     : accept selected (parent)
///   - Escape                  : dismiss (parent)
///   - Shift+Delete            : delete selected from history (parent)
///
/// The × button is the discoverable affordance for deletion — it's always
/// visible on the selected item and on hover for the others. The
/// `Shift+Delete` shortcut is kept for power users but is awkward on macOS
/// MacBook keyboards (which have no dedicated forward-delete key, so it
/// requires Shift+Fn+Backspace), so the × button is the primary path.
#[component]
pub fn SuggestionPopup(
    suggestions: Vec<String>,
    selected_index: usize,
    on_select: EventHandler<String>,
    on_dismiss: EventHandler<()>,
    on_delete: EventHandler<String>,
) -> Element {
    if suggestions.is_empty() {
        return rsx! {};
    }

    let current_selected = selected_index;

    rsx! {
        div {
            style: "
                position: absolute;
                left: 0; right: 0;
                bottom: var(--suggestion-bottom, 2em);
                max-height: 50%;
                overflow-y: auto;
                background: #16161e;
                border: 1px solid #2a2b3d;
                border-bottom: none;
                box-shadow: 0 -4px 16px rgba(0,0,0,0.4);
                font-family: 'JetBrains Mono', 'Fira Code', 'Cascadia Code', monospace;
                font-size: 13px;
                line-height: 1.5;
                z-index: 20;
                scrollbar-width: thin;
                scrollbar-color: #2a2b3d transparent;
            ",
            onclick: move |e| e.stop_propagation(),
            for (i, cmd) in suggestions.iter().enumerate() {
                {
                    let is_selected = i == current_selected;
                    let bg = if is_selected { "#283457" } else { "transparent" };
                    let fg = if is_selected { "#c0caf5" } else { "#a9b1d6" };
                    let left_border = if is_selected {
                        "border-left:2px solid #7aa2f7;"
                    } else {
                        "border-left:2px solid transparent;"
                    };
                    let cmd_for_select = cmd.clone();
                    let cmd_for_delete = cmd.clone();
                    // × button color:
                    //   - selected item : bright green (always visible)
                    //   - non-selected   : muted gray, brightens on row hover
                    //     via the `.sug-row:hover .sug-del` CSS rule emitted below.
                    let del_color = if is_selected { "#9ece6a" } else { "#565f89" };
                    rsx! {
                        div {
                            key: "{i}",
                            class: "sug-row",
                            style: "display:flex;align-items:center;padding:3px 12px;{left_border}background:{bg};color:{fg};cursor:pointer;white-space:pre;overflow:hidden;",
                            onclick: move |_| on_select.call(cmd_for_select.clone()),
                            span {
                                style: "flex:1;overflow:hidden;text-overflow:ellipsis;",
                                "{cmd}"
                            }
                            span {
                                class: "sug-del",
                                style: "
                                    margin-left:8px;
                                    padding:0 6px;
                                    color:{del_color};
                                    font-size:14px;
                                    font-weight:700;
                                    cursor:pointer;
                                    user-select:none;
                                    border-radius:3px;
                                    line-height:1;
                                    flex-shrink:0;
                                ",
                                title: "Remove from history (Shift+Del)",
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
            // Hint row — tells the user both affordances exist. Muted color
            // so it doesn't compete with the suggestions themselves.
            div {
                style: "
                    display:flex;
                    align-items:center;
                    justify-content:flex-end;
                    padding:2px 12px;
                    border-top:1px solid #2a2b3d;
                    color:#565f89;
                    font-size:11px;
                    background:#1a1b26;
                ",
                "Shift+Del or click × to remove"
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
