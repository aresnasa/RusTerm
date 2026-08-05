# Session tab reorder + numbering (issue #122 continuation)

## What it is
Two improvements to the top tab bar (`TabBar` component):
1. **Drag-to-reorder**: drag a session tab onto another tab in the bar to
   reposition it (insert before/after based on cursor x vs. tab midpoint).
2. **Positional numbering**: each tab shows a 1-based index (1, 2, 3…) in a
   muted colour, reflecting current order (updates after a reorder).

## Key files
- `crates/rusterm-ui/src/state.rs` — `reorder_tab(state, dragged_session_id,
  target_tab_id, before: bool) -> bool`. Moves the tab whose anchor is the
  dragged session to sit immediately before/after the target tab. No-ops
  (returns `false`) when source == target, either id is unknown, or the tab
  is already in the requested slot. Preserves `active_tab`/`active_session`
  (id-based, not index-based). 11 unit tests mirror `move_session_to_leftmost`.
- `crates/rusterm-ui/src/components/tab_bar.rs` — `data-rusterm-tab-id` attr on
  each tab div (for JS hit-test) + a muted `{tab_number}` span before the
  connection-state dot. Loop changed from `for tab in tabs` to
  `for (tab_index, tab) in tabs.into_iter().enumerate()`.
- `crates/rusterm-ui/src/app.rs`:
  - `TabDragHover` struct `{ tab_id: String, before: bool }` — the hovered-tab
    hit-test result.
  - `parse_tab_drag_poll_response` + `poll_tab_drag_state` extended from a
    6-tuple to a 7-tuple `(x, y, done, left, top, group_id, tab_hover)`.
    JS poll now also does `hit.closest('[data-rusterm-tab-id]')` and computes
    `before` from `cursor.x < tabRect.left + tabRect.width/2`. Poll format is
    now 8 `\u{1f}`-separated fields (was 6).
  - `finish_tab_drag` gained a `drop_tab_hover: Option<TabDragHover>` param.
    BEFORE the pane hit-test, if it's a `Session` drag AND a tab is hovered
    AND source != target, it calls `reorder_tab` and returns early. This keeps
    tab-reorder and pane-split/swap/move mutually exclusive: drop on a tab =
    reorder; drop on a pane = split/swap/move.
  - `reorder_tab` imported into `app.rs` alongside `move_session_to_leftmost`.

## Design decisions
- **Positional (not stable) numbering**: numbers reflect current `state.tabs`
  order. Simpler, matches "显示更加清晰" intent, and reordering wouldn't make
  stable numbering confusing. No config/persistence needed — tab order is
  runtime-only state (not in `PersistedConfig`).
- **Reused the existing manual mouse-drag system** (document-capture + 60Hz
  polling) rather than reintroducing HTML5 DnD (which was deliberately removed
  — see the long comment at `app.rs` ~L16094). Extended the existing JS poll
  to also hit-test tabs, instead of a separate eval (avoids doubling JS
  round-trips per poll tick).
- **before/after via cursor midpoint**: JS computes
  `Number(coords[0]) < tabRect.left + tabRect.width/2` → "1" (before) / "0"
  (after). This gives precise insert control (browsers do the same).
- **No i18n needed**: numbers are universal.
- **No config persistence**: tab order is runtime-only; `save_config` is NOT
  called after a reorder (it only saves connections anyway, not tab order).

## Reorder semantics (the `would_be_noop` check)
After removing the source tab, the target's index may shift. The no-op check
handles two cases where the tab is already in the requested slot:
- `before=true, src_pos < tgt_pos`: src already immediately precedes tgt.
- `after=true, src_pos > tgt_pos`: src already immediately follows tgt.
Both check `insert_at == src_pos` (NOT `src_pos + 1` — a bug in the first
draft that the `reorder_tab_after_already_following_is_noop` test caught).

## Tests
- 11 `reorder_tab_*` tests in `state.rs` (before/after, forward/backward,
  self-drop, already-in-position no-ops, unknown ids, active-tab preservation).
- 4 new + 5 updated `tab_drag_parse_*` tests in `app.rs` for the 8-field poll
  format (hovered tab before/after, invalid `before` rejected, no-tab + group).

## Pitfalls hit
- **Doc comments (`///`) on fn params are illegal in Rust** — use `//`. The
  `drop_tab_hover` param doc had to be downgraded to a plain comment.
- **`type_complexity` clippy lint** on the 7-tuple return — added
  `#[allow(clippy::type_complexity)]` to both `parse_tab_drag_poll_response`
  and `poll_tab_drag_state`.
- **`would_be_noop` off-by-one**: the `after` case checked `src_pos + 1`
  instead of `src_pos`. Fixed after a test caught it.
