use std::collections::HashSet;

use dioxus::prelude::*;

use crate::state::AppState;

/// Debounce window for the Send-panel completion query. Matches the terminal
/// suggestion pipeline (200ms) so typing feels consistent across the two
/// input surfaces.
const SEND_COMPLETION_DEBOUNCE_MS: u64 = 200;

/// Send panel: a multi-line textarea for typing a command to broadcast to one
/// or more connected sessions, plus an inline history-based completion popup.
///
/// Completion sources (same blend as the terminal suggestion pipeline):
///   1. SQLite `search_history` — frecency-ranked (frequency + recency +
///      success rate), the unified history DB.
///   2. DuckDB `suggest_by_prefix` — pure total-usage frequency from the
///      analytics mirror. Surfaces commands the user runs often but maybe
///      not recently, complementing SQLite's recency bias.
///
/// Both sources are prefix-filtered, deduped, and stripped of commands in the
/// recent-failed set. The popup is positioned just below the textarea and
/// accepts on Tab / ↓, navigates with ↑/↓, dismisses on Escape.
///
/// Why a dedicated component (rather than wiring completion into
/// `BottomToolPanel` directly): the panel already owns target selection,
/// resizing and tab state. Keeping the textarea + completion in its own
/// component lets the debounce/epoch state live close to the input and keeps
/// `BottomToolPanel`'s prop surface unchanged.
#[component]
pub fn SendPanel(
    state: Signal<AppState>,
    has_targets: bool,
    on_send: EventHandler<String>,
) -> Element {
    let mut command = use_signal(String::new);
    let mut suggestions = use_signal(Vec::<String>::new);
    let mut selected = use_signal(|| 0usize);
    let mut visible = use_signal(|| false);
    // Monotonic counter bumped on every keystroke; the spawned query captures
    // the value at spawn time and aborts if it changed by the time the debounce
    // elapses. This cancels stale queries the same way the terminal pipeline
    // uses `suggestion_epoch`.
    let mut epoch = use_signal(|| 0u64);

    let popup_open = visible() && !suggestions().is_empty();
    let count = popup_open as usize;

    rsx! {
        div {
            style: "flex:1;display:flex;gap:8px;padding:9px;min-height:0;position:relative;",
            div {
                style: "position:relative;min-width:0;flex:1;display:flex;",
                textarea {
                    style: "min-width:0;flex:1;resize:none;background:var(--skin-surface);border:1px solid var(--skin-border);border-radius:4px;padding:8px 9px;color:var(--skin-text);font:12px ui-monospace,SFMono-Regular,Menlo,monospace;outline:none;",
                    placeholder: "Command to send (Ctrl/Cmd+Enter to run, Tab to complete)...",
                    value: "{command}",
                    oninput: move |event| {
                        let value = event.value();
                        command.set(value.clone());
                        let trimmed = value.trim();
                        // No prefix → hide popup and skip the query entirely.
                        if trimmed.is_empty() {
                            suggestions.set(Vec::new());
                            visible.set(false);
                            selected.set(0);
                            return;
                        }
                        epoch += 1;
                        let my_epoch = epoch();
                        visible.set(true);
                        let state_for_query = state;
                        let prefix = trimmed.to_string();
                        spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_millis(
                                SEND_COMPLETION_DEBOUNCE_MS,
                            )).await;
                            // Stale guard: a newer keystroke superseded this one.
                            if epoch() != my_epoch {
                                return;
                            }
                            let fetched = fetch_send_completions(&state_for_query, &prefix).await;
                            // Re-check epoch after the await in case typing
                            // continued during the DB query.
                            if epoch() != my_epoch {
                                return;
                            }
                            let was_visible = !fetched.is_empty();
                            suggestions.set(fetched);
                            visible.set(was_visible);
                            selected.set(0);
                        });
                    },
                    onkeydown: move |event: KeyboardEvent| {
                        let key = event.key();
                        // Navigate / accept completion first so the popup can
                        // consume arrow keys and Tab before the textarea does.
                        if popup_open {
                            match key {
                                Key::Tab => {
                                    event.prevent_default();
                                    let idx = selected().min(suggestions().len().saturating_sub(1));
                                    if let Some(cmd) = suggestions().get(idx).cloned() {
                                        accept_completion(&mut command, &cmd);
                                        suggestions.set(Vec::new());
                                        visible.set(false);
                                        selected.set(0);
                                        epoch += 1;
                                    }
                                    return;
                                }
                                Key::ArrowDown => {
                                    event.prevent_default();
                                    if count > 0 {
                                        selected.set((selected() + 1) % count);
                                    }
                                    return;
                                }
                                Key::ArrowUp => {
                                    event.prevent_default();
                                    if count > 0 {
                                        selected.set(selected().wrapping_sub(1) % count);
                                    }
                                    return;
                                }
                                Key::Escape => {
                                    event.prevent_default();
                                    suggestions.set(Vec::new());
                                    visible.set(false);
                                    selected.set(0);
                                    epoch += 1;
                                    return;
                                }
                                _ => {}
                            }
                        }
                        if matches!(key, Key::Enter)
                            && (event.modifiers().ctrl() || event.modifiers().meta())
                        {
                            event.prevent_default();
                            let value = command().trim().to_string();
                            if !value.is_empty() && has_targets {
                                on_send.call(value);
                                command.set(String::new());
                                suggestions.set(Vec::new());
                                visible.set(false);
                                selected.set(0);
                                epoch += 1;
                            }
                        }
                    },
                }
                if popup_open {
                    SendCompletionPopup {
                        suggestions: suggestions(),
                        selected: selected(),
                        on_select: move |cmd: String| {
                            accept_completion(&mut command, &cmd);
                            suggestions.set(Vec::new());
                            visible.set(false);
                            selected.set(0);
                            epoch += 1;
                        },
                    }
                }
            }
            div {
                style: "display:flex;flex-direction:column;justify-content:flex-end;gap:6px;",
                button {
                    class: "workspace-primary-button",
                    disabled: command().trim().is_empty() || !has_targets,
                    onclick: move |_| {
                        let value = command().trim().to_string();
                        if !value.is_empty() && has_targets {
                            on_send.call(value);
                            command.set(String::new());
                            suggestions.set(Vec::new());
                            visible.set(false);
                            selected.set(0);
                            epoch += 1;
                        }
                    },
                    "Send ↵"
                }
            }
        }
    }
}

/// Inline completion popup for the Send panel. Rendered absolutely below the
/// textarea. Styled to match `SuggestionPopup` (Tokyo Night palette) so the
/// two completion surfaces feel like one feature.
#[component]
fn SendCompletionPopup(
    suggestions: Vec<String>,
    selected: usize,
    on_select: EventHandler<String>,
) -> Element {
    let current = selected.min(suggestions.len().saturating_sub(1));
    rsx! {
        div {
            style: "
                position:absolute;
                left:0;right:0;
                top:100%;
                z-index:30;
                background:#16161e;
                border:1px solid #2a2b3d;
                border-top:none;
                box-shadow:0 4px 16px rgba(0,0,0,0.4);
                font-family:'JetBrains Mono','Fira Code','Cascadia Code',monospace;
                font-size:12px;
                line-height:1.5;
                max-height:240px;
                overflow-y:auto;
            ",
            // Prevent default on pointer/mouse down so the textarea keeps
            // focus while the user clicks a suggestion — mirrors the
            // SuggestionPopup / OneKeyPopup pattern.
            onpointerdown: move |e: Event<PointerData>| {
                e.prevent_default();
                e.stop_propagation();
            },
            onmousedown: move |e: Event<MouseData>| {
                e.prevent_default();
                e.stop_propagation();
            },
            for (i, cmd) in suggestions.iter().enumerate() {
                {
                    let is_sel = i == current;
                    let bg = if is_sel { "#283457" } else { "transparent" };
                    let fg = if is_sel { "#c0caf5" } else { "#a9b1d6" };
                    let left_border = if is_sel {
                        "border-left:2px solid #7aa2f7;"
                    } else {
                        "border-left:2px solid transparent;"
                    };
                    let cmd_for_select = cmd.clone();
                    rsx! {
                        div {
                            key: "{cmd}",
                            style: "display:flex;align-items:center;padding:3px 12px;{left_border}background:{bg};color:{fg};cursor:pointer;white-space:pre;overflow:hidden;",
                            onclick: move |_| on_select.call(cmd_for_select.clone()),
                            span {
                                style: "min-width:0;overflow:hidden;text-overflow:ellipsis;",
                                "{cmd}"
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Replace the textarea contents with the accepted completion. We set the
/// full command (not just append) because completions are whole command
/// lines from history — partial-token completion would mislead users into
/// thinking they're completing a single argument.
fn accept_completion(command: &mut Signal<String>, cmd: &str) {
    command.set(cmd.to_string());
}

/// Fetch completion candidates for the Send panel by blending the two
/// history sources. Order: SQLite frecency first (recency-weighted, the
/// commands the user is most likely to want right now), then DuckDB pure
/// frequency (commands used often overall). Dedup is case-insensitive and
/// the current input + recent-failed commands are excluded.
///
/// `state` is taken by value (`Copy` — `Signal` is cheap to copy) so the
/// spawned future can capture it without lifetime issues.
async fn fetch_send_completions(state: &Signal<AppState>, prefix: &str) -> Vec<String> {
    let prefix_lower = prefix.to_lowercase();
    let sug_count = state.read().suggestion_count.max(1) as usize;
    let recent_failed: HashSet<String> = state.read().recent_failed_commands.clone();
    let analytics = state.read().analytics.clone();

    let mut all: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    // 1. SQLite frecency (unified history DB).
    let db_path = dirs::data_dir()
        .unwrap_or_default()
        .join("rusterm")
        .join("rusterm.db");
    if let Ok(db) = rusterm_db::Database::open(Some(db_path)).await {
        if let Ok(results) = db.search_history(prefix, sug_count * 2).await {
            for entry in results {
                if entry.command.to_lowercase().starts_with(&prefix_lower)
                    && entry.command != prefix
                    && !seen.contains(&entry.command.to_lowercase())
                    && !recent_failed.contains(&entry.command)
                {
                    seen.insert(entry.command.to_lowercase());
                    all.push(entry.command);
                }
            }
        }
    }

    // 2. DuckDB pure-frequency (analytics mirror). No-op when the analytics
    //    feature is off (returns an empty vec).
    if all.len() < sug_count {
        if let Ok(rankings) = analytics.suggest_by_prefix(prefix, (sug_count * 2) as u32) {
            for cmd in rankings {
                if cmd.to_lowercase().starts_with(&prefix_lower)
                    && cmd != prefix
                    && !seen.contains(&cmd.to_lowercase())
                    && !recent_failed.contains(&cmd)
                {
                    seen.insert(cmd.to_lowercase());
                    all.push(cmd);
                    if all.len() >= sug_count {
                        break;
                    }
                }
            }
        }
    }

    all.truncate(sug_count);
    all
}
