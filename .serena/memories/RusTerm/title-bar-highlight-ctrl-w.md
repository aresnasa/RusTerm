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

## Crosshair restoration + close-confirmation dialog (2026-07-20, follow-up)

Two changes in this pass:

### 1. Split-pane overlay: crosshair restored (both lines always drawn)

The user clarified they want the crosshair (田字形 — both vertical + horizontal center lines) back so they can see the 4-quadrant grid and drop into whichever box they want. The prior single-line-per-region change was reverted in `center_line_styles_for_region`:

- **All split regions** (`Left`/`Right`/`Top`/`Bottom`) → BOTH lines bright (2px `#7aa2f7` + glow). The half-rectangle highlight (rendered by the caller) communicates the split direction; the crosshair itself no longer encodes direction.
- **Center** → both lines dimmed (1px `rgba(122,162,247,0.35)`, no glow).

The HTML5/polling-loop race fix is PRESERVED (the polling loop remains the sole writer of `drag_over_pane`). The crosshair is safe now because the race that caused the "multiple 四方块" bug was the signal-write race, not the visual crosshair itself.

Tests updated: `center_line_styles_for_region_shows_one_line_per_split_axis` → renamed to `center_line_styles_for_region_shows_both_lines`; the bright/dimmed test now checks BOTH lines for all 4 split regions; the symmetry test is unchanged (still passes).

### 2. Last-window close-confirmation dialog

When the user closes the last window (OS close button, Cmd+Q, Alt+F4), a confirmation dialog now appears: "是否确实要关闭本软件？" with:
- A checkbox "下次关闭时不再询问" — **checked by default** ("默认勾选")
- **取消** button (primary/default — "取消默认关闭"): keeps the app running
- **确认关闭** button: actually exits the app
- Both buttons always visible ("都要显示给用户选择")

#### dioxus-desktop 0.7 close-event architecture (important)

`dioxus-desktop` 0.7.9's `handle_close_requested` (in `launch.rs`'s event loop) ALWAYS processes the close — there is no `prevent_default` mechanism exposed to wry event handlers. The handler reads `close_behaviour` (a `Cell<WindowCloseBehaviour>` on `DesktopContext`) and either hides (`WindowHides`) or destroys (`WindowCloses`) the window. The wry event handler registered via `use_wry_event_handler` runs in `app.tick(event)` BEFORE `handle_close_requested` in the same event-loop iteration.

**Interception strategy**:
1. Config sets `with_close_behaviour(WindowHides)` in `main.rs` — default action is hide.
2. `use_wry_event_handler` on `WindowEvent::CloseRequested`:
   - If `confirm_close_on_exit == false` (user previously opted out) → call `desktop.set_close_behavior(WindowCloses)` so `handle_close_requested` (running right after us) actually destroys + exits.
   - If `confirm_close_on_exit == true` → set `close_dialog_visible = true` + reset the checkbox to checked. Leave `close_behaviour` as `WindowHides` so the window is hidden (not destroyed).
3. A `use_future` polls `close_dialog_visible` every 50ms; when it transitions to true, calls `desktop.set_visible(true)` to re-show the hidden window (with the dialog now rendered). The brief hide→show flicker (~16ms) is unavoidable given dioxus 0.7's architecture.
4. Dialog buttons:
   - **取消** → `close_dialog_visible = false`. If checkbox was checked, persist `confirm_close_on_exit = false` (next close auto-cancels — close button does nothing visible).
   - **确认关闭** → if checkbox was checked, persist `confirm_close_on_exit = false` (next close exits immediately). Then `set_close_behavior(WindowCloses)` + `desktop.close()` (sends `UserWindowEvent::CloseWindow` → `handle_close_requested` → `WindowCloses` → destroys + exits).

#### Persistence

- `PersistedConfig` (`crates/rusterm-core/src/config.rs`) gained a `confirm_close_on_exit: bool` field with `#[serde(default = "default_confirm_close_on_exit")]` (defaults to `true` for old settings files).
- `ConfigManager` gained `load_confirm_close_on_exit()` + `save_confirm_close_on_exit(bool)` (read-modify-write, preserves all other settings).
- `AppState` gained `confirm_close_on_exit: bool` (persisted), `close_dialog_visible: bool` (transient, `#[serde(skip)]`), `close_dialog_dont_ask_again: bool` (transient).
- Loaded on unlock (right next to `restore_disabled`).
- New test `confirm_close_on_exit_defaults_to_true_and_roundtrips` (rusterm-core, 101 tests total).

#### Files touched

- `crates/rusterm-core/src/config.rs` — `PersistedConfig` + `default_confirm_close_on_exit`.
- `crates/rusterm-core/src/config_manager.rs` — all 6 `PersistedConfig { ... }` literals updated; new `load_confirm_close_on_exit` + `save_confirm_close_on_exit`; new test.
- `crates/rusterm-app/src/main.rs` — `with_close_behaviour(WindowHides)`.
- `crates/rusterm-ui/src/state.rs` — 3 new `AppState` fields + defaults.
- `crates/rusterm-ui/src/components/close_confirmation_dialog.rs` — new component.
- `crates/rusterm-ui/src/components.rs` — module + re-export.
- `crates/rusterm-ui/src/app.rs` — wry event handler + reshow `use_future` + dialog render + import + load on unlock.

### Validation

- nightly rustfmt on all touched files
- `cargo test -p rusterm-ui --lib` → 326 passed
- `cargo test -p rusterm-core --lib` → 101 passed (+1 new)
- `cargo build -p rusterm-app` → clean, no warnings
- `git diff --check HEAD` → clean

### Pitfalls / gotchas

- The wry `CloseRequested` handler runs BEFORE `handle_close_requested`, so to actually close (vs hide), we must call `set_close_behavior(WindowCloses)` in the handler — the subsequent `handle_close_requested` then reads the updated behaviour.
- `desktop.close()` sends `UserWindowEvent::CloseWindow(id)` which re-enters `handle_close_requested`. This is the intended exit path from the dialog's 确认关闭 button — there's no infinite loop because we only call `desktop.close()` from the button handler (not from the wry `CloseRequested` handler).
- `use_future` closures that capture `Rc<DesktopService>` (from `dioxus::desktop::window()`) must clone the `Rc` inside the `move || { ... async move { ... } }` body — the outer `move` would otherwise move the `Rc` into the outer closure, leaving nothing for the inner `async move`.
- `rsx!` `if` blocks cannot contain `let` statements — pass props via inline expressions (`state.read().field`) instead of binding locals.
- The reshow `use_future` polls every 50ms (not event-driven) because dioxus 0.7 doesn't expose a wake-up hook for signal changes from within a wry handler. This is cheap (one bool read per 50ms).

## Empty-pane button fix + Split/Distribute mode toggle (2026-07-20, follow-up)

Two changes in this pass:

### 1. Fix: empty-pane title bar buttons unresponsive

**Root cause**: The `⧉` (copy) and `+` (sidebar) buttons in `empty_pane_title_actions` called `e.prevent_default()` on `onmousedown`. In some webview implementations (notably macOS webkit via wry), `prevent_default()` on mousedown prevents the subsequent `click` event from firing, leaving the buttons visually rendered but functionally dead.

**Fix**: Removed `e.prevent_default()` from both buttons' `onmousedown` handlers. Kept `e.stop_propagation()` (still needed to prevent the title bar's `onmousedown` — which starts a tab drag for non-empty panes — from firing). The buttons' `onclick` handlers are unchanged.

This is safe because:
- `<button>` elements don't trigger text selection on mousedown (so `prevent_default` wasn't needed for that).
- `stop_propagation` alone is sufficient to prevent the parent title bar's mousedown handler from firing.
- Without `prevent_default`, the webview's native click synthesis proceeds normally.

### 2. Split/Distribute mode toggle (标签页平铺 / tab tiling)

The "⊕ Split" and "⇶ Distribute" toolbar buttons were one-shot action buttons. Now both support toggle state:

- **Split button**: toggles `split_mode_enabled`. ON (highlighted green border + background) = multi-pane layout visible. OFF (dimmed grey) = "标签页平铺" — the layout is zoomed to the focused pane so only one pane is visible; all sessions remain accessible via the workspace tab bar. When turning ON with no existing layout, it creates a Split2H layout (appends one pane).
- **Distribute button**: same toggle behavior, but when turning ON it also calls `distribute_sessions_across_panes` to fill all panes with open sessions. When OFF, same tab-tiling behavior as Split OFF.
- Both buttons share the same `split_mode_enabled` flag, so toggling either one updates both buttons' visual state.

#### Implementation

- **`AppState.split_mode_enabled: bool`** (default `true`, `#[serde(skip)]`) — controls whether the multi-pane layout is visible or collapsed to single-pane.
- **`toggle_split_mode(state: &mut AppState) -> Option<bool>`** (new in `state.rs`):
  - OFF → ON: calls `layout.unzoom()` to reveal all panes.
  - ON → OFF: calls `layout.zoom(focused_pane_idx)` (or pane 0 if no focus) so `is_multi_pane()` returns false → rendering takes the `single_pane_with_drop` path.
  - If no layout exists, still flips the flag (caller creates layout via `append_pane_to_active`).
  - If layout has ≤1 panes (Single preset), forces `split_mode_enabled = true` (nothing to collapse).
  - Returns `None` only if there's no active tab.
- **App rendering**: `is_multi` now includes `&& split_mode_on` as a safety net. When split mode is OFF, the single-pane path renders the focused pane's session (not the tab anchor) via an extended `render_sid` lookup.
- **`layout_display_label`**: shows "(tab-tiled)" suffix when split mode is OFF and the layout has >1 panes.
- **Cmd/Ctrl+Shift+L hotkey**: if split mode is OFF, turns it ON first, then appends a pane.

#### Visual indicators

- Split ON: `color: #9ece6a; border: 1px solid #9ece6a; background: rgba(158,206,106,0.15);`
- Split OFF: `color: #565f89; border: 1px solid #414868;` (no background)
- Distribute ON: `color: #bb9af7; border: 1px solid #bb9af7; background: rgba(187,154,247,0.15);`
- Distribute OFF: `color: #565f89; border: 1px solid #414868;` (no background)

### Tests added (5 new, 331 total in rusterm-ui)

- `toggle_split_mode_off_zooms_focused_pane` — OFF zooms to pane 0, `is_multi_pane()` becomes false.
- `toggle_split_mode_on_unzooms_layout` — ON clears `zoomed`, restores multi-pane.
- `toggle_split_mode_with_no_layout_still_flips_flag` — no layout → flag still flips.
- `toggle_split_mode_off_uses_focused_pane_idx` — OFF zooms to the focused pane (pane 2 in a Grid4).
- `toggle_split_mode_off_preserves_layout_tree` — OFF→ON round-trip preserves splitter ratios (verified via `pane_rect` width).

### Validation

- nightly rustfmt on `app.rs` + `state.rs`
- `cargo test -p rusterm-ui --lib` → 331 passed (326 prior + 5 new)
- `cargo test -p rusterm-core --lib` → 100 passed
- `cargo build --workspace` → clean
- `git diff --check HEAD` → clean

### Pitfalls / gotchas

- **`prevent_default()` on mousedown blocks click in webkit**: the root cause of the empty-pane button bug. Never call `prevent_default()` on a button's `onmousedown` if you need `onclick` to fire — use `stop_propagation()` alone.
- **`split_mode_enabled` is global, but `zoomed` is per-layout**: when the user switches tabs, the new tab's layout may not be zoomed even if `split_mode_enabled` is false. The `is_multi` check in App rendering (`&& split_mode_on`) is a safety net that forces the single-pane path regardless of the layout's `zoomed` state.
- **`toggle_split_mode` with no layout**: must still flip the flag (returns `Some(false)`), so the Split button's onclick can detect the OFF→ON transition and create a layout via `append_pane_to_active`.
- **`active_tab_id` is `Option<String>`**: when comparing with `focused_pane.layout_owner_tab_id` (a `String`), use `Some(&fp.layout_owner_tab_id) == active_tid.as_ref()` — not `fp.layout_owner_tab_id == active_tid`.
- **Borrow checker with `state.read().layouts.get(tid)`**: the `Ref` from `state.read()` is a temporary that doesn't live long enough for `.map(|l| l.panes.len() <= 1)`. Use `.map(|l| l.panes.len())` (returns `usize`, a Copy) then `.map(|len| len <= 1)` outside the borrow.

## Drag-over pane highlight enhancement (2026-07-20, follow-up)

### Goal

User asked: "拖拽的时候会话在 4 个窗格的某一个窗格需要高亮这个窗格提示用户。"

The existing drag-over highlight (2px border + blue title bar + 4-quadrant overlay) was too subtle on large panes — the border was barely visible and there was no glow to draw the eye to the target pane. This pass makes the highlight unmistakable at any pane size.

### Changes (all in `multi_pane_container`, `app.rs`)

1. **Border** — `2px solid #7aa2f7` → `3px solid #7aa2f7` PLUS a two-layer `box-shadow`:
   - `0 0 0 1px rgba(122,162,247,0.55)` — a 1px outer ring (intensifies the edge)
   - `0 0 18px rgba(122,162,247,0.55)` — an 18px diffuse glow that surrounds the whole pane
   The glow is what makes the highlight visible on large panes — a 3px border alone still disappears into the surrounding terminal text.
2. **Title bar chrome** — added `box-shadow: inset 0 0 0 1px rgba(187,154,247,0.6), 0 2px 10px rgba(122,162,247,0.55)`:
   - The inset 1px purple ring sharpens the title bar edge against the bright blue background.
   - The 10px drop shadow makes the title bar visually lift off the pane.
3. **Content-area tint** — NEW div at `z-index: 5` (under the 4-quadrant overlay at z 20, over terminal content at z 0):
   - `position: absolute; inset: 0; background: rgba(122,162,247,0.08); pointer-events: none;`
   - This is the "this is the drop pane" wash — distinct from the 4-quadrant overlay which says "this HALF of the pane is where the split will land". Together they form a two-level highlight: pane-level (this tint) + region-level (the 4-quadrant half rectangle + center crosshair).
   - Mounted via `{drag_over_region.is_some().then(|| rsx! { ... })}` — `drag_over_region` is `Some` iff `drag_over_pane` points at THIS pane (the same condition as `is_drag_over` in the `pane_items` map closure, but that variable is out of scope in the `for ... in pane_items.into_iter()` loop body).

### Layering (z-index, bottom to top)

| z | element | purpose |
|---|---------|---------|
| 0 | terminal content | the actual PTY output |
| 5 | pane-level tint (NEW) | "this is the drop pane" wash |
| 20 | 4-quadrant half rectangle | "this HALF receives the split" |
| 21 | center crosshair (vertical + horizontal lines) | "用中线作为标记" axis marker |
| 100 | comparison banner | input-broadcast mode indicator |

All overlays use `pointer-events: none` so they never intercept the drop event.

### Validation

- nightly rustfmt on `app.rs`
- `cargo test -p rusterm-ui --lib` → 331 passed (no new tests; the change is purely visual CSS)
- `cargo test -p rusterm-core --lib` → 100 passed
- `cargo build --workspace` → clean
- `git diff --check HEAD` → clean

### Pitfalls / gotchas

- **`is_drag_over` is NOT in scope in the `for ... in pane_items.into_iter()` loop body** — it's a local in the `pane_items` `.map()` closure. Use `drag_over_region.is_some()` instead, which is the loop-body equivalent (both are `Some`/`true` iff `drag_over_pane` points at THIS pane).
- **`box-shadow` + `box-sizing: border-box`**: the 3px border + 1px ring + 18px glow all sit OUTSIDE the content box (the `box-sizing: border-box` keeps the border inside the pane's w×h). The glow extends beyond the pane rect, which is fine — it's clipped by the parent container's `overflow: hidden`.
- **Don't put the tint at z 20** — it would cover the 4-quadrant half rectangle and the crosshair. z 5 keeps it as a background wash.
- **Don't add `pointer-events: auto` to the tint** — it would intercept the drop and the pane's `ondrop` would never fire.
## X close button on occupied pane title bars (2026-07-20)

### Problem

Non-empty panes had `rsx! {}` (empty) for `pane_actions`, so there was no X close button on their title bars. Only empty panes had action buttons (copy / add). Users had to use keyboard shortcuts (Cmd+W / Ctrl+Shift+W) to close a pane's session — no mouse path existed.

### Solution

Added `occupied_pane_title_actions` function (in `app.rs`, right after `empty_pane_title_actions`):
- Takes `(mut state, mut input_senders, session_id)` — `mut` on `input_senders` is required because the `onclick` calls `&mut input_senders.write()` (the `Signal::write()` borrow needs a mutable binding; `empty_pane_title_actions` didn't need it because it passes `input_senders` by value to `clone_session_into_pane` instead of calling `.write()` directly).
- Renders a single "✕" button styled with `#f7768e` (Tokyo Night red) to signal danger/close.
- `onclick` calls `close_session(&mut state.write(), &mut input_senders.write(), &sid_for_close)` then `restore_focus_to_active_session(state, 50)`.
- `onmousedown` calls ONLY `e.stop_propagation()` — NOT `e.prevent_default()` (webkit on macOS blocks `click` when mousedown prevents default; same lesson as the empty-pane button fix).
- `onclick` also calls `e.stop_propagation()` so the pane div's `onclick` (which changes focus) doesn't fire after the session is already closed.

Wired into `pane_actions` `else` branch (was `rsx! {}`):
```rust
let pane_actions = if sid.is_empty() {
    empty_pane_title_actions(state, input_senders, layout_owner_tab_id.clone(), idx, copy_source)
} else {
    occupied_pane_title_actions(state, input_senders, sid.clone())
};
```

### Already-working features verified (no changes needed)

- **Cmd+W closes focused pane even when terminal has focus**: the TerminalView's `onkeydown` (`terminal_view.rs` L482-484) has `if meta { return; }` — returns early WITHOUT calling `prevent_default()`, so Cmd+W bubbles to the App's `onkeydown` which calls `close_session`. Confirmed by reading the handler and the comment at `app.rs` L7190-7197.
- **Ctrl+Shift+W cross-platform close**: TerminalView returns early for `ctrl && shift && KeyW` (L495-501), bubbles to App.
- **Ctrl+W (Linux/Windows) when terminal NOT focused**: App's `onkeydown` handles it (L7257+). When terminal IS focused, TerminalView intercepts and sends `0x17` to PTY (standard terminal behavior — intentionally preserved).
- **Focus-on-click**: pane div `onclick` (L3072+) and title bar `onmousedown` (L3324+) both call `focus_pane_for_layout`.

### Validation

- nightly rustfmt on `app.rs` → clean
- `cargo build --workspace` → clean (one iteration: needed `mut input_senders` in the function signature)
- `cargo test -p rusterm-ui --lib` → 331 passed
- `cargo test -p rusterm-core --lib` → 100 passed
- `git diff --check HEAD` → clean

## `cargo run` warning cleanup (2026-07-20, follow-up)

### Goal

User pasted `cargo run` output showing two categories of warnings and asked to fix them:
1. `warning: rusterm-app@0.1.0: Generated app icon PNG: ...` (3 lines from build.rs)
2. `warning: the following packages contain code that will be rejected by a future version of Rust: block v0.1.6` (future-incompat lint)

### Root causes

- **App icon warnings**: `crates/rusterm-app/build.rs` emitted 3 `println!("cargo:warning=...")` lines for status logging (PNG/SVG/icns generation). These show up as `warning:` in cargo output despite being purely informational.
- **`block v0.1.6` future-incompat**: The crate declares `enum Class { }` (uninhabited) and then `static _NSConcreteStackBlock: Class;` — an uninhabited static. Rust issue #74840 phases this out. `block` is unmaintained (last release 2018) and pulled in transitively via `cocoa v0.26.1` → `dioxus-desktop v0.7.9` → `dioxus v0.7.9`, so we can't upgrade it directly.

### Fixes

#### 1. Vendored `block` crate with minimal patch

Created `third_party/block/` (copy of `block-0.1.6` from cargo registry) and added a `[patch.crates-io]` entry in the workspace `Cargo.toml`:

```toml
[patch.crates-io]
block = { path = "third_party/block" }
```

Minimal source changes in `third_party/block/src/lib.rs`:
- Replaced `enum Class { }` (uninhabited) with:
  ```rust
  #[repr(C)]
  struct Class {
      _opaque: [u8; 0],
  }
  ```
  `Class` is only ever used as `*const Class` (pointer for identity comparison in `BlockBase.isa`), so a zero-sized inhabited struct with defined layout is semantically equivalent and silences both the `uninhabited_static` lint AND the `improper_ctypes` lint (which `PhantomData<()>`-only structs would trip).
- Fixed two macro-generated deprecation warnings: `unsafe extern fn` → `unsafe extern "C" fn` in `block_args_impl!` and `concrete_block_impl!` macros (the original code predated Rust 2024's `unsafe_extern_blocks` requirement).

#### 2. Build script status messages silenced

In `crates/rusterm-app/build.rs`, replaced:
```rust
println!("cargo:warning=Generated app icon PNG: ...");
```
with:
```rust
eprintln!("[rusterm-app] generated app icon PNG: ...");
```

`eprintln!` writes to stderr (captured by cargo into `target/debug/build/<hash>/stderr` for diagnostics) but does NOT appear as a `warning:` line in the build summary. Status messages are still available if needed via `cat target/debug/build/rusterm-app-*/stderr`.

### Validation

- `cargo clean && cargo build --workspace` → clean, ZERO warnings, ZERO errors
- `cargo build --workspace 2>&1 | grep -E "^warning:|^error:"` → no output (clean)
- `cargo test -p rusterm-ui --lib` → 331 passed
- `cargo test -p rusterm-core --lib` → 100 passed
- App icon files still generated correctly:
  - `target/debug/build/rusterm-app-*/out/icon.png` (63410 bytes)
  - `target/debug/build/rusterm-app-*/out/assets/gemini-svg.svg` (4506 bytes)
  - `target/debug/build/rusterm-app-*/out/AppIcon.icns` (285028 bytes)
- `git status` → clean (changes auto-committed in `684a1aa`)

### Pitfalls / gotchas

- **Don't use `enum Class { }` (empty enum)** — uninhabited static triggers future-incompat lint #74840.
- **Don't use `struct Class(PhantomData<()>)`** — even with `#[repr(transparent)]`, the `improper_ctypes` lint rejects `PhantomData`-only types in `extern "C"` blocks.
- **`#[repr(C)] struct Class { _opaque: [u8; 0] }` is the right fix** — zero-sized, FFI-safe, defined layout, doesn't trip either lint.
- **`cargo:warning=` is for actual warnings, not status logging** — use `eprintln!` for status messages that should appear in stderr but not in the warning summary.
- **`[patch.crates-io]` requires the patched crate to have the same name+version** as the original. Our `third_party/block/Cargo.toml` keeps `name = "block"` and `version = "0.1.6"` to match.
- **The patch is workspace-wide** — `[patch.crates-io]` in the workspace `Cargo.toml` applies to ALL workspace members and ALL transitive deps. There's no way to patch for just one consumer.

## Session config local-reading + config path stability (2026-07-20)

### Goal

User asked: "配置会话时支持读取本地配置，这里可以提示相关路径而不是让用户必须输入，改造相关函数。本地配置需要存到一个默认位置，比如加目录的~/.config/rusterm/下，在 cargo clean 时不要删除已有的.config目录"

Two related asks:
1. The SSH connection dialog should read `~/.ssh/config` and `~/.ssh/id_*` and offer autocomplete suggestions + auto-fill instead of forcing the user to type everything.
2. The app's own config (`settings.json`, `session_state.json`, `window_state.json`) should live at a stable platform location (`~/.config/rusterm/` on Linux, `~/Library/Application Support/rusterm/` on macOS) that survives `cargo clean`, NOT next to the binary (which gets wiped by `cargo clean` during development).

### 1. New `rusterm-ssh::ssh_config` module

`crates/rusterm-ssh/src/ssh_config.rs` (980 lines, 30 unit tests). Reads the user's local OpenSSH config and exposes a small, well-typed surface for the UI.

**Public API** (re-exported from `rusterm-ssh`):
- `list_ssh_config_hosts() -> Vec<SshHostSuggestion>` — parse `~/.ssh/config` (or `list_ssh_config_hosts_at(path)` for an arbitrary path) and return one `SshHostSuggestion` per `Host` directive with a **literal** alias (wildcards like `*` or `*.example.com` are skipped — they're pattern matchers, not selectable autocomplete entries). Multi-host `Host a b c` directives expand to one suggestion per alias.
- `list_identity_files() -> Vec<String>` (or `list_identity_files_at(dir)`) — scan `~/.ssh/` for `id_*` private keys, returning **absolute** paths (tilde expanded). Excludes `.pub` files, `config`, `known_hosts`, `authorized_keys`, `environment`. Sorted lexically by filename for a stable suggestion list.
- `lookup_host(alias, path) -> Option<ResolvedHost>` — two-step lookup: (1) check our own `list_ssh_config_hosts` to confirm the alias is a literal `Host` entry (returns `None` if not — we don't want to clobber the user's in-progress form with `russh-config`'s default values for a host that isn't in the config); (2) defer to `russh-config`'s `parse_path`/`parse_home` for the authoritative resolution (handles `Include`, `Match`, percent tokens). Returns `ResolvedHost { host, port, user, identity_file, proxy_jump }`.
- `resolved_host_to_auth(&ResolvedHost) -> SshAuth` — convert to the `SshAuth` variant the rest of the codebase expects. `Some(identity_file)` → `SshAuth::Key { private_key_path, passphrase: None }`; `None` → `SshAuth::Agent` (OpenSSH convention: no `IdentityFile` means consult the agent).
- `parse_ssh_config_text(contents) -> Vec<SshHostSuggestion>` — pure parser (no I/O), exposed for unit testing. Handles comments (full-line `#`/`;` and inline ` #`), case-insensitive keywords, tab/space indentation, multi-host directives, wildcard filtering, `IdentityFile` tilde expansion, `ProxyJump`. The first `IdentityFile` in a block wins (OpenSSH tries them in order). Unknown directives (Compression, ForwardAgent, etc.) are silently skipped.
- `default_ssh_config_path() -> Option<PathBuf>` / `default_ssh_dir() -> Option<PathBuf>` — resolved paths for the UI hint display.
- `is_wildcard_pattern(alias) -> bool`, `is_identity_file(name) -> bool`, `expand_tilde(path, home) -> String` — pure helpers, each unit-tested.

**Why we don't use `russh-config` for `list_ssh_config_hosts`**: `russh-config`'s public API is `parse(file, host) -> Config` — it queries a single host by name. The `SshConfig` struct (with the parsed entries list) is private, so we can't iterate all `Host` directives through `russh-config`'s API. We do a minimal hand-parse for the list-all-hosts use case, and defer to `russh-config`'s proper parser for the single-host lookup via `lookup_host`.

**Why `lookup_host` checks our own list first**: `russh-config`'s `parse_path` *always* returns a `Config` — it folds over all matching `Host` entries and returns `HostConfig::default()` (all `None`) when nothing matches. That means `parse_path("not-in-config", ...)` succeeds with `port=22` and `user=current_user`, which would clobber the user's in-progress form input with defaults. We use our own parser to detect "is this alias literally in the config?" and only then defer to `russh-config` for the full resolution.

### 2. `ConnectionDialog` UI wiring

`crates/rusterm-ui/src/components/connection_dialog.rs` (+117 lines). The dialog now:

- Loads `host_suggestions: Signal<Vec<SshHostSuggestion>>` and `identity_suggestions: Signal<Vec<String>>` ONCE on first mount via `use_signal(list_ssh_config_hosts)` / `use_signal(list_identity_files)`. Synchronous I/O is fine here because both reads are tiny (one small text file + one directory listing), tolerant of missing files (return empty Vec), and happen only when the dialog opens (not at app startup). `use_resource` would add async overhead + a loading state for no benefit.
- Renders a `<datalist id="ssh-host-list">` under the Host input with one `<option value="{alias}">` per `~/.ssh/config` host alias. The input's `list: "ssh-host-list"` attribute links them, enabling the browser's native autocomplete dropdown.
- Renders a `<datalist id="ssh-identity-list">` under the Private Key Path input (shown only when auth_type == "key") with one `<option value="{path}">` per `~/.ssh/id_*` file.
- Path hints below each input: "提示：从 {config_path} 读取到 {N} 个主机配置" and "提示：从 ~/.ssh/ 找到 {N} 个私钥文件". Only shown when the suggestion list is non-empty (so a fresh install with no `~/.ssh/config` sees no clutter).
- Host input's `onchange` (NOT `oninput` — fires on blur or datalist selection, not every keystroke): calls `lookup_host(alias, None)`. If `Some(resolved)`, fills `host` (resolved HostName or alias), `port`, `username`, and either `key_path`+`auth_type="key"` (if `IdentityFile` set) or `auth_type="agent"` (OpenSSH convention when no `IdentityFile`).

### 3. New `rusterm-core::paths` module + config path stability

`crates/rusterm-core/src/paths.rs` (260 lines, 7 unit tests). Centralises the "where does the config file live?" logic that was previously duplicated across `config_manager.rs`, `session_state.rs`, and `window_state.rs`.

**Old resolution order** (all 3 files had a copy):
1. `RUSTERM_CONFIG_DIR` env var
2. **Next to the binary** (primary) — `<exe_dir>/settings.json`
3. Platform config dir fallback

**Problem**: during development, the binary lives under `target/debug/` (or `target/release/`), and `cargo clean` deletes the entire `target/` tree — taking the user's saved connections, master password hash, window state, and session state with it.

**New resolution order** (`paths::resolve_config_file_path(filename)`):
1. `RUSTERM_CONFIG_DIR` env var (unchanged — test/config override hook)
2. **Platform config dir** (`~/.config/rusterm/` on Linux, `~/Library/Application Support/rusterm/` on macOS, `%APPDATA%\rusterm\` on Windows) — the new primary location. Stable across `cargo clean`, follows platform conventions.
3. **Auto-migrate from "next to the binary"** — if the platform dir doesn't have the file but the binary's directory does (a legacy install or a pre-this-change dev build), we move the file to the platform dir. One-shot: after the migration, the platform dir has the file and the binary-dir copy is gone. `fs::rename` first (atomic on same filesystem); falls back to `fs::copy` + `fs::remove_file` if rename fails (cross-filesystem, e.g. binary on a mounted volume). Migration errors are logged at `tracing::warn!`/`tracing::info!` and don't fail the path resolution — the worst case is the file stays in the binary dir and the caller treats the platform path as "first launch".
4. **Binary-dir fallback** — only if the platform config dir can't be determined (very rare: `HOME` unset on Unix, corrupt OS profile on Windows). Keeps a portable / USB-stick install working as a last resort.
5. **Last-resort: `./<filename>`** — only if BOTH `dirs::config_dir()` returns `None` AND `std::env::current_exe()` fails. Should never happen in practice; a relative path is better than crashing on startup.

**Refactored callers**:
- `config_manager.rs::resolve_config_path()` → `crate::paths::resolve_config_file_path(CONFIG_FILE_NAME)` (1-line body now).
- `session_state.rs::resolve_path()` → `crate::paths::resolve_config_file_path(FILE_NAME)`.
- `window_state.rs::resolve_path()` → `crate::paths::resolve_config_file_path(WINDOW_STATE_FILE_NAME)`.

**Public helpers** for future "open config folder" UI actions:
- `app_config_dir() -> Option<PathBuf>` — the platform config dir + `rusterm` subdir.
- `platform_config_dir() -> Option<PathBuf>` — thin wrapper around `dirs::config_dir()`.
- `APP_CONFIG_SUBDIR = "rusterm"` — the subdir name constant.

### Tests

- `rusterm-ssh::ssh_config::tests` — 30 tests: parse empty/comments/blanks/wildcards/multi-host/negated-wildcard/inline-comments/hash-in-value/case-insensitive/invalid-port/proxy-jump/first-identity-file-wins; list-at-nonexistent-file/list-at-reads-file; identity-file-detection/list-skips-pub-and-non-id/list-nonexistent-dir; expand-tilde-with/without-home; wildcard-detection; lookup-host-missing-file/missing-alias/resolves-simple-block/falls-back-to-alias; resolved-to-auth-with/without-identity-file; real-world-config-with-comments-and-unknown-directives; duplicate-host-aliases; indented-and-tab-separated; strips-inline-comments; keeps-hash-when-no-preceding-whitespace.
- `rusterm-core::paths::tests` — 7 tests: env-var-override-highest-priority; env-var-creates-directory-if-missing; app-config-dir-returns-platform-plus-subdir; resolve-returns-path-with-filename; migrate-noop-if-target-exists; migrate-noop-if-source-missing; app-config-subdir-is-rusterm.
- Total workspace: 564 tests passing (was 557 before this pass). +30 ssh_config + 7 paths = +37 net.

### Validation

- nightly rustfmt on all touched files
- `cargo build --workspace` → 0 warnings, 0 errors
- `cargo test --workspace` → 564 passed, 0 failed
- `cargo clippy -p rusterm-ssh --lib` → 0 new warnings (1 pre-existing in `client.rs` line 210)
- `cargo clippy -p rusterm-ui --lib` → 0 warnings in `connection_dialog.rs` or `ssh_config.rs` (104 pre-existing in other files)
- `git diff --check HEAD` → clean

### Pitfalls / gotchas

- **`use_signal` initializer runs synchronously on first mount** — don't put slow I/O there. `~/.ssh/config` is a tiny file (usually <10KB) and `~/.ssh/` has <20 files; the directory listing is fast. If we ever needed to scan a huge directory, switch to `use_resource` (async) + a loading spinner.
- **`lookup_host` does TWO parses** — first our minimal hand-parse (to check if the alias is literal), then `russh-config`'s full parse. This is wasteful but the file is tiny and `onchange` fires once per blur (not per keystroke), so it's fine. A future optimisation could cache the parsed entries in a `OnceCell`, but that'd complicate the API for negligible gain.
- **`onchange` fires on blur AND on datalist selection** — both are correct triggers for auto-fill. Don't switch to `oninput` (fires per keystroke) — that would interrupt typing as the auto-fill clobbers mid-typed values.
- **Auto-fill clobbers user-typed port/user/identity_file when the host matches a config alias** — this is intentional. If the user picks an alias from the dropdown, they want the config's values. They can edit the fields after auto-fill if they want to override.
- **`russh-config`'s `parse_path` always returns `Ok(Config)`** (with default values when nothing matches) — that's why `lookup_host` checks our own list first. Don't "simplify" `lookup_host` by removing the pre-check; it'd reintroduce the "typing any host clobbers the form with defaults" bug.
- **Rust 2024 made `std::env::set_var`/`remove_var` unsafe** — the `paths::tests` module wraps them in `unsafe { ... }` blocks with a SAFETY comment. Single-threaded tests make this sound.
- **`fs::rename` is atomic on the same filesystem but fails cross-filesystem** — the migration code falls back to `fs::copy` + `fs::remove_file` when rename fails. Don't remove the fallback (a binary on a mounted volume + config on the root volume is a real scenario for portable installs).
- **The migration is one-shot per file** — after the first run, the platform dir has the file and the binary dir doesn't, so the migration's `if !target_path.exists()` guard short-circuits. Don't add a "migrate every time" loop — it'd be a no-op after the first run and would waste I/O.
- **`APP_CONFIG_SUBDIR = "rusterm"` is constant across platforms** — don't try to "localise" it (e.g. `RusTerm` with capital letters on macOS). The lowercase form matches the existing `dirs::config_dir().join("rusterm")` calls that were already in the codebase, and changing it would break existing installs.
- **The connection dialog's `<datalist>` uses fixed IDs `"ssh-host-list"` and `"ssh-identity-list"`** — if the dialog is ever rendered twice simultaneously (it isn't — it's a singleton modal), the IDs would collide and the browser would link both inputs to the same datalist. Not a problem now, but worth knowing if the dialog ever becomes a multi-instance component.
