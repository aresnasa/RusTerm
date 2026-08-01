use dioxus::prelude::*;
use rusterm_ai::{ShadowExecutionRequest, ShadowExecutionResult};

#[component]
pub fn ShadowExecutionDialog(
    request: ShadowExecutionRequest,
    on_execute: EventHandler<()>,
    on_cancel: EventHandler<()>,
) -> Element {
    let cwd = request.working_directory.as_deref().unwrap_or("未知");

    rsx! {
        div {
            style: "position:fixed;inset:0;background:rgba(0,0,0,0.72);display:flex;align-items:center;justify-content:center;z-index:2200;",
            div {
                style: "width:min(620px,90vw);background:var(--skin-surface);border:1px solid var(--skin-warning);border-radius:8px;padding:24px;color:var(--skin-text);box-shadow:0 12px 40px rgba(0,0,0,0.55);",
                h2 { style: "margin:0 0 6px;font-size:18px;color:var(--skin-warning);", "影子沙盒：确认执行" }
                p {
                    style: "margin:0 0 18px;color:var(--skin-text-muted);font-size:12px;line-height:1.6;",
                    "以下内容只是模型建议。模型不能执行命令；只有你点击“确认执行”后，命令才会写入当前登录会话。"
                }

                div { style: "display:grid;grid-template-columns:92px 1fr;gap:8px 12px;margin-bottom:14px;font-size:12px;",
                    span { style: "color:var(--skin-text-muted);", "目标会话" }
                    strong { "{request.target_label}" }
                    span { style: "color:var(--skin-text-muted);", "工作目录" }
                    code { style: "word-break:break-all;", "{cwd}" }
                }

                div {
                    style: "background:var(--skin-bg);border:1px solid var(--skin-border-strong);border-radius:5px;padding:13px;margin-bottom:14px;max-height:160px;overflow:auto;font:13px 'SF Mono','Menlo','Consolas',monospace;color:var(--skin-warning);white-space:pre-wrap;word-break:break-all;",
                    "{request.command}"
                }

                if let Some(reason) = request.risk_reason.as_ref() {
                    div {
                        style: "background:color-mix(in srgb,var(--skin-danger) 12%,transparent);border-left:3px solid var(--skin-danger);padding:10px 12px;margin-bottom:16px;font-size:12px;line-height:1.5;",
                        strong { style: "color:var(--skin-danger);", "风险提示：" }
                        "{reason}"
                    }
                } else {
                    div {
                        style: "background:color-mix(in srgb,var(--skin-warning) 10%,transparent);border-left:3px solid var(--skin-warning);padding:10px 12px;margin-bottom:16px;font-size:12px;line-height:1.5;",
                        "请自行核对参数、引号、目标主机和工作目录。确认后命令会在真实会话中执行，并非 OS 隔离沙盒。"
                    }
                }

                div { style: "display:flex;gap:12px;justify-content:flex-end;",
                    button {
                        style: "background:var(--skin-bg);color:var(--skin-text);border:1px solid var(--skin-border-strong);border-radius:4px;padding:9px 18px;cursor:pointer;",
                        onclick: move |_| on_cancel.call(()),
                        "取消"
                    }
                    button {
                        style: "background:var(--skin-warning);color:var(--skin-bg);border:0;border-radius:4px;padding:9px 18px;font-weight:700;cursor:pointer;",
                        onclick: move |_| on_execute.call(()),
                        "确认执行"
                    }
                }
            }
        }
    }
}

#[component]
pub fn ShadowResultDialog(
    result: ShadowExecutionResult,
    on_share: EventHandler<()>,
    on_reject: EventHandler<()>,
) -> Element {
    let cwd = result.working_directory.as_deref().unwrap_or("未知");
    let exit_label = result
        .exit_code
        .map(|code| code.to_string())
        .unwrap_or_else(|| "未获取".to_string());
    let truncation_note = result
        .truncated
        .then_some("输出超过 64 KiB，预览和共享内容已截断。")
        .unwrap_or("");

    rsx! {
        div {
            style: "position:fixed;inset:0;background:rgba(0,0,0,0.72);display:flex;align-items:center;justify-content:center;z-index:2200;",
            div {
                style: "width:min(720px,92vw);background:var(--skin-surface);border:1px solid var(--skin-accent);border-radius:8px;padding:24px;color:var(--skin-text);box-shadow:0 12px 40px rgba(0,0,0,0.55);",
                h2 { style: "margin:0 0 6px;font-size:18px;color:var(--skin-accent);", "影子沙盒：执行结果待授权" }
                p {
                    style: "margin:0 0 16px;color:var(--skin-text-muted);font-size:12px;line-height:1.6;",
                    "结果目前只保存在本地临时状态，尚未加入 LLM 请求。请预览后决定是否允许发送给模型。"
                }

                div { style: "display:grid;grid-template-columns:92px 1fr;gap:7px 12px;margin-bottom:12px;font-size:12px;",
                    span { style: "color:var(--skin-text-muted);", "目标会话" }
                    strong { "{result.target_label}" }
                    span { style: "color:var(--skin-text-muted);", "工作目录" }
                    code { "{cwd}" }
                    span { style: "color:var(--skin-text-muted);", "退出码" }
                    code { "{exit_label}" }
                    span { style: "color:var(--skin-text-muted);", "命令" }
                    code { style: "word-break:break-all;", "{result.command}" }
                }

                pre {
                    style: "margin:0 0 8px;background:var(--skin-bg);border:1px solid var(--skin-border-strong);border-radius:5px;padding:13px;max-height:280px;overflow:auto;color:var(--skin-text);font:12px/1.5 'SF Mono','Menlo','Consolas',monospace;white-space:pre-wrap;word-break:break-word;",
                    "{result.output}"
                }
                if !truncation_note.is_empty() {
                    p { style: "margin:0 0 12px;color:var(--skin-warning);font-size:11px;", "{truncation_note}" }
                }

                div { style: "display:flex;gap:12px;justify-content:flex-end;margin-top:16px;",
                    button {
                        style: "background:var(--skin-bg);color:var(--skin-text);border:1px solid var(--skin-border-strong);border-radius:4px;padding:9px 18px;cursor:pointer;",
                        onclick: move |_| on_reject.call(()),
                        "不分享"
                    }
                    button {
                        style: "background:var(--skin-accent);color:var(--skin-bg);border:0;border-radius:4px;padding:9px 18px;font-weight:700;cursor:pointer;",
                        onclick: move |_| on_share.call(()),
                        "确认发送给模型"
                    }
                }
            }
        }
    }
}
