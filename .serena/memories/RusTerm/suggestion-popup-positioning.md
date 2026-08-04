# 终端弹窗定位机制(SuggestionPopup / OneKeyPopup)

## 定位链路(三层,优先级从高到低)
弹窗根 div(`data-rusterm-terminal-popup="true"`)的 CSS:
```
top: var(--suggestion-popup-top, var(--suggestion-top, {fallback_top}));
bottom: var(--suggestion-popup-bottom, auto);
max-height: var(--suggestion-popup-max-height, calc(100% - var(--suggestion-top, {fallback_top})));
```
1. **`--suggestion-popup-top/bottom/max-height`**:由 terminal_view.rs 的 100ms resize future 写入。三种模式:
   - **拖动中**(`popup_drag_active` Signal 为 true):resize future 本 tick 跳过写入,位置由拖动 JS 直接维护。
   - **手动偏移**(`SUGGESTION_POPUP_OFFSET_Y` GlobalSignal ≠ 0):`build_manual_popup_layout_script` 在 JS 里算 `top = clamp(anchor + offset, 0, H-24)`(anchor = `--suggestion-top`),max-height = 剩余空间;忽略自动上/下翻转。
   - **自动**(offset == 0):`popup_layout()` 方向选择(优先下方,`POPUP_MIN_BELOW_PX=48`)。
2. **`--suggestion-top/bottom`**:测量 eval 通过 `[data-cursor-row="1"]` 找光标行,写在 `terminal-input-{sid}` 容器上。
3. **`fallback_top` prop**:`popup_fallback_top_px(cursor_row)` = `8 + (row+1)×19` px 兜底(首帧)。

## 拖动弹窗 + 记录用户习惯(2026-08, Task 5)
- 两个弹窗顶部有 **拖动把手条**("•••", 12px 高, cursor:grab;OneKeyPopup 的把手 `margin-right:26px` 避开右上 × 按钮)。props:`on_drag_start: EventHandler<f64>`(clientY)+ `on_position_reset: EventHandler<()>`(双击复位),均 `#[props(default)]`。
- 拖动实现 = 惯用 document-capture 模式:`build_install_popup_drag_script(container_id, start_y)` 安装 document 级 capture mousemove/mouseup/keydown(Esc)/blur 监听器,**唯一全局** `__rusterm_popup_drag_state`('active' / 'cancel' / 'done:<offset>')+ `_rusterm_popup_drag_remove`。移动时 JS 直接写 `--suggestion-popup-*`(平滑,Rust 不在环内);<3px 移动视为未动 → 'cancel'(保证双击复位安全)。
- TerminalView 的 `_popup_drag_future`(空闲 120ms / 拖动中 30ms 轮询 `POPUP_DRAG_POLL_JS`)在 done 时:`parse_popup_drag_poll_response`(clamp ±5000,|offset|<4 snap 到 0=自动)→ 写 `SUGGESTION_POPUP_OFFSET_Y` → `on_popup_offset_commit.call(offset)` → `POPUP_DRAG_CLEANUP_JS`。
- **偏移语义**:相对光标锚点(`--suggestion-top`)的垂直偏移,弹窗仍跟随提示符行;0.0 = 全自动(含上下翻转)。仅支持垂直拖动(弹窗是 left:0;right:0 全宽设计)。
- **持久化**:`PersistedConfig.suggestion_popup_offset_y: f64`(`#[serde(default)]`,settings.json)+ `ConfigManager::load/save_suggestion_popup_offset`(`normalize_suggestion_popup_offset` 公开纯函数:非有限→0,clamp ±5000)。app.rs 启动时(~L16640)load 进 GlobalSignal;`render_terminal_pane` 的 TerminalView 挂载处 wire `on_popup_offset_commit` → save。习惯是全局的(跨 session 共享)。
- i18n key:`popup.drag_grip_tooltip`。

## 关键位置
- `terminal_view.rs`:`popup_layout()`、`popup_fallback_top_px()`、`SUGGESTION_POPUP_OFFSET_Y`(pub GlobalSignal)、`POPUP_MANUAL_MIN_VISIBLE_PX=24`、`POPUP_OFFSET_SNAP_PX=4`、`build_install_popup_drag_script` / `build_manual_popup_layout_script` / `parse_popup_drag_poll_response` / `PopupDragPoll`;resize future(测量 + 三模式 layout)+ 拖动轮询 future;挂载点传 `on_drag_start`/`on_position_reset`。
- `suggestion_popup.rs` / `onekey_popup.rs`:把手 UI + 新 props,两组件必须保持一致。
- 注意:向 `PersistedConfig` 加字段要改 config_manager.rs 里 **14 处**全构造点 + config.rs roundtrip 测试。

## 陷阱(勿重蹈)
- CSS 变量必须由 100ms 轮询 future 维护——`use_effect` 只在 mount 跑一次。
- 行高实测 19.00px;容器 padding `8px 12px 4px 4px`。
- `data-cursor-row="1"` 仅当光标可见时输出;滚回历史时测量跳过。
- 测量/轮询 eval 需顶层 `return`。
- 拖动把手的 onmousedown 必须 prevent_default+stop_propagation(与弹窗根的焦点保护 handler 共存);把手 dblclick 依赖"未移动即 cancel"保护,否则单纯点击会把当前位置误存为偏移。

## 测试
`terminal_view.rs::tests`:6 个 popup 定位纯函数测试 + `popup_drag_script_uses_document_capture_listeners_and_unique_globals`、`popup_drag_poll_parsing_covers_pending_cancel_and_done`、`popup_drag_offsets_snap_to_automatic_and_clamp_extremes`、`manual_popup_layout_script_applies_offset_on_top_of_the_anchor`。
`config_manager.rs::tests`:`suggestion_popup_offset_defaults_roundtrips_and_survives_other_saves`、`suggestion_popup_offset_normalization_rejects_garbage`。
基线:rusterm-ui lib **727**,rusterm-core lib **193**(2026-08 Task 5 后)。
