use dioxus::prelude::*;

use rusterm_ai::suggestion::AiSuggestion;

#[component]
pub fn AiPanel(
    visible: bool,
    suggestions: Vec<AiSuggestion>,
    status: String,
    shared_result_count: usize,
    on_close: EventHandler<()>,
    on_review: EventHandler<String>,
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
                width: 340px;
                background: var(--skin-surface);
                border-left: 1px solid var(--skin-border);
                display: flex;
                flex-direction: column;
                z-index: 100;
                color: var(--skin-text);
                box-shadow: -8px 0 24px rgba(0,0,0,0.25);
            ",

            div {
                style: "padding:12px 16px;border-bottom:1px solid var(--skin-border);",
                div { style: "display:flex;justify-content:space-between;align-items:center;",
                    span { style: "font-weight:600;font-size:13px;", "AI 建议 · 影子沙盒" }
                    button {
                        style: "background:none;border:none;color:var(--skin-text-muted);cursor:pointer;font-size:14px;",
                        onclick: move |_| on_close.call(()),
                        "x"
                    }
                }
                p {
                    style: "margin:8px 0 0;color:var(--skin-text-muted);font-size:11px;line-height:1.5;",
                    "模型只能提出建议。选择后仍需在独立弹窗中确认，模型无权直接写入终端。"
                }
            }

            div {
                style: "padding:9px 12px;background:var(--skin-bg);border-bottom:1px solid var(--skin-border);font-size:11px;line-height:1.5;",
                div { style: "color:var(--skin-text-muted);", "{status}" }
                div { style: "margin-top:3px;color:var(--skin-accent);", "已授权给模型的本机结果：{shared_result_count}" }
            }

            div {
                style: "flex:1;overflow-y:auto;padding:8px;",

                for suggestion in suggestions {
                    div {
                        key: "{suggestion.command}",
                        style: "padding:10px 12px;margin:4px 0;background:var(--skin-bg);border:1px solid var(--skin-border);border-radius:5px;font-size:12px;",
                        div {
                            style: "font-family:'JetBrains Mono',monospace;white-space:pre-wrap;word-break:break-all;",
                            "{suggestion.command}"
                        }
                        div { style: "display:flex;align-items:center;justify-content:space-between;gap:8px;margin-top:9px;",
                            span {
                                style: "font-size:10px;color:var(--skin-text-muted);",
                                "{suggestion.source:?} · {suggestion.confidence * 100.0:.0}%"
                            }
                            button {
                                style: "background:var(--skin-accent);color:var(--skin-bg);border:0;border-radius:4px;padding:5px 9px;font-size:11px;font-weight:600;cursor:pointer;",
                                onclick: move |_| on_review.call(suggestion.command.clone()),
                                "审查执行"
                            }
                        }
                    }
                }

                if is_empty {
                    div {
                        style: "text-align:center;color:var(--skin-text-muted);padding:40px 16px;font-size:12px;line-height:1.6;",
                        "暂无模型建议。\n设置 OPENAI_API_KEY 或 ANTHROPIC_API_KEY 后重新打开 AI 面板。"
                    }
                }
            }
        }
    }
}
