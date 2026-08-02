use dioxus::prelude::*;
use rusterm_ai::{ShadowExecutionRequest, ShadowExecutionResult};

#[component]
pub fn ShadowExecutionDialog(
    request: ShadowExecutionRequest,
    on_execute: EventHandler<()>,
    on_cancel: EventHandler<()>,
) -> Element {
    let _lang = crate::i18n::LANGUAGE();
    let cwd = request
        .working_directory
        .as_deref()
        .map(str::to_owned)
        .unwrap_or_else(|| crate::i18n::t("shadow.unknown"));

    rsx! {
        div {
            style: "position:fixed;inset:0;background:rgba(0,0,0,0.72);display:flex;align-items:center;justify-content:center;z-index:2200;",
            div {
                style: "width:min(620px,90vw);background:var(--skin-surface);border:1px solid var(--skin-warning);border-radius:8px;padding:24px;color:var(--skin-text);box-shadow:0 12px 40px rgba(0,0,0,0.55);",
                h2 { style: "margin:0 0 6px;font-size:18px;color:var(--skin-warning);", { crate::i18n::t("shadow.execution_title") } }
                p {
                    style: "margin:0 0 18px;color:var(--skin-text-muted);font-size:12px;line-height:1.6;",
                    { crate::i18n::t("shadow.execution_description") }
                }

                div { style: "display:grid;grid-template-columns:92px 1fr;gap:8px 12px;margin-bottom:14px;font-size:12px;",
                    span { style: "color:var(--skin-text-muted);", { crate::i18n::t("shadow.target_session") } }
                    strong { "{request.target_label}" }
                    span { style: "color:var(--skin-text-muted);", { crate::i18n::t("shadow.working_directory") } }
                    code { style: "word-break:break-all;", "{cwd}" }
                }

                div {
                    style: "background:var(--skin-bg);border:1px solid var(--skin-border-strong);border-radius:5px;padding:13px;margin-bottom:14px;max-height:160px;overflow:auto;font:13px 'SF Mono','Menlo','Consolas',monospace;color:var(--skin-warning);white-space:pre-wrap;word-break:break-all;",
                    "{request.command}"
                }

                if let Some(reason) = request.risk_reason.as_ref() {
                    div {
                        style: "background:color-mix(in srgb,var(--skin-danger) 12%,transparent);border-left:3px solid var(--skin-danger);padding:10px 12px;margin-bottom:16px;font-size:12px;line-height:1.5;",
                        strong { style: "color:var(--skin-danger);", { crate::i18n::t("shadow.risk_warning") } }
                        "{reason}"
                    }
                } else {
                    div {
                        style: "background:color-mix(in srgb,var(--skin-warning) 10%,transparent);border-left:3px solid var(--skin-warning);padding:10px 12px;margin-bottom:16px;font-size:12px;line-height:1.5;",
                        { crate::i18n::t("shadow.execution_warning") }
                    }
                }

                div { style: "display:flex;gap:12px;justify-content:flex-end;",
                    button {
                        style: "background:var(--skin-bg);color:var(--skin-text);border:1px solid var(--skin-border-strong);border-radius:4px;padding:9px 18px;cursor:pointer;",
                        onclick: move |_| on_cancel.call(()),
                        { crate::i18n::t("common.cancel") }
                    }
                    button {
                        style: "background:var(--skin-warning);color:var(--skin-bg);border:0;border-radius:4px;padding:9px 18px;font-weight:700;cursor:pointer;",
                        onclick: move |_| on_execute.call(()),
                        { crate::i18n::t("shadow.confirm_execute") }
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
    let _lang = crate::i18n::LANGUAGE();
    let cwd = result
        .working_directory
        .as_deref()
        .map(str::to_owned)
        .unwrap_or_else(|| crate::i18n::t("shadow.unknown"));
    let exit_label = result
        .exit_code
        .map(|code| code.to_string())
        .unwrap_or_else(|| crate::i18n::t("shadow.exit_code_unavailable"));
    let truncation_note = result
        .truncated
        .then(|| crate::i18n::tf("shadow.output_truncated", &[("limit", &"64 KiB")]));

    rsx! {
        div {
            style: "position:fixed;inset:0;background:rgba(0,0,0,0.72);display:flex;align-items:center;justify-content:center;z-index:2200;",
            div {
                style: "width:min(720px,92vw);background:var(--skin-surface);border:1px solid var(--skin-accent);border-radius:8px;padding:24px;color:var(--skin-text);box-shadow:0 12px 40px rgba(0,0,0,0.55);",
                h2 { style: "margin:0 0 6px;font-size:18px;color:var(--skin-accent);", { crate::i18n::t("shadow.result_title") } }
                p {
                    style: "margin:0 0 16px;color:var(--skin-text-muted);font-size:12px;line-height:1.6;",
                    { crate::i18n::t("shadow.result_description") }
                }

                div { style: "display:grid;grid-template-columns:92px 1fr;gap:7px 12px;margin-bottom:12px;font-size:12px;",
                    span { style: "color:var(--skin-text-muted);", { crate::i18n::t("shadow.target_session") } }
                    strong { "{result.target_label}" }
                    span { style: "color:var(--skin-text-muted);", { crate::i18n::t("shadow.working_directory") } }
                    code { "{cwd}" }
                    span { style: "color:var(--skin-text-muted);", { crate::i18n::t("shadow.exit_code") } }
                    code { "{exit_label}" }
                    span { style: "color:var(--skin-text-muted);", { crate::i18n::t("shadow.command") } }
                    code { style: "word-break:break-all;", "{result.command}" }
                }

                pre {
                    style: "margin:0 0 8px;background:var(--skin-bg);border:1px solid var(--skin-border-strong);border-radius:5px;padding:13px;max-height:280px;overflow:auto;color:var(--skin-text);font:12px/1.5 'SF Mono','Menlo','Consolas',monospace;white-space:pre-wrap;word-break:break-word;",
                    "{result.output}"
                }
                if let Some(truncation_note) = truncation_note.as_ref() {
                    p { style: "margin:0 0 12px;color:var(--skin-warning);font-size:11px;", "{truncation_note}" }
                }

                div { style: "display:flex;gap:12px;justify-content:flex-end;margin-top:16px;",
                    button {
                        style: "background:var(--skin-bg);color:var(--skin-text);border:1px solid var(--skin-border-strong);border-radius:4px;padding:9px 18px;cursor:pointer;",
                        onclick: move |_| on_reject.call(()),
                        { crate::i18n::t("shadow.do_not_share") }
                    }
                    button {
                        style: "background:var(--skin-accent);color:var(--skin-bg);border:0;border-radius:4px;padding:9px 18px;font-weight:700;cursor:pointer;",
                        onclick: move |_| on_share.call(()),
                        { crate::i18n::t("shadow.confirm_send_to_model") }
                    }
                }
            }
        }
    }
}
