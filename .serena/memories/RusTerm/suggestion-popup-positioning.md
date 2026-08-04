# 终端弹窗定位机制(SuggestionPopup / OneKeyPopup)

## 定位链路(三层,优先级从高到低)
弹窗根 div(`data-rusterm-terminal-popup="true"`)的 CSS:
```
top: var(--suggestion-popup-top, var(--suggestion-top, {fallback_top}));
bottom: var(--suggestion-popup-bottom, auto);
max-height: var(--suggestion-popup-max-height, calc(100% - var(--suggestion-top, {fallback_top})));
```
1. **`--suggestion-popup-top/bottom/max-height`**:由 terminal_view.rs 的 100ms resize future 根据 `popup_layout()`(方向选择)写入。方向策略(commit `e08111f`):**优先下方**——下方能放下整个弹窗、或至少有 `POPUP_MIN_BELOW_PX`(48px≈2行)可用空间就留在下方(内部滚动吃掉溢出);仅当光标贴底且上方更大才翻上方。
2. **`--suggestion-top/bottom`**:同一 resize future 的测量 eval 通过 `el.querySelector('[data-cursor-row="1"]')` 找光标行 DOM 测出,写在 `terminal-input-{sid}` 容器上。
3. **`fallback_top` prop(2026-08 新增,修"弹窗贴顶"bug)**:上面两层变量未设置时的兜底。以前是写死 `2em` → 测量没跑到/失败时弹窗贴在终端顶部(用户截图 bug)。现在 TerminalView 用纯函数 `popup_fallback_top_px(cursor_row)` = `8px(容器上 padding) + (cursor_row+1) × 19px(行高)` 算出提示符正下方的像素值传入,首帧即正确。

## 关键位置
- `terminal_view.rs`:`popup_layout()` + `POPUP_MIN_BELOW_PX` + `popup_fallback_top_px()` + `TERMINAL_ROW_HEIGHT_PX`(19.0)/`TERMINAL_PADDING_TOP_PX`(8.0) 常量;resize future ~L1837-1913(测量 eval + layout eval);挂载点 ~L2780+。
- `suggestion_popup.rs` / `onekey_popup.rs`:`fallback_top: String` prop(`#[props(default = "2em".to_string())]`),两组件共用同一套变量,改动必须保持一致。
- OneKey 提交反馈小标签(terminal_view.rs)也用 `top:var(--suggestion-top, {popup_fallback_top})`。

## 陷阱(勿重蹈)
- CSS 变量必须由 100ms 轮询 future 维护——`use_effect` 只在 mount 跑一次(version 是普通 prop 非 Signal),会留下过期值。
- 行高实测 **19.00px**(13px × 1.5,WebKit);容器 padding `8px 12px 4px 4px`。改字号/padding 时同步改 `TERMINAL_ROW_HEIGHT_PX`/`TERMINAL_PADDING_TOP_PX`。
- `data-cursor-row="1"` 仅当 `row_idx == cursor_row && cursor_visible` 时输出;滚回历史(scroll_offset>0)时 cursor_visible=false → 测量跳过,变量保持旧值,兜底 prop 用 cursor_row=0(可接受:此时弹窗一般不显示)。
- 测量 eval 需顶层 `return`(dioxus desktop AsyncFunction 语义,见 `mem:RusTerm/mouse-selection-windterm-issue-39`)。

## 测试
`terminal_view.rs::tests`:`popup_fallback_top_sits_below_the_cursor_row`、`popup_stays_below_with_partial_space_and_scrolls_internally`、`popup_flips_above_only_when_bottom_edge_leaves_no_usable_space` 等 6 个 popup 定位纯函数测试。
