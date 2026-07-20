# RusTerm — Split-Pane Refactor + Title Bar Polish + Sidebar Drag-Split (2026-07-20)

## Scope (this pass)

User-reported issues addressed:
1. **"方案 b，展示为空时需要复制当前焦点的会话"** — already implemented (prior session); re-verified contract.
2. **"如果是从左侧会话栏拖拽则是需要新开会话来对比"** — sidebar drag now SPLITS the layout (preserving existing session) instead of REPLACING the target pane's session. New session lands alongside the existing one for side-by-side comparison.
3. **"control+w 不能关闭窗口，快捷健需要调整，标准的 linux 终端快捷键在会话中需要能正常执行的"** — added Linux/Windows plain Ctrl+W (no Shift) close-pane handler in App's `onkeydown`. This handler ONLY fires when no terminal has focus (TerminalView intercepts Ctrl+W and sends `0x17` to PTY when a terminal is focused). Resolves "Ctrl+W can't close window" without breaking the standard Linux terminal shortcut inside a session.
4. **"无法通过拖动左边的会话到2分屏窗口，不符合期望，需要修复"** — fixed via the sidebar-drag-split refactor above. Previously the drag would REPLACE the target pane's session; now it preserves the existing session by growing the layout.
5. **"继续优化会话顶部的框，增加高亮颜色，支持拖拽"** + **"当前颜色是暗色的，支持高亮会话的小框"** — brightened title bar palette further (default `#2a2e42` → `#2f3550`; focused flat `#3b4261` → gradient `#565f89 → #414868` with glow; drag-over `#414868` → bright blue `#7aa2f7`). Accent strip widened 3px → 4px + glow shadow.
6. **"compare模式的颜色需要改进，现在颜色会导致按钮按了后会导致字体被颜色覆盖"** — compare button redesigned with icon `⇄` + darker text color `#0f1119` + text-shadow for extra contrast. ON state has `box-shadow: 0 0 6px rgba(122,162,247,0.45)` glow + transparent-border trick to prevent 2px layout shift.
7. **"改造分屏这个逻辑，支持分屏提示"** — empty pane now shows a 3-line hint with `⊡` icon, "空白窗格" header, and three call-to-action lines.
8. **"一个新的会话支持多分屏"** — new "⊕ Split" toolbar button grows the layout to the next preset (Single → Split2H → Grid4 → Grid8).
9. **"可以多个会话放到多个分屏中"** — new "⇶ Distribute" toolbar button fills all panes with open sessions in tab order.
10. **"继续优化相关逻辑"** — sidebar drag-split + distribute function + split button + empty-pane hint collectively address the multi-pane workflow gaps.

## Changes

### 1. `state.rs` — new functions

**`prepare_split_for_sidebar_drop(state, target_pane_idx) -> Option<SidebarDropPlan>`** (L1004-1104):
The core of the sidebar-drag-split refactor. Returns the pane index where the new sidebar connection should land:
- Case 1: no layout → create Split2H, preserve pane 0's anchor session, return pane_idx=1.
- Case 2: target pane empty → return target_pane_idx (no layout change).
- Case 3: target occupied, another empty pane exists → return first empty pane's index.
- Case 4: target occupied, no empty panes, can grow → grow to next preset, clear the first newly-created pane (so the sidebar connection can take it without displacing an existing session), return that pane's index.
- Case 5: at Grid8 with all panes occupied → fall back to replacing target pane (preserves old "sidebar drop = replace" at the cap).
- Returns `None` if there's no active tab.

The `SidebarDropPlan` struct carries `layout_owner_tab_id` (NOT necessarily `active_tab` — `apply_layout_preset` may have mutated things) so the caller's `PaneTarget` is keyed correctly.

**`distribute_sessions_across_panes(state) -> usize`** (L1106-1156):
One-click "fill all panes with open sessions". Collects sessions in tab order (active anchor first, deduplicated), assigns them to panes in row-major order. Extra sessions beyond pane count are dropped (remain in `state.sessions`); extra panes are emptied. Returns the number of panes actually assigned.

### 2. `app.rs` — `finish_tab_drag` Connection branch rewritten

The `DragKind::Connection(conn)` arm of `finish_tab_drag` now calls `prepare_split_for_sidebar_drop` first to determine the target pane index (which may differ from the hit-test `target_idx` if we reused an empty pane or grew the preset), then calls `open_connection` with a `PaneTarget` keyed by the plan's `layout_owner_tab_id`.

This is the key behavioral change: dragging a sidebar connection into an OCCUPIED pane no longer replaces the existing session. Instead, the layout grows (or reuses an empty pane), preserving the existing session for comparison.

### 3. `app.rs` — title bar visual overhaul (pass 2)

**`title_chrome` palette brightened again**:
- default: `#2a2e42` → `#2f3550` (two steps lighter than terminal bg #1a1b26)
- focused: flat `#3b4261` → `linear-gradient(180deg, #565f89 0%, #414868 100%)` + `box-shadow: 0 1px 6px rgba(122,162,247,0.25)` (Tokyo Night "selection" tone with glow — the "高亮会话的小框" the user asked for)
- drag-over: `#414868` → bright `#7aa2f7` (unambiguous drop target)

**Accent strip**: 3px → 4px wide, added `box-shadow: 0 0 4px {accent_color}` glow, added `class: "pane-accent-strip"` for the new `.pane-accent-strip { transition: width 0.12s ease; }` rule.

**Hover brightness**: `1.08` → `1.10` (slightly stronger).

### 4. `app.rs` — compare button redesign (pass 2)

Replaced the prior class system with a more robust design:
- Added `⇄` icon (`<span class="compare-btn-icon">⇄</span>`) before the "Compare" text.
- ON state: `color: #0f1119` (was `#1a1b26`) — darker for better contrast against `#7aa2f7`.
- ON state: `font-weight: 700` (was 600).
- ON state: `text-shadow: 0 1px 0 rgba(255,255,255,0.25)` — extra crispness.
- ON state: `box-shadow: 0 0 6px rgba(122,162,247,0.45)` — glow.
- ON state: `border-color: #89b5fa` (was `#7aa2f7`) — slightly lighter for definition.
- Padding: `2px 8px` → `3px 10px`; line-height: `16px` → `18px`; border-radius: `3px` → `4px`.
- New `.compare-btn-icon` class for the icon span.

### 5. `app.rs` — empty pane hint redesign

The empty pane rendering in `multi_pane_container` (~L3059-3083) was a single dashed box with one line of text. Redesigned to a 3-section hint:
- `⊡` icon (22px, dim `#414868`)
- "空白窗格" header (blue `#7aa2f7`, 12px, bold)
- 3-line call-to-action (dim `#565f89`, 11px, line-height 1.5):
  - "点击标题栏 ⧉ 复制焦点会话"
  - "拖动左侧会话到此处新建会话"
  - "或拖动标签页/会话标题到此处"
- Background: `linear-gradient(180deg, #16161e 0%, #1a1b26 100%)` for depth.

### 6. `app.rs` — empty_pane_title_actions buttons refined

The `⧉` (copy) and `+` (sidebar) buttons in the empty pane's title bar actions:
- min-width: `22px` → `24px`, padding: `0 5px` → `0 6px`.
- Added `transition: background 0.12s ease, color 0.12s ease` for hover smoothness (hover state itself comes from CSS classes on the parent, if any).

### 7. `app.rs` — Ctrl+W behavior (Linux/Windows, outside terminal)

New handler block in App's `onkeydown`:
```rust
if cfg!(not(target_os = "macos"))
    && mods.ctrl()
    && !mods.meta()
    && !mods.alt()
    && !mods.shift()
{
    let snapshot = state.read();
    let target = focused_pane_session(&snapshot)
        .or_else(|| snapshot.active_session.clone());
    drop(snapshot);
    if let Some(session_id) = target {
        e.prevent_default();
        e.stop_propagation();
        close_session(&mut state.write(), &mut input_senders.write(), &session_id);
        restore_focus_to_active_session(state, 50);
    }
}
```

Key insight: this handler ONLY fires when no terminal has focus, because the TerminalView's `onkeydown` calls `e.prevent_default()` and consumes Ctrl+W (sending `0x17` to the PTY) before the event can bubble. So plain Ctrl+W remains the standard Linux terminal shortcut inside a session, AND works as a close-pane shortcut when the user has clicked away from a terminal (e.g., onto the sidebar or empty chrome).

macOS keeps Cmd+W as the platform convention (already implemented in the prior pass).

### 8. `app.rs` — toolbar buttons: "⊕ Split" and "⇶ Distribute"

Added two new buttons in the status bar toolbar (after the "Layout: N" button):

**"⊕ Split"** (green `#9ece6a`, bordered):
- Grows the active tab's layout to the next preset (Single → Split2H → Grid4 → Grid8).
- New panes start EMPTY; user fills them via empty-pane hint buttons or drag-drop.
- No-op at Grid8 (max).

**"⇶ Distribute"** (purple `#bb9af7`, bordered):
- One-click "fill all panes with open sessions".
- Calls `distribute_sessions_across_panes`.
- Sessions beyond pane count remain in `state.sessions` for manual placement.

### 9. `app.rs` — imports updated

Added imports:
- `crate::layout::LayoutPreset` (was using full path `crate::layout::LayoutPreset::*`).
- `crate::state::apply_layout_preset` (for the Split button).
- `crate::state::distribute_sessions_across_panes` (for the Distribute button).
- `crate::state::prepare_split_for_sidebar_drop` (for the Connection drag branch).

`layout_label` signature simplified: `preset: crate::layout::LayoutPreset` → `preset: LayoutPreset`.

## Tests

- **12 new tests** in `state.rs` `mod tests`:
  - `sidebar_drop_creates_split_when_no_layout_exists` — case 1.
  - `sidebar_drop_uses_empty_target_pane_without_splitting` — case 2.
  - `sidebar_drop_reuses_other_empty_pane_when_target_occupied` — case 3.
  - `sidebar_drop_grows_layout_when_all_panes_occupied` — case 4 (Split2H → Grid4).
  - `sidebar_drop_falls_back_to_replace_at_grid8` — case 5.
  - `sidebar_drop_returns_none_without_active_tab` — None case.
  - `sidebar_drop_preserves_anchor_session_in_pane_0` — comparison contract.
  - `distribute_fills_panes_in_tab_order` — basic distribute.
  - `distribute_drops_extra_sessions_beyond_pane_count` — overflow.
  - `distribute_empties_extra_panes_when_fewer_sessions` — underflow.
  - `distribute_returns_zero_without_active_tab` — None case.
  - `distribute_returns_zero_without_layout` — no layout case.
- **Total**: 304 in `rusterm-ui` (was 292), 499 workspace-wide (was 487).
- All passing: `cargo test -p rusterm-ui --lib` and `cargo test --workspace`.
- `cargo clippy -p rusterm-ui --lib`: no new warnings from the new code (100 baseline → 100 after my changes; the 102 count includes 2 pre-existing `doc list item without indentation` warnings in `close_session`'s doc comment, unrelated to my work).
- `rustup run nightly rustfmt --edition 2024` applied to `app.rs` and `state.rs`.
- `git diff --check HEAD`: clean.

## Files touched

- `crates/rusterm-ui/src/state.rs`:
  - New `SidebarDropPlan` struct + `prepare_split_for_sidebar_drop` function (~100 lines).
  - New `distribute_sessions_across_panes` function (~50 lines).
  - 12 new tests in `mod tests`.
- `crates/rusterm-ui/src/app.rs`:
  - `finish_tab_drag` Connection branch rewritten to call `prepare_split_for_sidebar_drop`.
  - Title bar palette brightened (3 states).
  - Accent strip widened 3px → 4px + glow.
  - Compare button redesigned (icon + darker text + glow).
  - Empty pane hint redesigned (3-section hint with icon + 3-line CTA).
  - Empty pane title bar buttons refined (padding + transition).
  - New "⊕ Split" toolbar button.
  - New "⇶ Distribute" toolbar button.
  - Ctrl+W (Linux/Windows, outside terminal) close-pane handler.
  - Imports updated (added `LayoutPreset`, `apply_layout_preset`, `distribute_sessions_across_panes`, `prepare_split_for_sidebar_drop`).
  - `layout_label` simplified to use imported `LayoutPreset`.

## Pitfalls / gotchas

- **`prepare_split_for_sidebar_drop` mutates `state.layout_preset`**: when growing from no-layout to Split2H, it sets `state.layout_preset = LayoutPreset::Split2H`. This is necessary so the toolbar's "Layout: N" label updates correctly.
- **`SidebarDropPlan.layout_owner_tab_id` ≠ `state.active_tab` after `apply_layout_preset`**: the `active_tab` shouldn't change (it's the tab anchor), but using the plan's owner is the safe pattern in case of any future mutation.
- **`prepare_split_for_sidebar_drop` clears the first new pane after `apply_layout_preset`**: `apply_layout_preset` may have filled it with a background tab; we clear it so the new sidebar connection can take the slot without displacing an existing session. This is the right behavior for the "sidebar drop = new independent session for comparison" contract.
- **Ctrl+W (Linux/Windows) only fires outside terminals**: the TerminalView intercepts Ctrl+W and sends `0x17` to the PTY when a terminal is focused. The App's `onkeydown` only sees Ctrl+W when no terminal has focus (e.g., sidebar, search box, or main div itself). This is the intended design — plain Ctrl+W remains the standard Linux terminal shortcut inside a session.
- **`distribute_sessions_across_panes` borrow order**: must collect `session_ids` BEFORE the `state.layouts.get_mut` borrow, because `active_tab_anchor_session` reads `state.sessions`/`state.active_tab` immutably. The Rust borrow checker enforces this.
- **Compare button icon span needs `class: "compare-btn-icon"`**: not just for the CSS rule (font-size: 13px), but also so the icon doesn't inherit the parent's `font-weight: 700` (which would make the icon too thick on some fonts).
- **Empty pane hint uses `br {}`**: dioxus 0.7 supports `<br />` as a self-closing element. We use `<br />` between the CTA lines for line breaks within the same div.

## On-demand pane splitting refactor (2026-07-20, follow-up pass)

The user explicitly said: "这里的窗口逻辑不要用 2/4/8 这种，而是按需开启窗口...客户如果只在 1 个窗口中拖拽会话则按需的新增一个会话，不需要一次性增加 2/4/8 这种的，修复相关逻辑". The prior pass's toolbar "⊕ Split" button and sidebar-drag Case 4 used `apply_layout_preset(next)`, which jumps presets and adds 2-4 panes at once (Split2H → Grid4 = +2 panes, Grid4 → Grid8 = +4 panes). This was wrong: the user wants +1 pane per operation.

### Changes

**`crates/rusterm-ui/src/layout.rs`** — new `PaneLayout::append_pane(horizontal: bool) -> Option<usize>` method (added after `set_pane_session`):
- Adds exactly ONE pane to 1D layouts (1×N strips grow a column when `horizontal=true`; N×1 strips grow a row when `horizontal=false`).
- For 2D grids (Grid4/Grid8), adds a full row/col (multiple panes) — acceptable limitation since the user doesn't use 2D grids in the on-demand workflow.
- Redistributes fractions evenly along the grown axis (matches `from_preset`'s `1/n` distribution). The other axis is untouched.
- Empty `panes` vector → creates a 1×2 Split2H (returns pane index 1 as the "new" pane).
- Returns `None` if already at `MAX_PANES` (16).
- 5 new unit tests in `layout.rs` `mod tests`: `append_pane_horizontal_to_empty_creates_1x2`, `append_pane_horizontal_to_1x2_creates_1x3_with_one_new_pane`, `append_pane_vertical_to_2x1_creates_3x1_with_one_new_pane`, `append_pane_at_max_returns_none`, `append_pane_redistributes_fractions_evenly`.

**`crates/rusterm-ui/src/state.rs`** — two changes:
1. `prepare_split_for_sidebar_drop` Case 4 rewritten: replaced `apply_layout_preset(next)` (preset jump) with `layout.append_pane(horizontal)`. Direction heuristic: `cols() >= rows()` → horizontal (add column); else vertical (add row). For 1×2 (Split2H) this adds exactly ONE pane → 1×3. The new pane's `session_id` is cleared defensively (already empty from `append_pane`, but safety net).
2. New `append_pane_to_active(state: &mut AppState) -> Option<usize>` wrapper (mirrors `set_pane_session_for_active`): if no layout exists, builds Split2H and returns pane 1; otherwise calls `layout.append_pane(horizontal)` with the same direction heuristic. Does NOT update `state.layout_preset` (the preset no longer matches the pane count after `append_pane`).

**`crates/rusterm-ui/src/app.rs`** — three changes:
1. "Layout: N" display: replaced `layout_label(state.read().layout_preset)` (preset-based) with `layout_display_label(&state.read())` — a new helper that reads the actual pane count from the active tab's layout and returns `"Layout: N panes"` (or `"Layout: 1 pane"`). This is now a READ-ONLY display (no onclick cycle).
2. "⊕ Split" toolbar button: replaced the preset-cycling `apply_layout_preset(next)` with `append_pane_to_active(&mut state.write())`. Each click adds exactly ONE pane (1 → 2 → 3 → 4 → … → MAX_PANES).
3. Cmd/Ctrl+Shift+L hotkey: changed from `cycle_layout_preset` (preset cycling) to `append_pane_to_active` (on-demand +1 pane), matching the toolbar button's new behavior.
4. Removed dead code: `layout_label` function (was only used by the toolbar), `apply_layout_preset` and `cycle_layout_preset` imports (no longer called from `app.rs` non-test code). `LayoutPreset` removed from the top-level `use crate::layout::{...}` (only used in test modules, which import it themselves). `MAX_PANES` import in `state.rs` is `#[cfg(test)]`-gated.

### Tests updated

- `sidebar_drop_grows_layout_when_all_panes_occupied`: was Split2H → Grid4 (4 panes); now Split2H → 1×3 (3 panes, exactly +1). Also asserts the new pane is at col=2, row=0, and is empty.
- `sidebar_drop_falls_back_to_replace_at_grid8`: RENAMED to `sidebar_drop_falls_back_to_replace_at_max_panes`. The old test built a Grid8 and expected fallback; the new test grows a Split2H layout to `MAX_PANES` (16) via repeated `append_pane(true)` calls and expects the fallback at the cap. This reflects the new "grow on demand until MAX_PANES" model.
- New test `sidebar_drop_repeated_each_adds_exactly_one_pane`: verifies 3 sequential drops on a 1×N strip each add exactly one pane (1×2 → 1×3 → 1×4 → 1×5), confirming the on-demand contract.
- Total: 310 tests in `rusterm-ui` (was 304; +6: 5 layout + 1 state). All passing.

### Design decisions

- **Direction heuristic uses `cols() >= rows()`, not container aspect ratio**: the prior design considered using container pixel dimensions to pick direction (wide container → add column). Dropped because (a) it requires passing container size into a pure-layout function, and (b) the user's typical layouts are 1D strips where the layout's own shape (1×2, 1×3) already tells you the right direction. For square layouts (2×2 Grid4), the heuristic picks horizontal (arbitrary) — adds a full column (2 panes), the documented 2D-grid limitation.
- **`append_pane` does NOT update `state.layout_preset`**: after `append_pane`, the preset no longer matches the pane count (3 panes has no preset). The "Layout: N" display reads the pane count from the layout, not from `layout_preset`. `state.layout_preset` is now stale after `append_pane` — this is acceptable since no UI reads it anymore (the toolbar display was the only reader and it's been changed).
- **`cycle_layout_preset` and `apply_layout_preset` NOT removed from `state.rs`**: kept for backward compat (other code/tests may reference them). They're just no longer called from the toolbar button, the hotkey, or the sidebar-drop Case 4.
- **`LayoutPreset` import kept in test modules**: the test modules (`connection_target_tests`, `tab_drag_tests`) construct layouts via `PaneLayout::from_preset(LayoutPreset::Split2H, ...)` for testing hit-testing. These presets still exist and are valid for constructing test layouts — they're just not used for the on-demand growth workflow anymore.

### Pitfalls / gotchas (addendum)

- **`prepare_split_for_sidebar_drop` Case 4 borrow checker**: the function needs `state.layouts.get_mut(&active_id)` to mutate the layout, but then needs to clear the new pane's session. Solution: hold the `&mut PaneLayout` borrow, call `layout.set_pane_session(first_new_idx, String::new())` directly on it (NOT the `set_pane_session_for_active` wrapper, which would re-borrow `state`).
- **dioxus rsx! `{}` blocks can't contain `let` + element**: the initial "Layout: N" display tried `{ let pane_count = ...; span { ... } }` — dioxus's rsx! macro rejects this ("expected identifier, found string"). Fixed by extracting the logic into a helper function `layout_display_label(state: &AppState) -> String` and calling it as `{ layout_display_label(&state.read()) }`.
- **`MAX_PANES` import warning**: adding `MAX_PANES` to `state.rs`'s top-level `use crate::layout::{...}` triggers an "unused import" warning in non-test builds (it's only used in tests). Fixed by splitting into `use crate::layout::{LayoutPreset, PaneLayout};` + `#[cfg(test)] use crate::layout::MAX_PANES;`.

## Follow-ups NOT done

- **Manual runtime verification**: I didn't run the app to verify the visual changes (title bar colors, compare button, empty pane hint) render correctly in the live WKWebView. The CSS is straightforward and follows the existing `<style>` block pattern, but the user should visually confirm.
- **macOS Cmd+W vs Cmd+Shift+W**: both close the focused pane (intentional for muscle memory). User may want Cmd+Shift+W to close the whole TAB in the future.
- **Sidebar drag visual feedback during drag**: the drag-over highlight now uses bright `#7aa2f7` (was `#414868`), but there's no animated ghost element showing the connection name during the drag. Could add a tooltip-style ghost in a future pass.
- **"Distribute" doesn't clone sessions**: it only assigns existing sessions to panes (one session per pane). If the user wants the SAME session in multiple panes (tmux link-window style), they'd use the empty pane's "⧉ copy" button per pane. A future "clone to all panes" button could automate this.


## Strict N→N+1 pane growth fix (2026-07-20, runtime-path diagnosis)

The prior on-demand pass fixed sidebar Case 4 and toolbar/hotkey growth, but the real tab-drop path still called `drop_background_tab_to_create_split`, which upgraded a full `Split2H` to `Grid4`. A regression test through `execute_tab_drop_on_pane` reproduced the exact symptom: expected 3 panes, actual 4.

Final rules and implementation:
- `PaneLayout::append_pane` now adds exactly one pane for every non-empty layout shape. Legacy 2D grids are normalized to `1×N`/`N×1` while preserving pane vector order, session assignments, comparison/zoom state, and existing floating geometry.
- `append_pane_to_active` is the single state-level automatic growth path.
- Background-tab drops, self-drops/clones, sidebar drops, toolbar split, and Cmd/Ctrl+Shift+L all use it.
- Self-drop now reserves exactly one clone target (`pane_count: 1`) per operation instead of 1→2→4→8.
- Both primary manual sidebar drag and defensive HTML5 fallback branches call `prepare_split_for_sidebar_drop`; no fallback directly replaces an occupied pane.
- At `MAX_PANES`, sidebar drops preserve all occupied sessions and fall back to opening a separate top-level tab rather than replacing a pane.
- `AppState.layouts` is `#[serde(skip)]`; session restore recreates one top-level tab per session, so an old `Grid4 + empty pane` layout cannot return after restart.

Regression coverage includes:
- Real `execute_tab_drop_on_pane`: 2 occupied panes + background third session → exactly 3 filled panes.
- Sidebar prepare + assignment: 2 occupied panes → exactly 3 filled panes, originals preserved.
- Legacy Grid4 append → exactly 5 panes (normalized 1×5), not 6.
- Repeated self-drop: 1→2→3→…→MAX_PANES, one clone target each time.
- Floating-pane geometry preservation during +1 growth.

Validation: `cargo test -p rusterm-ui --lib` (311 passed), `cargo build -p rusterm-app`, nightly rustfmt, and `git diff --check HEAD` all passed.

## Local split-tree layout follow-up (2026-07-20)

- `PaneLayout` now keeps stable `panes` vector indices for focus/session routing while recursive `SplitNode` geometry supports local `Top`/`Bottom` and `Left`/`Right` splits.
- Manual drag hit-testing uses top 40% / center 20% / bottom 40%; upper/lower zones map to splitting only the target leaf.
- Direction-aware sidebar and background-tab paths preserve the target session and add exactly one leaf (`N → N+1`).
- Splitter geometry is generated recursively and rendered only inside its parent subtree; resizing uses the local subtree extent.
- Toolbar/hotkey growth uses `append_balanced` (largest leaf, longest side), avoiding forced `1×5` strips.
- Legacy row/column fields and preset constructors remain only as compatibility adapters; runtime geometry comes from the tree.
- Validation: nightly rustfmt; `cargo test -p rusterm-ui --lib` (318 passed); `cargo build -p rusterm-app`; `git diff --check HEAD`.

## Four-quadrant drag hints with center crosshair (2026-07-20)

- `PaneDropRegion` extended from `{Top, Center, Bottom}` to `{Top, Bottom, Left, Right, Center}` so drops can request any of the four cardinal split directions.
- `hit_test_pane_drop_target_at` rewritten with a 4-quadrant scheme: cursor's normalised distance from pane center (`dx`, `dy` each in `[-0.5, 0.5]`). Center ±0.15 on BOTH axes is the swap/move zone; outside that, the dominant axis (the larger of `|dx|`/`|dy|`) wins. Ties go to vertical (matches the old top/bottom behaviour).
- Decision logic extracted into `pane_drop_region_for_cursor(dx, dy)` so `finish_tab_drag`'s single-pane fallback and the App polling loop's single-pane fallback share the SAME region computation as the multi-pane hit-test.
- `drag_over_pane` signal changed from `Signal<Option<usize>>` to `Signal<Option<(usize, PaneDropRegion)>>` so the visual overlay can render the correct target half.
- `finish_tab_drag`'s region → direction mapping now covers all four cardinal directions (`Left → Left`, `Right → Right`, etc.) plus `Center → Bottom` as the default swap/move fallback.
- Visual feedback: when `drag_over_region` is `Some`, the pane's terminal content area renders (a) a translucent blue (`rgba(122,162,247,0.18)`) rectangle covering the target half (skipped for `Center`), and (b) the SINGLE relevant center line ("中线") via `center_line_styles_for_region(region)`.
- Validation: nightly rustfmt; `cargo test -p rusterm-ui --lib` (323 passed, +5 new); `cargo test --workspace` (all green); `cargo build -p rusterm-app`; `git diff --check HEAD`.

## Single-line center marker + HTML5/polling-loop race fix (2026-07-20, follow-up)

The user reported the 4-quadrant drag "错误的产生多个不需要的四方块" — multiple unwanted "田"-shaped (4-block) overlays appeared during a drag. Two root causes:

1. **Visual ambiguity**: the prior overlay always drew BOTH center lines (vertical + horizontal), forming a "田" shape that was ambiguous about which direction the split would go and visually competed with the half-rectangle highlight.
2. **HTML5 vs polling-loop race**: the HTML5 `ondragover`/`ondragenter` handlers wrote `drag_over_pane.set(Some((idx, PaneDropRegion::Center)))` at ~60Hz while the manual polling loop wrote the real 4-quadrant region every ~16ms. At pane boundaries the two disagreed (HTML5 reported pane A, hit-test said pane B), flickering the overlay between adjacent panes — visually appearing as multiple overlays.

### Fixes

- **New pure helper `center_line_styles_for_region(region) -> (Option<&'static str>, Option<&'static str>)`** returns `(vertical_line_style, horizontal_line_style)`. The overlay now shows EXACTLY ONE bright line per split region:
  - `Left`/`Right` → vertical center line only (signals 横着 / horizontal placement)
  - `Top`/`Bottom` → horizontal center line only (signals 竖着 / vertical placement)
  - `Center` → both lines dimmed (swap/move zone, no split)
  - Bright lines are 2px solid `#7aa2f7` with a `box-shadow: 0 0 6px rgba(122,162,247,0.5)` glow; dimmed lines are 1px `rgba(122,162,247,0.35)` with no glow.
  - Both `single_pane_with_drop` and `multi_pane_container` call this helper so the line-selection logic is shared.
- **HTML5 `ondragover`/`ondragenter` no longer write `drag_over_pane`** — they only call `prevent_default()` (for drop permission). The manual polling loop in `App` is now the SOLE writer of `drag_over_pane`, eliminating the race. `ondrop` and `finish_tab_drag` still clear the signal to `None`.
- This implements the user's "用中线作为标记" request: ONE center line tells the user whether the new pane will sit beside the existing one (横着) or stack above/below (竖着).

### Tests added (3 new, 326 total)

- `center_line_styles_for_region_shows_one_line_per_split_axis` — verifies Left/Right produce only a vertical line, Top/Bottom only a horizontal line, Center produces both (dimmed).
- `center_line_styles_for_region_uses_bright_line_for_splits_dimmed_for_center` — verifies split lines are 2px full-opacity with glow, center lines are 1px 35% opacity without glow.
- `center_line_styles_for_region_is_symmetric_within_split_axis` — verifies Left==Right and Top==Bottom line styles (regression guard against permuting styles per side).

### Validation

- nightly rustfmt on `app.rs`
- `cargo test -p rusterm-ui --lib` → 326 passed (323 prior + 3 new)
- `cargo build -p rusterm-app` → clean
- `git diff --check HEAD` → clean

### Pitfalls / gotchas

- The HTML5 `ondragover`/`ondragenter` handlers MUST stay as `prevent_default`-only. Re-adding signal writes there will reintroduce the flicker bug.
- The polling loop is the SOLE writer of `drag_over_pane` during a drag. If the loop ever stops running (e.g., `tab_drag` signal never set), the overlay won't update — but that's the correct behaviour because no drag is in progress.
- `ondrop` still writes `drag_over_pane.set(None)` as a defensive clear — this is safe because it only fires once at drag end.
- The bright line uses `box-shadow` for the glow effect; this is purely visual and doesn't affect hit-testing.
