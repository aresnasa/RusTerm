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
    pub label: String,
    pub start_x: f64,
    pub start_y: f64,
    pub cur_x: f64,
    pub cur_y: f64,
    pub dragging: bool,
    pub target: Option<DockDropTarget>,
}

pub fn panel_label(panel: PanelId) -> String {
    match panel {
        PanelId::Connections => crate::i18n::t("connections.title"),
        PanelId::RemoteFiles => crate::i18n::t("remote_files.title"),
        PanelId::Sessions => crate::i18n::t("sessions.title"),
        PanelId::History => crate::i18n::t("history.title"),
        PanelId::Send => crate::i18n::t("send.tab_title"),
        PanelId::EmbeddedShell => crate::i18n::t("shell.tab_title"),
        PanelId::Transfers => crate::i18n::t("transfers.tab_title"),
        PanelId::Relay => crate::i18n::t("api.tab_title"),
    }
}

fn hide_dock_label(zone: DockZone) -> String {
    match zone {
        DockZone::Left => crate::i18n::t("dock.hide_left"),
        DockZone::Right => crate::i18n::t("dock.hide_right"),
        DockZone::Bottom => crate::i18n::t("dock.hide_bottom"),
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
            var rails = Array.from(document.querySelectorAll('[data-rusterm-dock-hidden-edge]'));\n\
            for (var railIndex = 0; railIndex < rails.length; railIndex++) {\n\
                var rail = rails[railIndex];\n\
                var railRect = rail.getBoundingClientRect();\n\
                if (x < railRect.left || x >= railRect.right || y < railRect.top || y >= railRect.bottom) continue;\n\
                targetZone = rail.getAttribute('data-rusterm-dock-hidden-edge') || '';\n\
                targetIndex = rail.getAttribute('data-rusterm-dock-panel-count') || '0';\n\
                break;\n\
            }\n\
            if (!targetZone) {\n\
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
            }\n\
            return [coords[0], coords[1], window.__rusterm_dock_drag_done ? '1' : '0', targetZone, targetIndex].join('\\u001f');\n\
        })()",
    )
    .await
    .ok()?;
    parse_dock_drag_poll_response(result.as_str()?)
}

fn build_install_dock_resize_script(
    initial_position: f64,
    coordinate: &str,
    cursor: &str,
    token: &str,
) -> String {
    format!(
        "(function() {{\n\
            if (window._rusterm_dock_resize_remove) {{ window._rusterm_dock_resize_remove(); window._rusterm_dock_resize_remove = null; }}\n\
            window.__rusterm_dock_resize_pos = '{initial_position}';\n\
            window.__rusterm_dock_resize_done = false;\n\
            window.__rusterm_dock_resize_token = '{token}';\n\
            var previousCursor = document.body.style.cursor;\n\
            var previousWebkitUserSelect = document.body.style.webkitUserSelect;\n\
            var previousUserSelect = document.body.style.userSelect;\n\
            document.body.style.cursor = '{cursor}';\n\
            document.body.style.webkitUserSelect = 'none';\n\
            document.body.style.userSelect = 'none';\n\
            if (window.getSelection) {{ window.getSelection().removeAllRanges(); }}\n\
            var removeListeners = function() {{\n\
                document.removeEventListener('mousemove', moveHandler, true);\n\
                document.removeEventListener('mouseup', finishHandler, true);\n\
                document.removeEventListener('pointerup', finishHandler, true);\n\
                document.removeEventListener('pointercancel', finishHandler, true);\n\
                window.removeEventListener('blur', finishHandler, true);\n\
                document.removeEventListener('visibilitychange', visibilityHandler, true);\n\
                document.removeEventListener('keydown', keyHandler, true);\n\
                document.body.style.cursor = previousCursor;\n\
                document.body.style.webkitUserSelect = previousWebkitUserSelect;\n\
                document.body.style.userSelect = previousUserSelect;\n\
            }};\n\
            var finishHandler = function(e) {{\n\
                if (e && Number.isFinite(e.{coordinate})) {{ window.__rusterm_dock_resize_pos = String(e.{coordinate}); }}\n\
                window.__rusterm_dock_resize_done = true;\n\
                if (e && e.cancelable) {{ e.preventDefault(); }}\n\
                var remove = window._rusterm_dock_resize_remove;\n\
                window._rusterm_dock_resize_remove = null;\n\
                if (remove) {{ remove(); }}\n\
            }};\n\
            var moveHandler = function(e) {{\n\
                if (typeof e.buttons === 'number' && e.buttons === 0) {{ finishHandler(e); return; }}\n\
                window.__rusterm_dock_resize_pos = String(e.{coordinate});\n\
                if (e.cancelable) {{ e.preventDefault(); }}\n\
            }};\n\
            var visibilityHandler = function() {{ if (document.hidden) {{ finishHandler(null); }} }};\n\
            var keyHandler = function(e) {{ if (e.key === 'Escape') {{ finishHandler(e); }} }};\n\
            window._rusterm_dock_resize_remove = removeListeners;\n\
            document.addEventListener('mousemove', moveHandler, true);\n\
            document.addEventListener('mouseup', finishHandler, true);\n\
            document.addEventListener('pointerup', finishHandler, true);\n\
            document.addEventListener('pointercancel', finishHandler, true);\n\
            window.addEventListener('blur', finishHandler, true);\n\
            document.addEventListener('visibilitychange', visibilityHandler, true);\n\
            document.addEventListener('keydown', keyHandler, true);\n\
        }})()"
    )
}

fn install_dock_resize_listeners(initial_position: f64, zone: DockZone, generation: u64) {
    let coordinate = if zone == DockZone::Bottom {
        "clientY"
    } else {
        "clientX"
    };
    let cursor = if zone == DockZone::Bottom {
        "row-resize"
    } else {
        "col-resize"
    };
    let token = format!("{}:{generation}", zone_name(zone));
    let script = build_install_dock_resize_script(initial_position, coordinate, cursor, &token);
    spawn(async move {
        let _ = dioxus::document::eval(&script).await;
    });
}

async fn poll_dock_resize_state() -> Option<(f64, bool, String)> {
    let result = dioxus::document::eval(
        "return (function() {\n\
            var position = Number(window.__rusterm_dock_resize_pos);\n\
            var token = window.__rusterm_dock_resize_token || '';\n\
            if (!Number.isFinite(position) || !token) return '';\n\
            return [String(position), window.__rusterm_dock_resize_done ? '1' : '0', token].join('\\u001f');\n\
        })()",
    )
    .await
    .ok()?;
    parse_dock_resize_poll_response(result.as_str()?)
}

fn parse_dock_resize_poll_response(response: &str) -> Option<(f64, bool, String)> {
    let mut fields = response.split('\u{1f}');
    let position = fields.next()?.parse::<f64>().ok()?;
    if !position.is_finite() {
        return None;
    }
    let done = match fields.next()? {
        "0" => false,
        "1" => true,
        _ => return None,
    };
    let token = fields.next()?;
    if token.is_empty() || fields.next().is_some() {
        return None;
    }
    Some((position, done, token.to_owned()))
}

fn resized_dock_extent(
    zone: DockZone,
    start_position: f64,
    start_extent: u16,
    position: f64,
) -> u16 {
    let delta = match zone {
        DockZone::Left => position - start_position,
        DockZone::Right | DockZone::Bottom => start_position - position,
    };
    let (minimum, maximum) = match zone {
        DockZone::Left => (MIN_SIDEBAR_WIDTH_PX, MAX_SIDEBAR_WIDTH_PX),
        DockZone::Right => (MIN_RIGHT_PANEL_WIDTH_PX, MAX_RIGHT_PANEL_WIDTH_PX),
        DockZone::Bottom => (MIN_BOTTOM_PANEL_HEIGHT_PX, MAX_BOTTOM_PANEL_HEIGHT_PX),
    };
    (f64::from(start_extent) + delta)
        .round()
        .clamp(f64::from(minimum), f64::from(maximum)) as u16
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
    let _lang = crate::i18n::LANGUAGE();
    if !stack.visible || stack.panels.is_empty() {
        return rsx! {};
    }

    let incoming_extent = stack.extent_px;
    let mut live_extent = use_signal(|| incoming_extent);
    let mut resize_drag = use_signal(|| Option::<(f64, u16, u64)>::None);
    let mut resize_generation = use_signal(|| 0_u64);
    use_effect(move || {
        if resize_drag.peek().is_none() && *live_extent.peek() != incoming_extent {
            live_extent.set(incoming_extent);
        }
    });
    let _resize_drag_poll = use_future(move || async move {
        loop {
            let Some((start_position, start_extent, generation)) = resize_drag() else {
                tokio::time::sleep(std::time::Duration::from_millis(32)).await;
                continue;
            };
            if let Some((position, done, token)) = poll_dock_resize_state().await {
                let expected_token = format!("{}:{generation}", zone_name(zone));
                if token == expected_token {
                    let extent = resized_dock_extent(zone, start_position, start_extent, position);
                    if live_extent() != extent {
                        live_extent.set(extent);
                    }
                    if done {
                        resize_drag.set(None);
                        on_extent_change.call((zone, extent));
                        continue;
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(16)).await;
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
    // Resting handle uses a semi-transparent accent-tinted fill so the grip is
    // visibly clickable even before hover. The grip bar (a short rounded
    // rectangle centered on the handle) gives an explicit "drag here" affordance.
    let handle_style = match zone {
        DockZone::Left => {
            "position:absolute;right:-4px;top:0;width:8px;height:100%;z-index:80;cursor:col-resize;background:color-mix(in srgb,var(--skin-border-strong) 60%,transparent);transition:background 0.12s ease,box-shadow 0.12s ease;"
        }
        DockZone::Right => {
            "position:absolute;left:-4px;top:0;width:8px;height:100%;z-index:80;cursor:col-resize;background:color-mix(in srgb,var(--skin-border-strong) 60%,transparent);transition:background 0.12s ease,box-shadow 0.12s ease;"
        }
        DockZone::Bottom => {
            "position:absolute;left:0;top:-4px;width:100%;height:8px;z-index:80;cursor:row-resize;background:color-mix(in srgb,var(--skin-border-strong) 60%,transparent);transition:background 0.12s ease,box-shadow 0.12s ease;"
        }
    };
    // Grip bar: a short rounded bar centered on the handle. Vertical handles
    // (Left/Right) get a tall narrow bar; the Bottom handle gets a wide flat
    // bar. The bar contrasts with the handle fill so the affordance reads at a
    // glance and inverts to bg-colored on hover/active.
    let (grip_class, grip_style) = if zone == DockZone::Bottom {
        (
            "dock-resize-grip dock-resize-grip-h",
            "position:absolute;top:50%;left:50%;transform:translate(-50%,-50%);width:22px;height:2px;border-radius:1px;background:var(--skin-text-muted);opacity:0.7;transition:opacity 0.12s ease,background 0.12s ease;pointer-events:none;",
        )
    } else {
        (
            "dock-resize-grip dock-resize-grip-v",
            "position:absolute;top:50%;left:50%;transform:translate(-50%,-50%);width:2px;height:22px;border-radius:1px;background:var(--skin-text-muted);opacity:0.7;transition:opacity 0.12s ease,background 0.12s ease;pointer-events:none;",
        )
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
            .dock-tab {{ border:0;background:transparent;color:var(--skin-text-muted);padding:7px 9px;font-size:11px;cursor:grab;border-bottom:2px solid transparent;white-space:nowrap;user-select:none;-webkit-user-select:none; }}
            .dock-tab:hover {{ color:var(--skin-text);background:var(--skin-surface-hover); }}
            .dock-tab.active {{ color:var(--skin-accent);border-bottom-color:var(--skin-accent);cursor:grab; }}
            .dock-tab:active {{ cursor:grabbing; }}
            .dock-close {{ margin-left:auto;margin-right:5px;border:0;background:transparent;color:var(--skin-text-muted);cursor:pointer;padding:4px 7px;font-size:14px; }}
            .dock-close:hover {{ color:var(--skin-text);background:var(--skin-surface-hover); }}
            .dock-insertion {{ width:2px;align-self:stretch;flex:0 0 2px;background:var(--skin-accent);box-shadow:0 0 6px color-mix(in srgb,var(--skin-accent) 75%,transparent); }}
            .dock-resize-handle {{ transition: background 0.12s ease, box-shadow 0.12s ease; }}
            .dock-resize-handle:hover,.dock-resize-handle.active {{ background:var(--skin-accent);box-shadow:0 0 6px color-mix(in srgb,var(--skin-accent) 55%,transparent); }}
            .dock-resize-handle:hover .dock-resize-grip,.dock-resize-handle.active .dock-resize-grip {{ background:var(--skin-bg);opacity:1; }}
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
                        title: crate::i18n::tf("dock.drag_panel", &[("panel", &panel_label(panel))]),
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
                    title: hide_dock_label(zone),
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
                    "data-rusterm-dock-resize-overlay": "true",
                    style: "position:fixed;inset:0;z-index:79;cursor:{resize_cursor};background:transparent;",
                    onmousemove: move |event: MouseEvent| {
                        let Some((start_position, start_extent, _)) = resize_drag() else { return; };
                        event.prevent_default();
                        let coordinates = event.client_coordinates();
                        let position = if zone == DockZone::Bottom {
                            coordinates.y
                        } else {
                            coordinates.x
                        };
                        live_extent.set(resized_dock_extent(
                            zone,
                            start_position,
                            start_extent,
                            position,
                        ));
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
                        let generation = resize_generation().wrapping_add(1);
                        resize_generation.set(generation);
                        resize_drag.set(Some((position, live_extent(), generation)));
                        install_dock_resize_listeners(position, zone, generation);
                    }
                },
                span {
                    class: "{grip_class}",
                    style: "{grip_style}",
                }
            }
        }
    }
}

#[component]
pub fn DockHiddenDropTargets(layout: DockLayout, drag: Option<DockDragState>) -> Element {
    let _lang = crate::i18n::LANGUAGE();
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
    let _lang = crate::i18n::LANGUAGE();
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
    fn dock_resize_poll_response_requires_a_finite_position_and_token() {
        assert_eq!(
            parse_dock_resize_poll_response("125.5\u{1f}1\u{1f}bottom:2"),
            Some((125.5, true, "bottom:2".to_owned()))
        );
        assert_eq!(
            parse_dock_resize_poll_response("NaN\u{1f}0\u{1f}bottom:2"),
            None
        );
        assert_eq!(parse_dock_resize_poll_response("125\u{1f}0\u{1f}"), None);
    }

    #[test]
    fn dock_resize_script_captures_and_cleans_up_every_end_signal() {
        let script = build_install_dock_resize_script(200.0, "clientY", "row-resize", "bottom:3");
        for listener in [
            "document.addEventListener('mouseup', finishHandler, true)",
            "document.addEventListener('pointerup', finishHandler, true)",
            "document.addEventListener('pointercancel', finishHandler, true)",
            "window.addEventListener('blur', finishHandler, true)",
            "document.addEventListener('visibilitychange', visibilityHandler, true)",
            "document.addEventListener('keydown', keyHandler, true)",
        ] {
            assert!(script.contains(listener), "missing listener: {listener}");
        }
        for listener in [
            "document.removeEventListener('mouseup', finishHandler, true)",
            "document.removeEventListener('pointerup', finishHandler, true)",
            "document.removeEventListener('pointercancel', finishHandler, true)",
            "window.removeEventListener('blur', finishHandler, true)",
            "document.removeEventListener('visibilitychange', visibilityHandler, true)",
            "document.removeEventListener('keydown', keyHandler, true)",
        ] {
            assert!(script.contains(listener), "missing cleanup: {listener}");
        }
        assert!(script.contains("e.buttons === 0"));
        assert!(script.contains("document.hidden"));
        assert!(script.contains("e.key === 'Escape'"));
        assert!(script.contains("window._rusterm_dock_resize_remove = null"));
        assert!(script.contains("document.body.style.cursor = previousCursor"));
        assert!(script.contains("document.body.style.userSelect = previousUserSelect"));
    }

    #[test]
    fn dock_resize_extent_follows_each_zone_direction() {
        assert_eq!(resized_dock_extent(DockZone::Left, 100.0, 250, 140.0), 290);
        assert_eq!(resized_dock_extent(DockZone::Right, 100.0, 250, 60.0), 290);
        assert_eq!(resized_dock_extent(DockZone::Bottom, 100.0, 250, 60.0), 290);
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
