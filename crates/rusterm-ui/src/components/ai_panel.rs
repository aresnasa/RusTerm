use dioxus::prelude::*;

use rusterm_ai::suggestion::AiSuggestion;

#[component]
pub fn AiPanel(
    visible: bool,
    suggestions: Vec<AiSuggestion>,
    on_close: EventHandler<()>,
    on_apply: EventHandler<String>,
) -> Element {
    if !visible {
        return rsx! {};
    }

    let is_empty = suggestions.is_empty();

    rsx! {
        div {
            style: "
                position: fixed;
                right: 0; top: 36px; bottom: 0;
                width: 300px;
                background: #1a1b26;
                border-left: 1px solid #2a2b3d;
                display: flex;
                flex-direction: column;
                z-index: 100;
                color: #c0caf5;
            ",

            div {
                style: "
                    padding: 12px 16px;
                    display: flex;
                    justify-content: space-between;
                    align-items: center;
                    border-bottom: 1px solid #2a2b3d;
                ",
                span { style: "font-weight: 600; font-size: 13px;", "AI Suggestions" }
                button {
                    style: "background: none; border: none; color: #565f89; cursor: pointer; font-size: 14px;",
                    onclick: move |_| on_close.call(()),
                    "x"
                }
            }

            div {
                style: "flex: 1; overflow-y: auto; padding: 8px;",

                for suggestion in suggestions {
                    div {
                        key: "{suggestion.command}",
                        style: "
                            padding: 10px 12px;
                            margin: 4px 0;
                            background: #24283b;
                            border-radius: 4px;
                            cursor: pointer;
                            font-family: 'JetBrains Mono', monospace;
                            font-size: 12px;
                        ",
                        onclick: move |_| on_apply.call(suggestion.command.clone()),

                        div { "{suggestion.command}" }
                        div {
                            style: "font-size: 10px; color: #565f89; margin-top: 4px;",
                            "{suggestion.source:?} - {suggestion.confidence:.0}%"
                        }
                    }
                }

                if is_empty {
                    div {
                        style: "text-align: center; color: #565f89; padding: 40px 16px; font-size: 13px;",
                        "No suggestions yet.\nStart typing a command to get AI-powered completions."
                    }
                }
            }
        }
    }
}
