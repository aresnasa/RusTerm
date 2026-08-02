use dioxus::html::input_data::MouseButton;
use dioxus::prelude::*;
use rusterm_core::config::{
    BottomPanelTab, MAX_BOTTOM_PANEL_HEIGHT_PX, MIN_BOTTOM_PANEL_HEIGHT_PX,
};

use crate::components::SendPanel;
use crate::components::TransfersPanel;
use crate::state::{AppState, SendTargetOption};
use crate::transfers::TransferJob;

#[component]
pub fn BottomToolPanel(
    height_px: u16,
    embedded: bool,
    active_tab: BottomPanelTab,
    target_label: String,
    target_options: Vec<SendTargetOption>,
    selected_target_ids: Vec<String>,
    shell_content: Option<Element>,
    transfer_jobs: Vec<TransferJob>,
    /// App state, used by the Send sub-panel to query history-based
    /// completions (SQLite frecency + DuckDB frequency). Passed straight
    /// through to [`SendPanel`].
    state: Signal<AppState>,
    on_height_change: EventHandler<u16>,
    on_tab_change: EventHandler<BottomPanelTab>,
    on_send: EventHandler<String>,
    on_target_toggle: EventHandler<(String, bool)>,
    on_select_all_targets: EventHandler<()>,
    on_invert_targets: EventHandler<()>,
    on_open_shell: EventHandler<()>,
    on_terminate_shell: EventHandler<()>,
    on_cancel_transfer: EventHandler<String>,
    on_retry_transfer: EventHandler<String>,
    on_clear_transfers: EventHandler<()>,
    on_close: EventHandler<()>,
) -> Element {
    // Subscribe to language changes so tabs/labels re-render on switch.
    let _lang = crate::i18n::LANGUAGE();
    let mut live_height = use_signal(|| height_px);
    let mut resize_drag = use_signal(|| Option::<(f64, u16)>::None);
    let mut target_picker_open = use_signal(|| false);
    let has_targets = !selected_target_ids.is_empty();
    let target_rows = target_options
        .into_iter()
        .map(|target| {
            let selected = selected_target_ids.contains(&target.session_id);
            (target.session_id, target.label, selected)
        })
        .collect::<Vec<_>>();
    let selected_target_count = selected_target_ids.len();
    let target_count = target_rows.len();
    let has_available_targets = target_count > 0;

    rsx! {
        style { "
            .workspace-tab{{border:0;background:transparent;color:var(--skin-text-muted);padding:7px 9px;font-size:11px;cursor:pointer;border-bottom:2px solid transparent;white-space:nowrap;}}
            .workspace-tab:hover{{color:var(--skin-text);background:var(--skin-surface-hover);}}
            .workspace-tab.active{{color:var(--skin-accent);border-bottom-color:var(--skin-accent);}}
            .workspace-primary-button{{border:1px solid var(--skin-accent);background:var(--skin-accent);color:var(--skin-bg);border-radius:4px;padding:6px 12px;font-size:11px;font-weight:600;cursor:pointer;}}
            .workspace-primary-button:disabled{{opacity:.45;cursor:default;}}
            .send-target-button{{margin-left:auto;max-width:260px;border:1px solid transparent;background:transparent;color:var(--skin-text-muted);padding:4px 8px;font-size:10px;cursor:pointer;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;}}
            .send-target-button:hover,.send-target-button.active{{color:var(--skin-text);background:var(--skin-surface-hover);border-color:var(--skin-border);}}
            .send-target-action{{border:1px solid var(--skin-border);background:var(--skin-surface);color:var(--skin-text-muted);border-radius:3px;padding:3px 8px;font-size:10px;cursor:pointer;}}
            .send-target-action:hover{{color:var(--skin-accent);border-color:var(--skin-accent);}}
            .workspace-resize-handle:hover,.workspace-resize-handle.active{{background:var(--skin-accent);box-shadow:0 0 6px rgba(122,162,247,.5);}}
        " }
        div {
            style: if embedded {
                "position:relative;width:100%;min-width:0;height:100%;min-height:0;display:flex;flex-direction:column;background:var(--skin-bg);box-sizing:border-box;overflow:hidden;".to_string()
            } else {
                format!("position:relative;height:min({live_height}px,55vh);min-height:min({live_height}px,55vh);max-height:min({live_height}px,55vh);flex:0 0 min({live_height}px,55vh);display:flex;flex-direction:column;background:var(--skin-bg);border-top:1px solid var(--skin-border);box-sizing:border-box;overflow:hidden;")
            },
            div {
                style: "display:flex;align-items:center;border-bottom:1px solid var(--skin-border);min-width:0;",
                if !embedded {
                button {
                    class: if active_tab == BottomPanelTab::Send { "workspace-tab active" } else { "workspace-tab" },
                    onclick: move |_| on_tab_change.call(BottomPanelTab::Send),
                    { crate::i18n::t("send.tab_title") }
                }
                button {
                    class: if active_tab == BottomPanelTab::Shell { "workspace-tab active" } else { "workspace-tab" },
                    onclick: move |_| on_tab_change.call(BottomPanelTab::Shell),
                    { crate::i18n::t("shell.tab_title") }
                }
                button {
                    class: if active_tab == BottomPanelTab::Transfers { "workspace-tab active" } else { "workspace-tab" },
                    onclick: move |_| on_tab_change.call(BottomPanelTab::Transfers),
                    { crate::i18n::t("transfers.tab_title") }
                }
                button {
                    class: if active_tab == BottomPanelTab::Api { "workspace-tab active" } else { "workspace-tab" },
                    onclick: move |_| on_tab_change.call(BottomPanelTab::Api),
                    { crate::i18n::t("api.tab_title") }
                }
                }
                if active_tab == BottomPanelTab::Send {
                    button {
                        class: if target_picker_open() { "send-target-button active" } else { "send-target-button" },
                        title: crate::i18n::t("send.choose_targets"),
                        onclick: move |event: MouseEvent| {
                            event.stop_propagation();
                            target_picker_open.set(!target_picker_open());
                        },
                        { crate::i18n::tf("send.target", &[("label", &target_label)]) }
                    }
                } else {
                    span { style: "margin-left:auto;" }
                }
                if active_tab == BottomPanelTab::Shell && shell_content.is_some() {
                    button {
                        style: "border:0;background:transparent;color:var(--skin-danger);cursor:pointer;padding:4px 7px;font-size:11px;",
                        title: crate::i18n::t("shell.terminate"),
                        onclick: move |_| on_terminate_shell.call(()),
                        { crate::i18n::t("shell.terminate") }
                    }
                }
                if !embedded {
                button {
                    style: "margin-right:5px;border:0;background:transparent;color:var(--skin-text-muted);cursor:pointer;padding:4px 7px;font-size:14px;",
                    title: "Hide bottom panel",
                    onclick: move |_| on_close.call(()),
                    "×"
                }
                }
            }

            if active_tab == BottomPanelTab::Send && target_picker_open() {
                div {
                    // Anchor to both edges of the real dock container. When the
                    // user resizes the bottom dock, CSS constrains this picker
                    // to the remaining content area and its list scrolls.
                    style: "position:absolute;right:8px;top:31px;bottom:8px;z-index:120;width:min(320px,calc(100% - 16px));max-height:240px;min-height:0;display:flex;flex-direction:column;background:var(--skin-bg);border:1px solid var(--skin-border);border-radius:5px;box-shadow:0 8px 24px rgba(0,0,0,.4);overflow:hidden;",
                    onclick: move |event: MouseEvent| event.stop_propagation(),
                    div {
                        style: "display:flex;align-items:center;flex-wrap:wrap;gap:6px;padding:7px;border-bottom:1px solid var(--skin-border);",
                        span { style: "color:var(--skin-text);font-size:11px;font-weight:600;", { crate::i18n::t("send.targets") } }
                        span { style: "margin-left:auto;color:var(--skin-text-muted);font-size:10px;", "{selected_target_count}/{target_count} selected" }
                        button {
                            class: "send-target-action",
                            disabled: !has_available_targets,
                            onclick: move |_| on_select_all_targets.call(()),
                            { crate::i18n::t("send.all") }
                        }
                        button {
                            class: "send-target-action",
                            disabled: !has_available_targets,
                            onclick: move |_| on_invert_targets.call(()),
                            { crate::i18n::t("send.invert") }
                        }
                    }
                    div {
                        style: "min-height:0;overflow:auto;padding:4px;",
                        if !has_available_targets {
                            div { style: "padding:12px 8px;text-align:center;color:var(--skin-text-muted);font-size:11px;", { crate::i18n::t("send.no_sessions") } }
                        }
                        for (session_id, label, selected) in target_rows {
                            label {
                                key: "send-target-{session_id}",
                                style: "display:flex;align-items:center;gap:8px;padding:6px 7px;border-radius:3px;color:var(--skin-text);font-size:11px;cursor:pointer;",
                                input {
                                    r#type: "checkbox",
                                    checked: selected,
                                    style: "accent-color:var(--skin-accent);cursor:pointer;",
                                    onchange: move |event| on_target_toggle.call((session_id.clone(), event.checked())),
                                }
                                span { style: "min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;", "{label}" }
                            }
                        }
                    }
                }
            }

            match active_tab {
                BottomPanelTab::Send => rsx! {
                    SendPanel {
                        state,
                        has_targets,
                        on_send: move |command: String| on_send.call(command),
                    }
                },
                BottomPanelTab::Shell => rsx! {
                    if let Some(content) = shell_content {
                        div {
                            style: "flex:1;min-height:0;overflow:hidden;position:relative;",
                            {content}
                        }
                    } else {
                        div {
                            style: "flex:1;display:flex;flex-direction:column;align-items:center;justify-content:center;gap:9px;color:var(--skin-text-muted);font-size:12px;",
                            div { style: "font-size:24px;color:var(--skin-accent);", ">_" }
                            div { { crate::i18n::t("shell.start_hint") } }
                            button { class: "workspace-primary-button", onclick: move |_| on_open_shell.call(()), { crate::i18n::t("shell.start") } }
                        }
                    }
                },
                BottomPanelTab::Transfers => rsx! {
                    TransfersPanel {
                        jobs: transfer_jobs,
                        on_cancel: move |job_id| on_cancel_transfer.call(job_id),
                        on_retry: move |job_id| on_retry_transfer.call(job_id),
                        on_clear_finished: move |_| on_clear_transfers.call(()),
                    }
                },
                BottomPanelTab::Api => rsx! {
                    crate::components::ApiPanel { state }
                },
            }

            if !embedded && resize_drag().is_some() {
                div {
                    style: "position:fixed;inset:0;z-index:79;cursor:row-resize;background:transparent;",
                    onmousemove: move |event: MouseEvent| {
                        let Some((start_y, start_height)) = resize_drag() else { return; };
                        let delta = start_y - event.client_coordinates().y;
                        live_height.set((f64::from(start_height) + delta).round().clamp(f64::from(MIN_BOTTOM_PANEL_HEIGHT_PX), f64::from(MAX_BOTTOM_PANEL_HEIGHT_PX)) as u16);
                    },
                    onmouseup: move |_| {
                        resize_drag.set(None);
                        on_height_change.call(live_height());
                    },
                }
            }
            if !embedded {
            div {
                class: if resize_drag().is_some() { "workspace-resize-handle active" } else { "workspace-resize-handle" },
                style: "position:absolute;left:0;top:-3px;width:100%;height:6px;z-index:80;cursor:row-resize;background:transparent;",
                onmousedown: move |event: MouseEvent| {
                    if event.trigger_button() == Some(MouseButton::Primary) {
                        event.prevent_default();
                        resize_drag.set(Some((event.client_coordinates().y, live_height())));
                    }
                },
            }
            }
        }
    }
}
