use dioxus::html::input_data::MouseButton;
use dioxus::prelude::*;
use rusterm_core::config::{
    DockLayout, DockStackState, DockZone, MAX_BOTTOM_PANEL_HEIGHT_PX, MAX_RIGHT_PANEL_WIDTH_PX,
    MAX_SIDEBAR_WIDTH_PX, MIN_BOTTOM_PANEL_HEIGHT_PX, MIN_RIGHT_PANEL_WIDTH_PX,
    MIN_SIDEBAR_WIDTH_PX, PanelId,
};

const DOCK_DRAG_THRESHOLD_PX: f64 = 6.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DockDropTarget {
    pub zone: DockZone,
    pub index: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DockDragState {
    pub panel: PanelId,
    pub label: &'static str,
    pub start_x: f64,
    pub start_y: f64,
    pub cur_x: f64,
    pub cur_y: f64,
    pub dragging: bool,
    pub target: Option<DockDropTarget>,
}

pub fn panel_label(panel: PanelId) -> &'static str {
    match panel {
        PanelId::Connections => "Connections",
        PanelId::RemoteFiles => "Remote files",
        PanelId::Sessions => "Sessions",
        PanelId::History => "History",
        PanelId::Send => "Send",
        PanelId::EmbeddedShell => "Shell",
        PanelId::Transfers => "Transfers",
    }
}

fn zone_name(zone: DockZone) -> &'static str {
    match zone {
        DockZone::Left => "left",
        DockZone::Right => "right",
        DockZone::Bottom => "bottom",
    }
}

fn parse_zone(value: &str) -> Option<DockZone> {
    match value {
        "left" => Some(DockZone::Left),
        "right" => Some(DockZone::Right),
        "bottom" => Some(DockZone::Bottom),
        _ => None,
    }
}

pub fn dock_drag_threshold_exceeded(start_x: f64, start_y: f64, x: f64, y: f64) -> bool {
    let dx = x - start_x;
    let dy = y - start_y;
    dx * dx + dy * dy >= DOCK_DRAG_THRESHOLD_PX * DOCK_DRAG_THRESHOLD_PX
}

pub fn adjusted_drop_index(layout: &DockLayout, panel: PanelId, target: DockDropTarget) -> usize {
    let mut index = target.index.min(layout.stack(target.zone).panels.len());
    if layout.zone_for(panel) == Some(target.zone) {
        if let Some(source_index) = layout
            .stack(target.zone)
            .panels
            .iter()
            .position(|candidate| *candidate == panel)
        {
            if source_index < index {
                index = index.saturating_sub(1);
            }
        }
    }
    index
}

pub fn start_dock_drag(mut drag: Signal<Option<DockDragState>>, panel: PanelId, x: f64, y: f64) {
    drag.set(Some(DockDragState {
        panel,
        label: panel_label(panel),
        start_x: x,
        start_y: y,
        cur_x: x,
        cur_y: y,
        dragging: false,
        target: None,
    }));
    spawn(async move {
        let _ = install_dock_drag_listeners(x, y).await;
    });
}

fn build_install_dock_drag_script(initial_x: f64, initial_y: f64) -> String {
    format!(
        "(function() {{\n\
            window.__rusterm_dock_drag_pos = '{initial_x},{initial_y}';\n\
            window.__rusterm_dock_drag_done = false;\n\
            if (window._rusterm_dock_drag_remove) {{ window._rusterm_dock_drag_remove(); }}\n\
            var previousWebkitUserSelect = document.body.style.webkitUserSelect;\n\
            var previousUserSelect = document.body.style.userSelect;\n\
            document.body.style.webkitUserSelect = 'none';\n\
            document.body.style.userSelect = 'none';\n\
            if (window.getSelection) {{ window.getSelection().removeAllRanges(); }}\n\
            var moveHandler = function(e) {{\n\
                window.__rusterm_dock_drag_pos = e.clientX + ',' + e.clientY;\n\
                e.preventDefault();\n\
            }};\n\
            var upHandler = function(e) {{\n\
                window.__rusterm_dock_drag_pos = e.clientX + ',' + e.clientY;\n\
                window.__rusterm_dock_drag_done = true;\n\
                if (window._rusterm_dock_drag_remove) {{\n\
                    window._rusterm_dock_drag_remove();\n\
                    window._rusterm_dock_drag_remove = null;\n\
                }}\n\
            }};\n\
            document.addEventListener('mousemove', moveHandler, true);\n\
            document.addEventListener('mouseup', upHandler, true);\n\
            window._rusterm_dock_drag_remove = function() {{\n\
                document.removeEventListener('mousemove', moveHandler, true);\n\
                document.removeEventListener('mouseup', upHandler, true);\n\
                document.body.style.webkitUserSelect = previousWebkitUserSelect;\n\
                document.body.style.userSelect = previousUserSelect;\n\
                if (window.getSelection) {{ window.getSelection().removeAllRanges(); }}\n\
            }};\n\
        }})()"
    )
}

async fn install_dock_drag_listeners(initial_x: f64, initial_y: f64) -> Result<(), String> {
    dioxus::document::eval(&build_install_dock_drag_script(initial_x, initial_y))
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
}

pub async fn poll_dock_drag_state() -> Option<(f64, f64, bool, Option<DockDropTarget>)> {
    let result = dioxus::document::eval(
        "return (function() {\n\
            var pos = window.__rusterm_dock_drag_pos || '';\n\
            if (!pos) return '';\n\
            var coords = pos.split(',');\n\
            if (coords.length !== 2) return '';\n\
            var x = Number(coords[0]);\n\
            var y = Number(coords[1]);\n\
            if (!Number.isFinite(x) || !Number.isFinite(y)) return '';\n\
            var targetZone = '';\n\
            var targetIndex = '';\n\
            var zones = ['left', 'right', 'bottom'];\n\
            for (var zoneIndex = 0; zoneIndex < zones.length; zoneIndex++) {\n\
                var zone = zones[zoneIndex];\n\
                var element = document.querySelector('[data-rusterm-dock-zone=\"' + zone + '\"]');\n\
                if (!element) continue;\n\
                var rect = element.getBoundingClientRect();\n\
                if (x < rect.left || x >= rect.right || y < rect.top || y >= rect.bottom) continue;\n\
                var tabs = Array.from(element.querySelectorAll('[data-rusterm-dock-tab-index]'));\n\
                var insertion = tabs.length;\n\
                var tabStrip = element.querySelector('[data-rusterm-dock-tabs]');\n\
                var tabStripRect = tabStrip ? tabStrip.getBoundingClientRect() : null;\n\
                if (tabStripRect && y >= tabStripRect.top && y < tabStripRect.bottom) {\n\
                    for (var tabIndex = 0; tabIndex < tabs.length; tabIndex++) {\n\
                        var tabRect = tabs[tabIndex].getBoundingClientRect();\n\
                        if (x < tabRect.left + tabRect.width / 2) { insertion = tabIndex; break; }\n\
                    }\n\
                }\n\
                targetZone = zone;\n\
                targetIndex = String(insertion);\n\
                break;\n\
            }\n\
            if (!targetZone) {\n\
                var rails = Array.from(document.querySelectorAll('[data-rusterm-dock-hidden-edge]'));\n\
                for (var railIndex = 0; railIndex < rails.length; railIndex++) {\n\
                    var rail = rails[railIndex];\n\
                    var railRect = rail.getBoundingClientRect();\n\
                    if (x < railRect.left || x >= railRect.right || y < railRect.top || y >= railRect.bottom) continue;\n\
                    targetZone = rail.getAttribute('data-rusterm-dock-hidden-edge') || '';\n\
                    targetIndex = rail.getAttribute('data-rusterm-dock-panel-count') || '0';\n\
                    break;\n\
                }\n\
            }\n\
            return [coords[0], coords[1], window.__rusterm_dock_drag_done ? '1' : '0', targetZone, targetIndex].join('\\u001f');\n\
        })()",
    )
    .await
    .ok()?;
    parse_dock_drag_poll_response(result.as_str()?)
}

fn parse_dock_drag_poll_response(
    response: &str,
) -> Option<(f64, f64, bool, Option<DockDropTarget>)> {
    let mut fields = response.split('\u{1f}');
    let x = fields.next()?.parse().ok()?;
    let y = fields.next()?.parse().ok()?;
    let done = match fields.next()? {
        "0" => false,
        "1" => true,
        _ => return None,
    };
    let zone = fields.next()?;
    let index = fields.next()?;
    if fields.next().is_some() {
        return None;
    }
    let target = if zone.is_empty() {
        None
    } else {
        Some(DockDropTarget {
            zone: parse_zone(zone)?,
            index: index.parse().ok()?,
        })
    };
    Some((x, y, done, target))
}

#[component]
pub fn DockZoneView(
    zone: DockZone,
    stack: DockStackState,
    drag: Option<DockDragState>,
    content: Element,
    on_activate: EventHandler<PanelId>,
    on_hide: EventHandler<DockZone>,
    on_extent_change: EventHandler<(DockZone, u16)>,
    on_drag_start: EventHandler<(PanelId, f64, f64)>,
) -> Element {
    if !stack.visible || stack.panels.is_empty() {
        return rsx! {};
    }

    let incoming_extent = stack.extent_px;
    let mut live_extent = use_signal(|| incoming_extent);
    let mut resize_drag = use_signal(|| Option::<(f64, u16)>::None);
    use_effect(move || {
        if resize_drag.peek().is_none() && *live_extent.peek() != incoming_extent {
            live_extent.set(incoming_extent);
        }
    });

    let extent = live_extent();
    let zone_value = zone_name(zone);
    let outer_style = match zone {
        DockZone::Left => format!(
            "position:relative;width:min({extent}px,45vw);min-width:min({extent}px,45vw);max-width:min({extent}px,45vw);flex:0 0 min({extent}px,45vw);height:100%;display:flex;flex-direction:column;overflow:hidden;background:var(--skin-bg);border-right:1px solid var(--skin-border);box-sizing:border-box;"
        ),
        DockZone::Right => format!(
            "position:relative;width:min({extent}px,45vw);min-width:min({extent}px,45vw);max-width:min({extent}px,45vw);flex:0 0 min({extent}px,45vw);height:100%;display:flex;flex-direction:column;overflow:hidden;background:var(--skin-bg);border-left:1px solid var(--skin-border);box-sizing:border-box;"
        ),
        DockZone::Bottom => format!(
            "position:relative;height:min({extent}px,55vh);min-height:min({extent}px,55vh);max-height:min({extent}px,55vh);flex:0 0 min({extent}px,55vh);width:100%;display:flex;flex-direction:column;overflow:hidden;background:var(--skin-bg);border-top:1px solid var(--skin-border);box-sizing:border-box;"
        ),
    };
    let resize_cursor = if zone == DockZone::Bottom {
        "row-resize"
    } else {
        "col-resize"
    };
    let handle_style = match zone {
        DockZone::Left => {
            "position:absolute;right:-3px;top:0;width:6px;height:100%;z-index:80;cursor:col-resize;background:transparent;"
        }
        DockZone::Right => {
            "position:absolute;left:-3px;top:0;width:6px;height:100%;z-index:80;cursor:col-resize;background:transparent;"
        }
        DockZone::Bottom => {
            "position:absolute;left:0;top:-3px;width:100%;height:6px;z-index:80;cursor:row-resize;background:transparent;"
        }
    };
    let target_index = drag
        .as_ref()
        .filter(|drag| drag.dragging)
        .and_then(|drag| drag.target)
        .filter(|target| target.zone == zone)
        .map(|target| target.index);
    let panel_count = stack.panels.len();

    rsx! {
        style { r#"
            .dock-tab {{ border:0;background:transparent;color:var(--skin-text-muted);padding:7px 9px;font-size:11px;cursor:pointer;border-bottom:2px solid transparent;white-space:nowrap;user-select:none;-webkit-user-select:none; }}
            .dock-tab:hover {{ color:var(--skin-text);background:var(--skin-surface-hover); }}
            .dock-tab.active {{ color:var(--skin-accent);border-bottom-color:var(--skin-accent); }}
            .dock-close {{ margin-left:auto;margin-right:5px;border:0;background:transparent;color:var(--skin-text-muted);cursor:pointer;padding:4px 7px;font-size:14px; }}
            .dock-close:hover {{ color:var(--skin-text);background:var(--skin-surface-hover); }}
            .dock-insertion {{ width:2px;align-self:stretch;flex:0 0 2px;background:var(--skin-accent);box-shadow:0 0 6px color-mix(in srgb,var(--skin-accent) 75%,transparent); }}
            .dock-resize-handle:hover,.dock-resize-handle.active {{ background:var(--skin-accent);box-shadow:0 0 6px color-mix(in srgb,var(--skin-accent) 55%,transparent); }}
        "# }
        div {
            "data-rusterm-dock-zone": "{zone_value}",
            "data-rusterm-dock-panel-count": "{panel_count}",
            style: "{outer_style}",
            div {
                "data-rusterm-dock-tabs": "true",
                style: "height:31px;min-height:31px;display:flex;align-items:stretch;border-bottom:1px solid var(--skin-border);min-width:0;overflow-x:auto;overflow-y:hidden;",
                for (index, panel) in stack.panels.iter().copied().enumerate() {
                    if target_index == Some(index) {
                        span { class: "dock-insertion" }
                    }
                    button {
                        class: if stack.active == Some(panel) { "dock-tab active" } else { "dock-tab" },
                        "data-rusterm-dock-tab-index": "{index}",
                        title: "Drag to reorder or move {panel_label(panel)}",
                        onclick: move |_| on_activate.call(panel),
                        onmousedown: move |event: MouseEvent| {
                            if event.trigger_button() == Some(MouseButton::Primary) {
                                event.prevent_default();
                                event.stop_propagation();
                                let coordinates = event.client_coordinates();
                                on_drag_start.call((panel, coordinates.x, coordinates.y));
                            }
                        },
                        {panel_label(panel)}
                    }
                }
                if target_index.is_some_and(|index| index >= panel_count) {
                    span { class: "dock-insertion" }
                }
                button {
                    class: "dock-close",
                    title: "Hide {zone_value} dock",
                    onclick: move |_| on_hide.call(zone),
                    "×"
                }
            }
            div {
                style: "flex:1;min-width:0;min-height:0;overflow:hidden;position:relative;",
                {content}
            }
            if target_index.is_some() {
                div {
                    style: "position:absolute;inset:0;z-index:75;pointer-events:none;border:2px solid var(--skin-accent);background:color-mix(in srgb,var(--skin-accent) 7%,transparent);box-sizing:border-box;",
                }
            }
            if resize_drag().is_some() {
                div {
                    style: "position:fixed;inset:0;z-index:79;cursor:{resize_cursor};background:transparent;",
                    onmousemove: move |event: MouseEvent| {
                        let Some((start_position, start_extent)) = resize_drag() else { return; };
                        event.prevent_default();
                        let coordinates = event.client_coordinates();
                        let delta = match zone {
                            DockZone::Left => coordinates.x - start_position,
                            DockZone::Right => start_position - coordinates.x,
                            DockZone::Bottom => start_position - coordinates.y,
                        };
                        let (minimum, maximum) = match zone {
                            DockZone::Left => (MIN_SIDEBAR_WIDTH_PX, MAX_SIDEBAR_WIDTH_PX),
                            DockZone::Right => (MIN_RIGHT_PANEL_WIDTH_PX, MAX_RIGHT_PANEL_WIDTH_PX),
                            DockZone::Bottom => (MIN_BOTTOM_PANEL_HEIGHT_PX, MAX_BOTTOM_PANEL_HEIGHT_PX),
                        };
                        live_extent.set(
                            (f64::from(start_extent) + delta)
                                .round()
                                .clamp(f64::from(minimum), f64::from(maximum)) as u16,
                        );
                    },
                    onmouseup: move |event: MouseEvent| {
                        event.prevent_default();
                        resize_drag.set(None);
                        on_extent_change.call((zone, live_extent()));
                    },
                }
            }
            div {
                class: if resize_drag().is_some() { "dock-resize-handle active" } else { "dock-resize-handle" },
                style: "{handle_style}",
                onmousedown: move |event: MouseEvent| {
                    if event.trigger_button() == Some(MouseButton::Primary) {
                        event.prevent_default();
                        event.stop_propagation();
                        let coordinates = event.client_coordinates();
                        let position = if zone == DockZone::Bottom { coordinates.y } else { coordinates.x };
                        resize_drag.set(Some((position, live_extent())));
                    }
                },
            }
        }
    }
}

#[component]
pub fn DockHiddenDropTargets(layout: DockLayout, drag: Option<DockDragState>) -> Element {
    let dragging = drag.as_ref().is_some_and(|drag| drag.dragging);
    if !dragging {
        return rsx! {};
    }

    rsx! {
        for zone in [DockZone::Left, DockZone::Right, DockZone::Bottom] {
            if !layout.stack(zone).visible || layout.stack(zone).panels.is_empty() {
                {let zone_value = zone_name(zone);
                let panel_count = layout.stack(zone).panels.len();
                let highlighted = drag
                    .as_ref()
                    .and_then(|drag| drag.target)
                    .is_some_and(|target| target.zone == zone);
                let edge_style = match zone {
                    DockZone::Left => "position:fixed;left:0;top:0;bottom:0;width:22px;",
                    DockZone::Right => "position:fixed;right:0;top:0;bottom:0;width:22px;",
                    DockZone::Bottom => "position:fixed;left:0;right:0;bottom:0;height:22px;",
                };
                rsx! {
                    div {
                        key: "hidden-dock-edge-{zone_value}",
                        "data-rusterm-dock-hidden-edge": "{zone_value}",
                        "data-rusterm-dock-panel-count": "{panel_count}",
                        style: if highlighted {
                            format!("{edge_style}z-index:9997;pointer-events:none;background:color-mix(in srgb,var(--skin-accent) 30%,transparent);border:2px solid var(--skin-accent);box-sizing:border-box;")
                        } else {
                            format!("{edge_style}z-index:9997;pointer-events:none;background:color-mix(in srgb,var(--skin-accent) 8%,transparent);")
                        },
                    }
                }}
            }
        }
    }
}

#[component]
pub fn DockDragGhost(drag: Option<DockDragState>) -> Element {
    let Some(drag) = drag.filter(|drag| drag.dragging) else {
        return rsx! {};
    };
    let x = drag.cur_x + 12.0;
    let y = drag.cur_y + 14.0;
    rsx! {
        div {
            style: "position:fixed;left:{x}px;top:{y}px;pointer-events:none;z-index:9999;background:var(--skin-surface);border:1px solid var(--skin-accent);padding:4px 8px;border-radius:4px;font-size:12px;color:var(--skin-text);box-shadow:0 2px 8px rgba(0,0,0,.4);user-select:none;-webkit-user-select:none;",
            "{drag.label}"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_response_parses_visible_and_hidden_targets() {
        assert_eq!(
            parse_dock_drag_poll_response("12.5\u{1f}30\u{1f}0\u{1f}right\u{1f}2"),
            Some((
                12.5,
                30.0,
                false,
                Some(DockDropTarget {
                    zone: DockZone::Right,
                    index: 2,
                }),
            ))
        );
        assert_eq!(
            parse_dock_drag_poll_response("12\u{1f}30\u{1f}1\u{1f}\u{1f}"),
            Some((12.0, 30.0, true, None))
        );
    }

    #[test]
    fn same_zone_forward_move_adjusts_for_removal() {
        let layout = DockLayout::default();
        assert_eq!(
            adjusted_drop_index(
                &layout,
                PanelId::Connections,
                DockDropTarget {
                    zone: DockZone::Left,
                    index: 2,
                },
            ),
            1
        );
    }

    #[test]
    fn cross_zone_move_keeps_raw_insertion_index() {
        let layout = DockLayout::default();
        assert_eq!(
            adjusted_drop_index(
                &layout,
                PanelId::Connections,
                DockDropTarget {
                    zone: DockZone::Right,
                    index: 1,
                },
            ),
            1
        );
    }

    #[test]
    fn install_script_uses_document_capture_without_html5_drag() {
        let script = build_install_dock_drag_script(1.0, 2.0);
        assert!(script.contains("document.addEventListener('mousemove', moveHandler, true)"));
        assert!(script.contains("document.addEventListener('mouseup', upHandler, true)"));
        assert!(!script.contains("dragstart"));
        assert!(!script.contains("ondrop"));
    }
}
