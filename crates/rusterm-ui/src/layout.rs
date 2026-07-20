//! Multi-pane terminal layout engine.
//!
//! This module implements a tmux/zellij-style split-pane layout for RusTerm's
//! terminal display area. A single tab can host multiple terminal sessions
//! arranged by a recursive split tree. Each split stores a normalized ratio,
//! so resizing the window rescales the entire tree without changing its
//! structure.
//!
//! ## Design choices
//!
//! - **Proportional, not pixel-based**: each pane stores `col_frac` and
//!   `row_frac` in `[0.0, 1.0]`. The renderer multiplies these by the
//!   container's CSS pixel size to get CSS box dimensions; the PTY winsize
//!   update (cols/rows) is derived from the CSS size and a fixed cell
//!   dimension. This decouples layout from rendering and lets the layout
//!   module be unit-tested with pure arithmetic — no dioxus runtime needed.
//!
//! - **Recursive local splits**: panes remain in a stable vector for session
//!   routing, while a binary tree maps each pane index to geometry. Splitting
//!   one leaf affects only that leaf, so five sessions need exactly five
//!   leaves and never require a sixth rectangular-grid cell.
//!
//! - **Splitter dragging**: every split node exposes one local divider.
//!   Adjusting its ratio shifts space only between its two child subtrees;
//!   the `MIN_PANE_FRAC` constant enforces a minimum so panes can't be
//!   shrunk to zero (which would crash the PTY whose cols must be ≥1).
//!
//! - **Fullscreen zoom**: any pane can be "zoomed" to fill the whole
//!   container. The other panes are hidden but their state is preserved
//!   so un-zooming restores the prior layout exactly. This is the
//!   "全屏分辨率" requirement — the zoomed pane's PTY gets resized to
//!   the full container size.
//!
//! - **Comparison mode**: when enabled, scrolling and keyboard input are
//!   broadcast to every pane in the layout. This lets the user run the
//!   same command across N hosts and watch outputs side-by-side, like
//!   `tmux synchronize-panes`. Each pane still owns its own PTY; the
//!   broadcast is purely a UI-side routing decision in `app.rs`.

use serde::{Deserialize, Serialize};

/// Minimum fraction of the container a pane can occupy along either axis.
/// Prevents shrinking a pane to 0 columns/rows, which would make the PTY
/// winsize invalid (cols=0 or rows=0 panics in the terminal model).
pub const MIN_PANE_FRAC: f64 = 0.1;

/// The maximum number of panes in a single tab. 8-way split is the largest
/// preset; the cap exists so a runaway split-loop can't OOM the app.
pub const MAX_PANES: usize = 16;

/// Normalized geometry for a freely movable pane window.
///
/// Values are fractions of the terminal container, so resizing the app window
/// preserves each pane's relative position and size.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FloatingPane {
    pub x_frac: f64,
    pub y_frac: f64,
    pub width_frac: f64,
    pub height_frac: f64,
    pub z_index: u32,
}

/// A single pane's layout metadata. The pane's terminal session is looked
/// up by `session_id` in `AppState::terminals`; this struct only owns the
/// geometry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pane {
    /// The session this pane displays. Must match a key in
    /// `AppState::terminals` and an entry in `AppState::sessions`.
    pub session_id: String,
    /// Row index (0-based) in the grid.
    pub row: usize,
    /// Column index (0-based) in the grid.
    pub col: usize,
    /// Freeform window geometry. `None` keeps the pane in its preset grid
    /// cell; the first window-move gesture promotes every pane in the layout
    /// to floating geometry.
    #[serde(default)]
    pub floating: Option<FloatingPane>,
}

/// Axis used by a local split node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SplitAxis {
    /// Children are arranged left and right.
    LeftRight,
    /// Children are arranged top and bottom.
    TopBottom,
}

/// Which side of a target pane receives the newly-created pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SplitDirection {
    Left,
    Right,
    Top,
    Bottom,
}

/// One step in a split-tree path. Paths provide stable splitter identity
/// without exposing or allocating node IDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SplitBranch {
    First,
    Second,
}

pub type SplitPath = Vec<SplitBranch>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
enum SplitNode {
    Leaf {
        pane_idx: usize,
    },
    Split {
        axis: SplitAxis,
        ratio: f64,
        first: Box<SplitNode>,
        second: Box<SplitNode>,
    },
}

/// Geometry for one draggable divider in the split tree.
#[derive(Debug, Clone, PartialEq)]
pub struct SplitterGeometry {
    /// Stable preorder index while the tree structure is unchanged (which it
    /// is throughout one drag gesture).
    pub splitter_idx: usize,
    pub path: SplitPath,
    pub axis: SplitAxis,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub local_extent: f64,
}

/// A multi-pane terminal layout. `panes` retains stable vector indices for
/// focus/session routing, while `root` owns the recursive non-rectangular
/// geometry. The legacy row/column fields remain as a preset compatibility
/// facade; local splits do not depend on their rectangular-grid invariant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PaneLayout {
    /// All panes in stable creation order. Tree leaves reference these
    /// indices; local splits only append and never reorder existing panes.
    pub panes: Vec<Pane>,
    /// Fraction of the container's width taken by each column.
    /// `col_fracs.len()` must equal the number of distinct columns in
    /// `panes`, and the values must sum to ~1.0 (we normalize on read).
    pub col_fracs: Vec<f64>,
    /// Fraction of the container's height taken by each row.
    /// Same contract as `col_fracs`.
    pub row_fracs: Vec<f64>,
    /// Recursive geometry. Layouts are runtime-only today, but the default
    /// keeps old serialized values defensively readable.
    #[serde(default)]
    root: Option<SplitNode>,
    /// If `Some(idx)`, the pane at this index is "zoomed" to fill the
    /// whole container. All other panes are hidden but kept in `panes`
    /// so un-zooming restores the prior layout. This is the fullscreen
    /// /全屏 mode.
    pub zoomed: Option<usize>,
    /// When true, scrolling and keyboard input are broadcast to every
    /// pane (the cross-terminal comparison mode / 跨终端会话的比对模式).
    /// Each pane still owns its own PTY; this flag only changes how the
    /// UI routes events.
    pub comparison: bool,
}

/// Built-in split presets the user can cycle through with a hotkey.
/// 2-split is a single vertical or horizontal divider; 4-split is a 2x2
/// grid; 8-split is a 2x4 grid (2 rows × 4 columns — wider than tall, which
/// matches typical terminal aspect ratios better than 4x2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum LayoutPreset {
    /// One pane fills the container (the legacy single-session view).
    #[default]
    Single,
    /// Two panes side-by-side (1 row × 2 cols).
    Split2H,
    /// Two panes stacked (2 rows × 1 col).
    Split2V,
    /// 2x2 grid (4 panes).
    Grid4,
    /// 2x4 grid (8 panes).
    Grid8,
}

impl LayoutPreset {
    /// Number of panes this preset creates.
    pub fn pane_count(self) -> usize {
        match self {
            LayoutPreset::Single => 1,
            LayoutPreset::Split2H | LayoutPreset::Split2V => 2,
            LayoutPreset::Grid4 => 4,
            LayoutPreset::Grid8 => 8,
        }
    }

    /// (rows, cols) grid dimensions for this preset.
    pub fn grid_dims(self) -> (usize, usize) {
        match self {
            LayoutPreset::Single => (1, 1),
            LayoutPreset::Split2H => (1, 2),
            LayoutPreset::Split2V => (2, 1),
            LayoutPreset::Grid4 => (2, 2),
            LayoutPreset::Grid8 => (2, 4),
        }
    }
}

impl PaneLayout {
    /// Build a layout from a preset and an ordered list of session IDs.
    ///
    /// Extra session IDs are ignored. If fewer are supplied, the remaining
    /// preset leaves are retained as empty drop targets (`session_id = ""`).
    /// Presets are only construction adapters; subsequent growth uses local
    /// tree splits and does not preserve a rectangular-grid invariant.
    pub fn from_preset(preset: LayoutPreset, session_ids: &[String]) -> Self {
        let (rows, cols) = preset.grid_dims();
        let n = rows * cols;
        let mut panes = Vec::with_capacity(n);
        for i in 0..n {
            let row = i / cols;
            let col = i % cols;
            let session_id = session_ids.get(i).cloned().unwrap_or_default();
            panes.push(Pane {
                session_id,
                row,
                col,
                floating: None,
            });
        }
        // Even distribution. We deliberately don't normalize here — the
        // values already sum to 1.0 by construction.
        let col_fracs = vec![1.0 / cols as f64; cols];
        let row_fracs = vec![1.0 / rows as f64; rows];
        let root = grid_split_tree(rows, cols, &col_fracs, &row_fracs);
        Self {
            panes,
            col_fracs,
            row_fracs,
            root,
            zoomed: None,
            comparison: false,
        }
    }

    /// Number of rows in the grid.
    pub fn rows(&self) -> usize {
        self.row_fracs.len().max(1)
    }

    /// Number of columns in the grid.
    pub fn cols(&self) -> usize {
        self.col_fracs.len().max(1)
    }

    /// Find the pane index displaying a given session, if any.
    pub fn pane_index_for_session(&self, session_id: &str) -> Option<usize> {
        self.panes.iter().position(|p| p.session_id == session_id)
    }

    /// True if this layout has more than one visible pane (i.e. the user
    /// is in any split-preset other than Single, and not currently zoomed).
    pub fn is_multi_pane(&self) -> bool {
        self.zoomed.is_none() && self.panes.len() > 1
    }

    /// Whether every pane currently uses freely movable window geometry.
    pub fn is_floating(&self) -> bool {
        !self.panes.is_empty() && self.panes.iter().all(|pane| pane.floating.is_some())
    }

    /// Promote the preset grid to independent floating windows.
    ///
    /// Grid cell centers are preserved, while full-height/full-width panes are
    /// reduced enough to leave movement room on both axes. Calling this again
    /// is a no-op, so existing user positions are never reset.
    pub fn enable_floating(&mut self) -> bool {
        if self.panes.is_empty() {
            return false;
        }
        if self.is_floating() {
            return true;
        }

        let rects: Vec<_> = (0..self.panes.len())
            .map(|idx| {
                self.tree_pane_rect(idx, 1.0, 1.0)
                    .unwrap_or((0.0, 0.0, 1.0, 1.0))
            })
            .collect();
        for (idx, pane) in self.panes.iter_mut().enumerate() {
            let (cell_x, cell_y, cell_w, cell_h) = rects[idx];
            let width = cell_w.clamp(0.32, 0.68);
            let height = cell_h.clamp(0.34, 0.68);
            let x = (cell_x + (cell_w - width) / 2.0).clamp(0.0, 1.0 - width);
            let y = (cell_y + (cell_h - height) / 2.0).clamp(0.0, 1.0 - height);
            pane.floating = Some(FloatingPane {
                x_frac: x,
                y_frac: y,
                width_frac: width,
                height_frac: height,
                z_index: idx as u32 + 1,
            });
        }
        true
    }

    /// Bring a floating pane in front of its siblings without changing its
    /// session assignment or the active tab/layout anchor.
    pub fn bring_floating_pane_to_front(&mut self, pane_idx: usize) -> bool {
        if !self.enable_floating() || pane_idx >= self.panes.len() {
            return false;
        }
        let max_z = self
            .panes
            .iter()
            .filter_map(|pane| pane.floating.map(|geometry| geometry.z_index))
            .max()
            .unwrap_or(0);
        let front_z = if max_z < 90 {
            max_z + 1
        } else {
            // Keep normal moves surgical (only the target changes). Rebase
            // rarely so pane windows never overtake the z=100 comparison
            // banner after many drag operations.
            let mut order: Vec<usize> = (0..self.panes.len())
                .filter(|idx| *idx != pane_idx)
                .collect();
            order.sort_by_key(|idx| {
                self.panes[*idx]
                    .floating
                    .map(|geometry| geometry.z_index)
                    .unwrap_or(0)
            });
            for (position, idx) in order.into_iter().enumerate() {
                if let Some(geometry) = self.panes[idx].floating.as_mut() {
                    geometry.z_index = position as u32 + 1;
                }
            }
            self.panes.len() as u32
        };
        if let Some(geometry) = self.panes[pane_idx].floating.as_mut() {
            geometry.z_index = front_z;
            true
        } else {
            false
        }
    }

    /// Move one floating pane by a CSS-pixel delta, clamped to the terminal
    /// container. Coordinates remain normalized so later container resizes
    /// retain a stable relative arrangement.
    pub fn move_floating_pane(
        &mut self,
        pane_idx: usize,
        delta_x: f64,
        delta_y: f64,
        container_w: f64,
        container_h: f64,
    ) -> bool {
        if !container_w.is_finite()
            || !container_h.is_finite()
            || container_w <= 0.0
            || container_h <= 0.0
            || !delta_x.is_finite()
            || !delta_y.is_finite()
            || !self.enable_floating()
        {
            return false;
        }
        let Some(geometry) = self
            .panes
            .get_mut(pane_idx)
            .and_then(|pane| pane.floating.as_mut())
        else {
            return false;
        };
        geometry.x_frac =
            (geometry.x_frac + delta_x / container_w).clamp(0.0, 1.0 - geometry.width_frac);
        geometry.y_frac =
            (geometry.y_frac + delta_y / container_h).clamp(0.0, 1.0 - geometry.height_frac);
        true
    }

    /// Return the pane's stacking order. Grid panes use their index so the
    /// result is deterministic before floating mode is enabled.
    pub fn pane_z_index(&self, pane_idx: usize) -> Option<u32> {
        let pane = self.panes.get(pane_idx)?;
        Some(
            pane.floating
                .map(|geometry| geometry.z_index)
                .unwrap_or(pane_idx as u32 + 1),
        )
    }

    /// Normalize `col_fracs` and `row_fracs` so each sums to exactly 1.0.
    /// This corrects for floating-point drift from repeated drag
    /// operations (each drag adds/subtracts a small delta, and rounding
    /// error accumulates). After normalization, every pane's actual
    /// size is within `f64::EPSILON` of its intended fraction, so the
    /// renderer never sees a "missing" pixel at the right/bottom edge.
    pub fn normalize(&mut self) {
        normalize_fracs(&mut self.col_fracs);
        normalize_fracs(&mut self.row_fracs);
    }

    /// Adjust the fraction of column `col` by `delta`, stealing width from
    /// or giving width to the adjacent column `col + 1` (if `delta > 0`,
    /// the column grows and its right neighbor shrinks; if `delta < 0`,
    /// the column shrinks and its right neighbor grows).
    ///
    /// Returns `true` if the resize was applied, `false` if it was rejected
    /// because either column would drop below `MIN_PANE_FRAC`.
    ///
    /// Panics if `col` is not an interior column index (i.e. `col + 1`
    /// doesn't exist). The caller (the splitter drag handler) is
    /// responsible for only invoking this on interior columns — there's
    /// no splitter to drag for the last column.
    pub fn resize_col(&mut self, col: usize, delta: f64) -> bool {
        if col + 1 >= self.col_fracs.len() {
            return false;
        }
        let a = self.col_fracs[col] + delta;
        let b = self.col_fracs[col + 1] - delta;
        if a < MIN_PANE_FRAC || b < MIN_PANE_FRAC {
            return false;
        }
        self.col_fracs[col] = a;
        self.col_fracs[col + 1] = b;
        self.root = grid_split_tree(self.rows(), self.cols(), &self.col_fracs, &self.row_fracs);
        true
    }

    /// Adjust the fraction of row `row` by `delta`, stealing from / giving
    /// to row `row + 1`. Same contract as `resize_col`.
    pub fn resize_row(&mut self, row: usize, delta: f64) -> bool {
        if row + 1 >= self.row_fracs.len() {
            return false;
        }
        let a = self.row_fracs[row] + delta;
        let b = self.row_fracs[row + 1] - delta;
        if a < MIN_PANE_FRAC || b < MIN_PANE_FRAC {
            return false;
        }
        self.row_fracs[row] = a;
        self.row_fracs[row + 1] = b;
        self.root = grid_split_tree(self.rows(), self.cols(), &self.col_fracs, &self.row_fracs);
        true
    }

    /// Zoom pane `idx` to fill the whole container. If `idx` is already
    /// zoomed, this is a no-op. Returns the zoomed pane index on success.
    pub fn zoom(&mut self, idx: usize) -> Option<usize> {
        if idx >= self.panes.len() {
            return None;
        }
        self.zoomed = Some(idx);
        Some(idx)
    }

    /// Exit zoom mode, restoring the prior multi-pane layout. Returns
    /// the index of the pane that was zoomed, if any.
    pub fn unzoom(&mut self) -> Option<usize> {
        self.zoomed.take()
    }

    /// Toggle zoom on pane `idx`. If `idx` is currently zoomed, unzooms;
    /// otherwise zooms. Returns the new zoomed state (`Some(idx)` if now
    /// zoomed, `None` if now unzoomed).
    pub fn toggle_zoom(&mut self, idx: usize) -> Option<usize> {
        if self.zoomed == Some(idx) {
            self.unzoom();
            None
        } else {
            self.zoom(idx)
        }
    }

    /// Toggle comparison mode (synchronized scrolling + input broadcast).
    pub fn toggle_comparison(&mut self) -> bool {
        self.comparison = !self.comparison;
        self.comparison
    }

    /// Compute the CSS pixel rectangle for pane `idx` given the container's
    /// pixel dimensions. Returns `(x, y, width, height)` in CSS pixels.
    ///
    /// If the pane is zoomed, returns the full container rectangle.
    /// If the layout is in single-pane mode (only one pane), the pane fills
    /// the whole container.
    ///
    /// This is a pure function over the layout's fracs and the container
    /// size — no DOM access — so it's directly unit-testable.
    pub fn pane_rect(
        &self,
        idx: usize,
        container_w: f64,
        container_h: f64,
    ) -> Option<(f64, f64, f64, f64)> {
        let pane = self.panes.get(idx)?;
        if self.zoomed == Some(idx) || self.panes.len() == 1 {
            return Some((0.0, 0.0, container_w, container_h));
        }
        if self.zoomed.is_some() {
            // Some other pane is zoomed — this one is hidden.
            return None;
        }
        if let Some(geometry) = pane.floating {
            return Some((
                geometry.x_frac * container_w,
                geometry.y_frac * container_h,
                geometry.width_frac * container_w,
                geometry.height_frac * container_h,
            ));
        }
        self.tree_pane_rect(idx, container_w, container_h)
            .or_else(|| {
                // Defensive compatibility for an old value without a split tree.
                let (x, w) = span(&self.col_fracs, pane.col, container_w);
                let (y, h) = span(&self.row_fracs, pane.row, container_h);
                Some((x, y, w, h))
            })
    }

    fn tree_pane_rect(
        &self,
        idx: usize,
        container_w: f64,
        container_h: f64,
    ) -> Option<(f64, f64, f64, f64)> {
        let root = self.root.as_ref()?;
        find_leaf_rect(root, idx, (0.0, 0.0, container_w, container_h))
    }

    /// Split one target pane locally, preserving every existing pane and
    /// adding exactly one empty leaf. The returned index is stable because
    /// panes are only appended, never inserted or reordered.
    pub fn split_pane(&mut self, target_idx: usize, direction: SplitDirection) -> Option<usize> {
        if target_idx >= self.panes.len() || self.panes.len() >= MAX_PANES {
            return None;
        }
        if self.root.is_none() {
            self.root = Some(grid_split_tree(
                self.rows(),
                self.cols(),
                &self.col_fracs,
                &self.row_fracs,
            )?);
        }

        let was_floating = self.is_floating();
        let target_geometry = self.panes[target_idx].floating;
        let new_idx = self.panes.len();
        if !replace_leaf_with_split(self.root.as_mut()?, target_idx, new_idx, direction) {
            return None;
        }

        let floating = if was_floating {
            target_geometry.map(|geometry| {
                let mut created = geometry;
                match direction {
                    SplitDirection::Left => {
                        created.x_frac = (geometry.x_frac - geometry.width_frac * 0.12)
                            .clamp(0.0, 1.0 - geometry.width_frac);
                    }
                    SplitDirection::Right => {
                        created.x_frac = (geometry.x_frac + geometry.width_frac * 0.12)
                            .clamp(0.0, 1.0 - geometry.width_frac);
                    }
                    SplitDirection::Top => {
                        created.y_frac = (geometry.y_frac - geometry.height_frac * 0.12)
                            .clamp(0.0, 1.0 - geometry.height_frac);
                    }
                    SplitDirection::Bottom => {
                        created.y_frac = (geometry.y_frac + geometry.height_frac * 0.12)
                            .clamp(0.0, 1.0 - geometry.height_frac);
                    }
                }
                created.z_index = self
                    .panes
                    .iter()
                    .filter_map(|pane| pane.floating.map(|item| item.z_index))
                    .max()
                    .unwrap_or(0)
                    .saturating_add(1);
                created
            })
        } else {
            None
        };
        let target = &self.panes[target_idx];
        self.panes.push(Pane {
            session_id: String::new(),
            row: target.row,
            col: target.col,
            floating,
        });
        Some(new_idx)
    }

    /// Return every local divider in the tree with its CSS-pixel geometry.
    pub fn splitters(&self, container_w: f64, container_h: f64) -> Vec<SplitterGeometry> {
        let mut out = Vec::new();
        if let Some(root) = self.root.as_ref() {
            collect_splitters(
                root,
                (0.0, 0.0, container_w, container_h),
                &mut Vec::new(),
                &mut out,
            );
        }
        out
    }

    /// Resize one local tree divider by a fraction of that divider's own
    /// parent extent.
    pub fn resize_split(&mut self, splitter_idx: usize, delta: f64) -> bool {
        let Some(node) = self
            .root
            .as_mut()
            .and_then(|root| nth_split_node_mut(root, splitter_idx, &mut 0))
        else {
            return false;
        };
        let SplitNode::Split { ratio, .. } = node else {
            return false;
        };
        let next = *ratio + delta;
        if !next.is_finite() || !(MIN_PANE_FRAC..=1.0 - MIN_PANE_FRAC).contains(&next) {
            return false;
        }
        *ratio = next;
        true
    }

    /// Iterate over `(pane_index, pane, rect)` for every visible pane.
    /// Hidden panes (because another is zoomed) are skipped. Useful for
    /// the renderer to map layout → CSS.
    pub fn visible_panes<'a>(
        &'a self,
        container_w: f64,
        container_h: f64,
    ) -> impl Iterator<Item = (usize, &'a Pane, (f64, f64, f64, f64))> + 'a {
        self.panes.iter().enumerate().filter_map(move |(i, p)| {
            self.pane_rect(i, container_w, container_h)
                .map(|rect| (i, p, rect))
        })
    }

    /// Replace the session displayed in pane `idx`. Used when the user
    /// drag-and-drops a session from the sidebar onto a pane, or when a
    /// pane's session is closed and we substitute the next available one.
    ///
    /// If `session_id` is empty, the pane is "cleared" — the renderer
    /// treats an empty session_id as "no pane here" and renders nothing.
    /// This is how a pane is emptied without shrinking the grid (the grid
    /// invariant `rows * cols == panes.len()` is preserved).
    pub fn set_pane_session(&mut self, idx: usize, session_id: String) -> bool {
        if let Some(p) = self.panes.get_mut(idx) {
            p.session_id = session_id;
            true
        } else {
            false
        }
    }

    pub fn append_pane(&mut self, horizontal: bool) -> Option<usize> {
        if self.panes.len() >= MAX_PANES {
            return None;
        }

        // An empty PaneLayout represents the pre-layout single-pane state.
        // Materialize that implicit pane plus one requested pane.
        if self.panes.is_empty() {
            self.col_fracs = if horizontal {
                vec![0.5, 0.5]
            } else {
                vec![1.0]
            };
            self.row_fracs = if horizontal {
                vec![1.0]
            } else {
                vec![0.5, 0.5]
            };
            self.panes.push(Pane {
                session_id: String::new(),
                row: 0,
                col: 0,
                floating: None,
            });
            self.root = Some(SplitNode::Split {
                axis: if horizontal {
                    SplitAxis::LeftRight
                } else {
                    SplitAxis::TopBottom
                },
                ratio: 0.5,
                first: Box::new(SplitNode::Leaf { pane_idx: 0 }),
                second: Box::new(SplitNode::Leaf { pane_idx: 1 }),
            });
            self.panes.push(Pane {
                session_id: String::new(),
                row: if horizontal { 0 } else { 1 },
                col: if horizontal { 1 } else { 0 },
                floating: None,
            });
            return Some(1);
        }

        // Generic toolbar/hotkey growth splits the largest leaf along the
        // requested axis, avoiding the old forced 1×N strip. Targeted drops
        // call `split_pane` directly and therefore split exactly the leaf under
        // the cursor.
        let target_idx = (0..self.panes.len())
            .filter_map(|idx| {
                self.tree_pane_rect(idx, 1.0, 1.0).map(|rect| {
                    let score = if horizontal { rect.2 } else { rect.3 };
                    (idx, score)
                })
            })
            .max_by(|(left_idx, left), (right_idx, right)| {
                left.total_cmp(right).then_with(|| right_idx.cmp(left_idx))
            })
            .map(|(idx, _)| idx)
            .unwrap_or(0);
        self.split_pane(
            target_idx,
            if horizontal {
                SplitDirection::Right
            } else {
                SplitDirection::Bottom
            },
        )
    }

    /// Add one pane by splitting the largest current leaf along its longest
    /// side. This is the toolbar/hotkey growth policy and avoids degenerating
    /// into a forced 1×N strip as pane count increases.
    pub fn append_balanced(&mut self) -> Option<usize> {
        if self.panes.is_empty() {
            return self.append_pane(true);
        }
        let (target_idx, rect) = (0..self.panes.len())
            .filter_map(|idx| self.tree_pane_rect(idx, 1.0, 1.0).map(|rect| (idx, rect)))
            .max_by(|(left_idx, left), (right_idx, right)| {
                (left.2 * left.3)
                    .total_cmp(&(right.2 * right.3))
                    .then_with(|| right_idx.cmp(left_idx))
            })?;
        self.split_pane(
            target_idx,
            if rect.2 >= rect.3 {
                SplitDirection::Right
            } else {
                SplitDirection::Bottom
            },
        )
    }

    /// Swap the sessions displayed in panes `a` and `b`. Used when the user
    /// drag-and-drops an open session from one pane onto another pane — the
    /// two panes exchange their displayed sessions. Both panes keep their
    /// grid positions; only the `session_id` fields move.
    ///
    /// Returns `true` if the swap was applied, `false` if either index is
    /// out of range (in which case the layout is left untouched).
    ///
    /// # Examples
    ///
    /// ```
    /// use rusterm_ui::layout::{PaneLayout, LayoutPreset};
    ///
    /// let mut layout = PaneLayout::from_preset(
    ///     LayoutPreset::Split2H,
    ///     &["s0".to_string(), "s1".to_string()],
    /// );
    /// assert!(layout.swap_panes(0, 1));
    /// assert_eq!(layout.panes[0].session_id, "s1");
    /// assert_eq!(layout.panes[1].session_id, "s0");
    /// ```
    pub fn swap_panes(&mut self, a: usize, b: usize) -> bool {
        if a == b {
            return true; // No-op swap is trivially successful.
        }
        if a >= self.panes.len() || b >= self.panes.len() {
            return false;
        }
        // Window geometry belongs to the pane slot, not to the session. Swap
        // only the occupants so both grid cells and user-positioned floating
        // windows remain exactly where the user placed them.
        let (left, right) = self.panes.split_at_mut(b.max(a));
        let (pane_a, pane_b) = if a < b {
            (&mut left[a], &mut right[0])
        } else {
            (&mut right[0], &mut left[b])
        };
        std::mem::swap(&mut pane_a.session_id, &mut pane_b.session_id);
        true
    }

    /// Swap the panes displaying sessions `from_session` and `to_session`.
    /// Convenience wrapper around `swap_panes` for the case where the caller
    /// knows session IDs (e.g., the drag source's session_id and the drop
    /// target pane's session_id) rather than pane indices.
    ///
    /// Returns `true` if both sessions were found and swapped. Returns
    /// `false` (and leaves the layout unchanged) if either session is not
    /// currently displayed in any pane.
    pub fn swap_panes_by_session(&mut self, from_session: &str, to_session: &str) -> bool {
        let a = self.pane_index_for_session(from_session);
        let b = self.pane_index_for_session(to_session);
        match (a, b) {
            (Some(a), Some(b)) => self.swap_panes(a, b),
            _ => false,
        }
    }

    /// Get the session IDs of all panes, in row-major order. Empty strings
    /// (slots with no session) are skipped.
    pub fn session_ids(&self) -> Vec<String> {
        self.panes
            .iter()
            .map(|p| p.session_id.clone())
            .filter(|s| !s.is_empty())
            .collect()
    }
}

fn grid_split_tree(
    rows: usize,
    cols: usize,
    col_fracs: &[f64],
    row_fracs: &[f64],
) -> Option<SplitNode> {
    if rows == 0 || cols == 0 {
        return None;
    }
    let mut row_nodes = Vec::with_capacity(rows);
    for row in 0..rows {
        let leaves = (0..cols)
            .map(|col| SplitNode::Leaf {
                pane_idx: row * cols + col,
            })
            .collect::<Vec<_>>();
        row_nodes.push(weighted_split_nodes(
            leaves,
            col_fracs,
            SplitAxis::LeftRight,
        )?);
    }
    weighted_split_nodes(row_nodes, row_fracs, SplitAxis::TopBottom)
}

fn weighted_split_nodes(
    mut nodes: Vec<SplitNode>,
    weights: &[f64],
    axis: SplitAxis,
) -> Option<SplitNode> {
    if nodes.is_empty() {
        return None;
    }
    if nodes.len() == 1 {
        return nodes.pop();
    }
    let first = nodes.remove(0);
    let first_weight = weights.first().copied().unwrap_or(1.0);
    let remaining_weights = if weights.len() > 1 {
        &weights[1..]
    } else {
        &[]
    };
    let remaining_weight = remaining_weights.iter().sum::<f64>();
    let total = first_weight + remaining_weight;
    let ratio = if total.is_finite() && total > 0.0 {
        first_weight / total
    } else {
        1.0 / (nodes.len() + 1) as f64
    };
    let second = weighted_split_nodes(nodes, remaining_weights, axis)?;
    Some(SplitNode::Split {
        axis,
        ratio,
        first: Box::new(first),
        second: Box::new(second),
    })
}

fn split_rect(
    rect: (f64, f64, f64, f64),
    axis: SplitAxis,
    ratio: f64,
) -> ((f64, f64, f64, f64), (f64, f64, f64, f64)) {
    let (x, y, width, height) = rect;
    match axis {
        SplitAxis::LeftRight => {
            let first_width = width * ratio;
            (
                (x, y, first_width, height),
                (x + first_width, y, width - first_width, height),
            )
        }
        SplitAxis::TopBottom => {
            let first_height = height * ratio;
            (
                (x, y, width, first_height),
                (x, y + first_height, width, height - first_height),
            )
        }
    }
}

fn find_leaf_rect(
    node: &SplitNode,
    pane_idx: usize,
    rect: (f64, f64, f64, f64),
) -> Option<(f64, f64, f64, f64)> {
    match node {
        SplitNode::Leaf { pane_idx: idx } => (*idx == pane_idx).then_some(rect),
        SplitNode::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            let (first_rect, second_rect) = split_rect(rect, *axis, *ratio);
            find_leaf_rect(first, pane_idx, first_rect)
                .or_else(|| find_leaf_rect(second, pane_idx, second_rect))
        }
    }
}

fn replace_leaf_with_split(
    node: &mut SplitNode,
    target_idx: usize,
    new_idx: usize,
    direction: SplitDirection,
) -> bool {
    match node {
        SplitNode::Leaf { pane_idx } if *pane_idx == target_idx => {
            let original = SplitNode::Leaf {
                pane_idx: target_idx,
            };
            let created = SplitNode::Leaf { pane_idx: new_idx };
            let (axis, first, second) = match direction {
                SplitDirection::Left => (SplitAxis::LeftRight, created, original),
                SplitDirection::Right => (SplitAxis::LeftRight, original, created),
                SplitDirection::Top => (SplitAxis::TopBottom, created, original),
                SplitDirection::Bottom => (SplitAxis::TopBottom, original, created),
            };
            *node = SplitNode::Split {
                axis,
                ratio: 0.5,
                first: Box::new(first),
                second: Box::new(second),
            };
            true
        }
        SplitNode::Leaf { .. } => false,
        SplitNode::Split { first, second, .. } => {
            replace_leaf_with_split(first, target_idx, new_idx, direction)
                || replace_leaf_with_split(second, target_idx, new_idx, direction)
        }
    }
}

fn collect_splitters(
    node: &SplitNode,
    rect: (f64, f64, f64, f64),
    path: &mut SplitPath,
    out: &mut Vec<SplitterGeometry>,
) {
    let SplitNode::Split {
        axis,
        ratio,
        first,
        second,
    } = node
    else {
        return;
    };
    let (first_rect, second_rect) = split_rect(rect, *axis, *ratio);
    let (x, y, width, height, local_extent) = match axis {
        SplitAxis::LeftRight => (first_rect.0 + first_rect.2, rect.1, 0.0, rect.3, rect.2),
        SplitAxis::TopBottom => (rect.0, first_rect.1 + first_rect.3, rect.2, 0.0, rect.3),
    };
    out.push(SplitterGeometry {
        splitter_idx: out.len(),
        path: path.clone(),
        axis: *axis,
        x,
        y,
        width,
        height,
        local_extent,
    });
    path.push(SplitBranch::First);
    collect_splitters(first, first_rect, path, out);
    path.pop();
    path.push(SplitBranch::Second);
    collect_splitters(second, second_rect, path, out);
    path.pop();
}

fn nth_split_node_mut<'a>(
    node: &'a mut SplitNode,
    wanted: usize,
    next: &mut usize,
) -> Option<&'a mut SplitNode> {
    if !matches!(node, SplitNode::Split { .. }) {
        return None;
    }
    let current = *next;
    *next += 1;
    if current == wanted {
        return Some(node);
    }
    let SplitNode::Split { first, second, .. } = node else {
        unreachable!();
    };
    if let Some(found) = nth_split_node_mut(first, wanted, next) {
        return Some(found);
    }
    nth_split_node_mut(second, wanted, next)
}

/// Compute the pixel offset and size of the `i`-th item in a list of
/// fractions summing to ~1.0, given the container's extent along that axis.
///
/// Returns `(offset, size)`. The offset is the sum of all fractions before
/// index `i` times `total`, and the size is `fracs[i] * total`. The last
/// item's `offset + size` may be slightly less than `total` due to
/// floating-point rounding; the renderer should round up the last item's
/// size to fill the remaining gap (or just allow a 1px gap, which is
/// visually negligible).
fn span(fracs: &[f64], i: usize, total: f64) -> (f64, f64) {
    let offset: f64 = fracs.iter().take(i).sum::<f64>() * total;
    let size = fracs.get(i).copied().unwrap_or(0.0) * total;
    (offset, size)
}

/// Normalize a list of fractions so they sum to exactly 1.0.
///
/// We divide each by the sum and then patch the last entry to absorb the
/// residual floating-point error (so `0.5 + 0.5` doesn't become
/// `0.49999… + 0.50000…` after a drag, which would leave a 1-pixel gap at
/// the right/bottom edge of the container).
fn normalize_fracs(fracs: &mut [f64]) {
    let sum: f64 = fracs.iter().sum();
    if sum <= 0.0 || !sum.is_finite() {
        // Degenerate — fall back to equal distribution.
        let n = fracs.len() as f64;
        let each = if n > 0.0 { 1.0 / n } else { 0.0 };
        for f in fracs.iter_mut() {
            *f = each;
        }
        return;
    }
    for f in fracs.iter_mut() {
        *f /= sum;
    }
    // Absorb residual into the last entry so the sum is exactly 1.0.
    // Compute the residual BEFORE taking `last_mut` to satisfy the
    // borrow checker (can't iterate `fracs` while `last_mut` is held).
    let residual = 1.0 - fracs.iter().sum::<f64>();
    if let Some(last) = fracs.last_mut() {
        *last += residual;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: make N session IDs like "s0", "s1", …
    fn sids(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("s{i}")).collect()
    }

    // ------------------------------------------------------------------
    // Preset construction
    // ------------------------------------------------------------------

    #[test]
    fn single_preset_makes_one_pane_filling_container() {
        let layout = PaneLayout::from_preset(LayoutPreset::Single, &sids(1));
        assert_eq!(layout.panes.len(), 1);
        assert_eq!(layout.col_fracs, vec![1.0]);
        assert_eq!(layout.row_fracs, vec![1.0]);
        assert!(!layout.is_multi_pane());
        let rect = layout.pane_rect(0, 1000.0, 800.0).unwrap();
        assert_eq!(rect, (0.0, 0.0, 1000.0, 800.0));
    }

    #[test]
    fn split2h_makes_two_side_by_side_panes() {
        let layout = PaneLayout::from_preset(LayoutPreset::Split2H, &sids(2));
        assert_eq!(layout.panes.len(), 2);
        assert_eq!(layout.rows(), 1);
        assert_eq!(layout.cols(), 2);
        assert!(layout.is_multi_pane());
        // Each column gets half the width.
        let r0 = layout.pane_rect(0, 1000.0, 800.0).unwrap();
        let r1 = layout.pane_rect(1, 1000.0, 800.0).unwrap();
        assert_eq!(r0, (0.0, 0.0, 500.0, 800.0));
        assert_eq!(r1, (500.0, 0.0, 500.0, 800.0));
    }

    #[test]
    fn split2v_makes_two_stacked_panes() {
        let layout = PaneLayout::from_preset(LayoutPreset::Split2V, &sids(2));
        assert_eq!(layout.rows(), 2);
        assert_eq!(layout.cols(), 1);
        let r0 = layout.pane_rect(0, 1000.0, 800.0).unwrap();
        let r1 = layout.pane_rect(1, 1000.0, 800.0).unwrap();
        assert_eq!(r0, (0.0, 0.0, 1000.0, 400.0));
        assert_eq!(r1, (0.0, 400.0, 1000.0, 400.0));
    }

    #[test]
    fn grid4_makes_2x2_grid() {
        let layout = PaneLayout::from_preset(LayoutPreset::Grid4, &sids(4));
        assert_eq!(layout.rows(), 2);
        assert_eq!(layout.cols(), 2);
        // Row-major ordering: (0,0), (0,1), (1,0), (1,1).
        assert_eq!(layout.panes[0].row, 0);
        assert_eq!(layout.panes[0].col, 0);
        assert_eq!(layout.panes[1].row, 0);
        assert_eq!(layout.panes[1].col, 1);
        assert_eq!(layout.panes[2].row, 1);
        assert_eq!(layout.panes[2].col, 0);
        assert_eq!(layout.panes[3].row, 1);
        assert_eq!(layout.panes[3].col, 1);
        let r00 = layout.pane_rect(0, 1000.0, 800.0).unwrap();
        let r11 = layout.pane_rect(3, 1000.0, 800.0).unwrap();
        assert_eq!(r00, (0.0, 0.0, 500.0, 400.0));
        assert_eq!(r11, (500.0, 400.0, 500.0, 400.0));
    }

    #[test]
    fn grid8_makes_2x4_grid() {
        let layout = PaneLayout::from_preset(LayoutPreset::Grid8, &sids(8));
        assert_eq!(layout.rows(), 2);
        assert_eq!(layout.cols(), 4);
        assert_eq!(layout.panes.len(), 8);
        // Each pane should be 250 wide, 400 tall.
        let r = layout.pane_rect(5, 1000.0, 800.0).unwrap();
        // Pane 5 = row 1, col 1.
        assert_eq!(r, (250.0, 400.0, 250.0, 400.0));
    }

    // ------------------------------------------------------------------
    // Resize
    // ------------------------------------------------------------------

    #[test]
    fn resize_col_grows_left_shrinks_right() {
        let mut layout = PaneLayout::from_preset(LayoutPreset::Split2H, &sids(2));
        assert!(layout.resize_col(0, 0.1)); // +10% to col 0, -10% from col 1.
        assert!((layout.col_fracs[0] - 0.6).abs() < 1e-9);
        assert!((layout.col_fracs[1] - 0.4).abs() < 1e-9);
        let r0 = layout.pane_rect(0, 1000.0, 800.0).unwrap();
        let r1 = layout.pane_rect(1, 1000.0, 800.0).unwrap();
        assert!((r0.2 - 600.0).abs() < 1e-6);
        assert!((r1.2 - 400.0).abs() < 1e-6);
    }

    #[test]
    fn resize_col_rejects_below_minimum() {
        let mut layout = PaneLayout::from_preset(LayoutPreset::Split2H, &sids(2));
        // Try to shrink col 0 to 0 — should be rejected because
        // MIN_PANE_FRAC = 0.1.
        assert!(!layout.resize_col(0, -0.5));
        // Fracs unchanged.
        assert!((layout.col_fracs[0] - 0.5).abs() < 1e-9);
        assert!((layout.col_fracs[1] - 0.5).abs() < 1e-9);
    }

    #[test]
    fn resize_col_rejects_invalid_index() {
        let mut layout = PaneLayout::from_preset(LayoutPreset::Split2H, &sids(2));
        // Only col 0 has a right neighbor (col 1) — resizing col 1 is invalid.
        assert!(!layout.resize_col(1, 0.1));
    }

    #[test]
    fn resize_row_grows_top_shrinks_bottom() {
        let mut layout = PaneLayout::from_preset(LayoutPreset::Split2V, &sids(2));
        assert!(layout.resize_row(0, 0.2));
        assert!((layout.row_fracs[0] - 0.7).abs() < 1e-9);
        assert!((layout.row_fracs[1] - 0.3).abs() < 1e-9);
    }

    #[test]
    fn resize_row_rejects_below_minimum() {
        let mut layout = PaneLayout::from_preset(LayoutPreset::Split2V, &sids(2));
        assert!(!layout.resize_row(0, -0.5));
    }

    // ------------------------------------------------------------------
    // Zoom (fullscreen)
    // ------------------------------------------------------------------

    #[test]
    fn zoom_returns_full_container_rect() {
        let mut layout = PaneLayout::from_preset(LayoutPreset::Grid4, &sids(4));
        layout.zoom(2);
        let r = layout.pane_rect(2, 1000.0, 800.0).unwrap();
        assert_eq!(r, (0.0, 0.0, 1000.0, 800.0));
    }

    #[test]
    fn zoom_hides_other_panes() {
        let mut layout = PaneLayout::from_preset(LayoutPreset::Grid4, &sids(4));
        layout.zoom(0);
        assert!(layout.pane_rect(1, 1000.0, 800.0).is_none());
        assert!(layout.pane_rect(2, 1000.0, 800.0).is_none());
        assert!(layout.pane_rect(3, 1000.0, 800.0).is_none());
    }

    #[test]
    fn unzoom_restores_prior_layout() {
        let mut layout = PaneLayout::from_preset(LayoutPreset::Grid4, &sids(4));
        // Pre-distort the fracs so we can verify unzoom restores them
        // exactly (the fracs are not modified by zoom/unzoom).
        layout.resize_col(0, 0.1); // col 0 = 0.6, col 1 = 0.4
        let fracs_before = layout.col_fracs.clone();
        layout.zoom(1);
        layout.unzoom();
        assert_eq!(layout.col_fracs, fracs_before);
        // And the panes are back to their grid positions. Pane 1 is at
        // (row=0, col=1): x = 600 (sum of col 0's frac = 0.6 * 1000),
        // width = 0.4 * 1000 = 400.
        let r1 = layout.pane_rect(1, 1000.0, 800.0).unwrap();
        assert_eq!(r1, (600.0, 0.0, 400.0, 400.0));
    }

    #[test]
    fn toggle_zoom_cycles_on_off() {
        let mut layout = PaneLayout::from_preset(LayoutPreset::Split2H, &sids(2));
        assert_eq!(layout.toggle_zoom(0), Some(0));
        assert!(layout.zoomed == Some(0));
        assert_eq!(layout.toggle_zoom(0), None);
        assert!(layout.zoomed.is_none());
    }

    #[test]
    fn toggle_zoom_switches_between_panes() {
        let mut layout = PaneLayout::from_preset(LayoutPreset::Grid4, &sids(4));
        assert_eq!(layout.toggle_zoom(0), Some(0));
        assert_eq!(layout.toggle_zoom(1), Some(1));
        assert_eq!(layout.zoomed, Some(1));
    }

    #[test]
    fn zoom_rejects_out_of_range_index() {
        let mut layout = PaneLayout::from_preset(LayoutPreset::Single, &sids(1));
        assert!(layout.zoom(5).is_none());
    }

    // ------------------------------------------------------------------
    // Comparison mode
    // ------------------------------------------------------------------

    #[test]
    fn toggle_comparison_flips_flag() {
        let mut layout = PaneLayout::from_preset(LayoutPreset::Split2H, &sids(2));
        assert!(!layout.comparison);
        assert!(layout.toggle_comparison());
        assert!(layout.comparison);
        assert!(!layout.toggle_comparison());
        assert!(!layout.comparison);
    }

    // ------------------------------------------------------------------
    // Normalization
    // ------------------------------------------------------------------

    #[test]
    fn normalize_fixes_drift_to_exactly_one() {
        let mut layout = PaneLayout::from_preset(LayoutPreset::Grid4, &sids(4));
        // Drag the column splitter around — accumulated floating-point
        // drift should be cleaned up by normalize().
        for _ in 0..10 {
            layout.resize_col(0, 0.01);
        }
        layout.normalize();
        let sum: f64 = layout.col_fracs.iter().sum();
        assert!((sum - 1.0).abs() < 1e-9);
        // Each column still ≥ MIN_PANE_FRAC.
        for &f in &layout.col_fracs {
            assert!(f >= MIN_PANE_FRAC);
        }
    }

    #[test]
    fn normalize_handles_degenerate_zero_sum() {
        let mut fracs = vec![0.0, 0.0];
        normalize_fracs(&mut fracs);
        assert!((fracs[0] - 0.5).abs() < 1e-9);
        assert!((fracs[1] - 0.5).abs() < 1e-9);
    }

    // ------------------------------------------------------------------
    // Pane lookup / session mapping
    // ------------------------------------------------------------------

    #[test]
    fn pane_index_for_session_finds_pane() {
        let layout = PaneLayout::from_preset(LayoutPreset::Grid4, &sids(4));
        assert_eq!(layout.pane_index_for_session("s0"), Some(0));
        assert_eq!(layout.pane_index_for_session("s3"), Some(3));
        assert_eq!(layout.pane_index_for_session("s4"), None);
    }

    #[test]
    fn session_ids_returns_all_non_empty_in_order() {
        let mut layout = PaneLayout::from_preset(LayoutPreset::Grid4, &sids(4));
        assert_eq!(layout.session_ids(), vec!["s0", "s1", "s2", "s3"]);
        // Clear one pane's session.
        layout.set_pane_session(2, String::new());
        assert_eq!(layout.session_ids(), vec!["s0", "s1", "s3"]);
    }

    #[test]
    fn set_pane_session_replaces_session() {
        let mut layout = PaneLayout::from_preset(LayoutPreset::Split2H, &sids(2));
        assert!(layout.set_pane_session(1, "new-session".to_string()));
        assert_eq!(layout.panes[1].session_id, "new-session");
        // Out-of-range index returns false.
        assert!(!layout.set_pane_session(99, "x".to_string()));
    }

    // ------------------------------------------------------------------
    // append_pane (on-demand split, +1 pane per call)
    // ------------------------------------------------------------------

    #[test]
    fn splitting_target_pane_at_bottom_adds_one_local_leaf() {
        let mut layout = PaneLayout::from_preset(LayoutPreset::Split2H, &sids(2));

        let new_idx = layout
            .split_pane(1, SplitDirection::Bottom)
            .expect("target pane splits");

        assert_eq!(new_idx, 2);
        assert_eq!(layout.panes.len(), 3);
        assert_eq!(layout.panes[0].session_id, "s0");
        assert_eq!(layout.panes[1].session_id, "s1");
        assert_eq!(layout.panes[2].session_id, "");
        assert_eq!(
            layout.pane_rect(0, 1000.0, 800.0),
            Some((0.0, 0.0, 500.0, 800.0))
        );
        assert_eq!(
            layout.pane_rect(1, 1000.0, 800.0),
            Some((500.0, 0.0, 500.0, 400.0))
        );
        assert_eq!(
            layout.pane_rect(2, 1000.0, 800.0),
            Some((500.0, 400.0, 500.0, 400.0))
        );
    }

    #[test]
    fn splitting_target_pane_at_right_adds_one_local_leaf() {
        let mut layout = PaneLayout::from_preset(LayoutPreset::Split2H, &sids(2));

        let new_idx = layout
            .split_pane(0, SplitDirection::Right)
            .expect("target pane splits at right");

        assert_eq!(new_idx, 2);
        assert_eq!(layout.panes.len(), 3);
        assert_eq!(layout.panes[0].session_id, "s0");
        assert_eq!(layout.panes[1].session_id, "s1");
        assert_eq!(layout.panes[2].session_id, "");
        // Pane 0 split horizontally at ratio 0.5: original stays on the
        // left half (250px), new pane appears on the right half (250px).
        // Pane 1 (originally at x=500) is unchanged (500px wide).
        assert_eq!(
            layout.pane_rect(0, 1000.0, 800.0),
            Some((0.0, 0.0, 250.0, 800.0))
        );
        assert_eq!(
            layout.pane_rect(2, 1000.0, 800.0),
            Some((250.0, 0.0, 250.0, 800.0))
        );
        assert_eq!(
            layout.pane_rect(1, 1000.0, 800.0),
            Some((500.0, 0.0, 500.0, 800.0))
        );
    }

    #[test]
    fn append_pane_horizontal_to_empty_creates_1x2() {
        let mut layout = PaneLayout::default();
        assert!(layout.panes.is_empty());
        let new_idx = layout.append_pane(true).expect("empty layout grows");
        assert_eq!(layout.panes.len(), 2);
        assert_eq!(layout.rows(), 1);
        assert_eq!(layout.cols(), 2);
        // The second pane is the "new" one returned.
        assert_eq!(new_idx, 1);
        // Both new panes are empty (caller fills them).
        assert_eq!(layout.panes[0].session_id, "");
        assert_eq!(layout.panes[1].session_id, "");
        // Each column gets half the width.
        let r0 = layout.pane_rect(0, 1000.0, 800.0).unwrap();
        let r1 = layout.pane_rect(1, 1000.0, 800.0).unwrap();
        assert_eq!(r0, (0.0, 0.0, 500.0, 800.0));
        assert_eq!(r1, (500.0, 0.0, 500.0, 800.0));
    }

    #[test]
    fn append_pane_horizontal_to_1x2_creates_1x3_with_one_new_pane() {
        let mut layout = PaneLayout::from_preset(LayoutPreset::Split2H, &sids(2));
        let before_len = layout.panes.len();
        assert_eq!(before_len, 2);
        let new_idx = layout.append_pane(true).expect("append to Split2H");
        // Exactly ONE pane was added (not Grid4's +2).
        assert_eq!(layout.panes.len(), 3);
        assert_eq!(new_idx, 2);
        // Tree growth no longer manufactures a global third column.
        assert_eq!(layout.rows(), 1);
        assert_eq!(layout.cols(), 2);
        assert_eq!(layout.panes[2].session_id, "");
        // Existing sessions are preserved.
        assert_eq!(layout.panes[0].session_id, "s0");
        assert_eq!(layout.panes[1].session_id, "s1");
        // The largest leaf is split locally; the other half stays unchanged.
        let r0 = layout.pane_rect(0, 900.0, 800.0).unwrap();
        let r1 = layout.pane_rect(1, 900.0, 800.0).unwrap();
        let r2 = layout.pane_rect(2, 900.0, 800.0).unwrap();
        assert_eq!(r0, (0.0, 0.0, 225.0, 800.0));
        assert_eq!(r2, (225.0, 0.0, 225.0, 800.0));
        assert_eq!(r1, (450.0, 0.0, 450.0, 800.0));
    }

    #[test]
    fn append_pane_vertical_to_2x1_creates_3x1_with_one_new_pane() {
        let mut layout = PaneLayout::from_preset(LayoutPreset::Split2V, &sids(2));
        assert_eq!(layout.rows(), 2);
        assert_eq!(layout.cols(), 1);
        let new_idx = layout.append_pane(false).expect("append to Split2V");
        assert_eq!(layout.panes.len(), 3);
        assert_eq!(layout.rows(), 2);
        assert_eq!(layout.cols(), 1);
        assert_eq!(new_idx, 2);
        assert_eq!(layout.panes[2].session_id, "");
        let r0 = layout.pane_rect(0, 1000.0, 900.0).unwrap();
        let r1 = layout.pane_rect(1, 1000.0, 900.0).unwrap();
        let r2 = layout.pane_rect(2, 1000.0, 900.0).unwrap();
        assert_eq!(r0, (0.0, 0.0, 1000.0, 225.0));
        assert_eq!(r2, (0.0, 225.0, 1000.0, 225.0));
        assert_eq!(r1, (0.0, 450.0, 1000.0, 450.0));
    }

    #[test]
    fn five_balanced_panes_are_not_forced_into_one_horizontal_strip() {
        let mut layout = PaneLayout::from_preset(LayoutPreset::Single, &["s0".to_string()]);
        for idx in 1..5 {
            let new_idx = layout.append_balanced().expect("balanced growth");
            assert_eq!(new_idx, idx);
            assert!(layout.set_pane_session(new_idx, format!("s{idx}")));
        }

        assert_eq!(layout.panes.len(), 5);
        assert_eq!(layout.session_ids().len(), 5);
        let rects: Vec<_> = (0..5)
            .map(|idx| layout.pane_rect(idx, 1000.0, 800.0).unwrap())
            .collect();
        assert!(rects.iter().any(|rect| rect.3 < 800.0));
        assert!(rects.iter().any(|rect| rect.1 > 0.0));
    }

    #[test]
    fn local_splitter_only_resizes_its_two_target_halves() {
        let mut layout = PaneLayout::from_preset(LayoutPreset::Split2H, &sids(2));
        let new_idx = layout
            .split_pane(1, SplitDirection::Bottom)
            .expect("local split");
        assert_eq!(new_idx, 2);

        let splitters = layout.splitters(1000.0, 800.0);
        assert_eq!(splitters.len(), 2);
        let local = splitters
            .iter()
            .find(|splitter| splitter.axis == SplitAxis::TopBottom)
            .expect("local horizontal divider");
        assert_eq!((local.x, local.y, local.width), (500.0, 400.0, 500.0));
        assert_eq!(local.local_extent, 800.0);

        assert!(layout.resize_split(local.splitter_idx, 0.1));
        assert_eq!(
            layout.pane_rect(0, 1000.0, 800.0),
            Some((0.0, 0.0, 500.0, 800.0))
        );
        assert_eq!(
            layout.pane_rect(1, 1000.0, 800.0),
            Some((500.0, 0.0, 500.0, 480.0))
        );
        assert_eq!(
            layout.pane_rect(2, 1000.0, 800.0),
            Some((500.0, 480.0, 500.0, 320.0))
        );
    }

    #[test]
    fn append_pane_at_max_returns_none() {
        // Build a layout at MAX_PANES by appending repeatedly from a Split2H
        // base (1 row). MAX_PANES = 16, so we should be able to grow to 16
        // then fail on the 17th attempt.
        let mut layout = PaneLayout::from_preset(LayoutPreset::Split2H, &sids(2));
        while layout.panes.len() < MAX_PANES {
            assert!(layout.append_pane(true).is_some(), "append should succeed");
        }
        assert_eq!(layout.panes.len(), MAX_PANES);
        // Now at the cap — append must fail.
        assert!(layout.append_pane(true).is_none());
    }

    #[test]
    fn append_pane_splits_the_largest_leaf_without_resetting_user_ratio() {
        // Start with a 1×2 layout where the user has dragged the splitter so
        // pane 0 is wider (0.7) than pane 1 (0.3). Growth splits that largest
        // leaf instead of resetting the whole layout.
        let mut layout = PaneLayout::from_preset(LayoutPreset::Split2H, &sids(2));
        assert!(layout.resize_col(0, 0.2)); // 0.5→0.7, 0.5→0.3
        assert!((layout.col_fracs[0] - 0.7).abs() < 1e-9);
        assert!((layout.col_fracs[1] - 0.3).abs() < 1e-9);
        let new_idx = layout.append_pane(true).expect("append");
        assert_eq!(new_idx, 2);
        assert_eq!(layout.col_fracs, vec![0.7, 0.3]);
        assert_eq!(
            layout.pane_rect(0, 1000.0, 800.0),
            Some((0.0, 0.0, 350.0, 800.0))
        );
        assert_eq!(
            layout.pane_rect(2, 1000.0, 800.0),
            Some((350.0, 0.0, 350.0, 800.0))
        );
        assert_eq!(
            layout.pane_rect(1, 1000.0, 800.0),
            Some((700.0, 0.0, 300.0, 800.0))
        );
    }

    #[test]
    fn append_pane_to_legacy_grid4_locally_splits_one_cell() {
        let mut layout = PaneLayout::from_preset(LayoutPreset::Grid4, &sids(4));
        let original_sessions = layout.session_ids();

        let new_idx = layout.append_pane(true).expect("append to Grid4");

        assert_eq!(new_idx, 4);
        assert_eq!(layout.panes.len(), 5);
        assert_eq!(layout.rows(), 2);
        assert_eq!(layout.cols(), 2);
        assert_eq!(&layout.session_ids()[..4], original_sessions.as_slice());
        assert_eq!(layout.panes[4].session_id, "");
    }

    // ------------------------------------------------------------------
    // Task 16 — pane swap (drag-and-drop rearrangement)
    // ------------------------------------------------------------------

    /// Swapping two panes exchanges their `session_id` fields but leaves
    /// each pane's grid position (row/col) where it was. After the swap,
    /// the session that was at pane 0 is now at pane 1 and vice versa,
    /// but both panes still occupy their original cells in the grid.
    /// This is the core invariant of the drag-and-drop rearrange feature.
    #[test]
    fn swap_panes_exchanges_sessions_in_place() {
        let mut layout = PaneLayout::from_preset(LayoutPreset::Split2H, &sids(2));
        // Before: pane[0]=s0 at (0,0), pane[1]=s1 at (0,1).
        assert_eq!(layout.panes[0].session_id, "s0");
        assert_eq!(layout.panes[1].session_id, "s1");
        assert!(layout.swap_panes(0, 1));
        // After: pane[0]=s1 at (0,0), pane[1]=s0 at (0,1).
        assert_eq!(layout.panes[0].session_id, "s1");
        assert_eq!(layout.panes[1].session_id, "s0");
        // Grid positions are unchanged — pane 0 still claims (0,0), etc.
        assert_eq!(layout.panes[0].row, 0);
        assert_eq!(layout.panes[0].col, 0);
        assert_eq!(layout.panes[1].row, 0);
        assert_eq!(layout.panes[1].col, 1);
    }

    /// Swapping a pane with itself is a no-op (and trivially successful).
    /// This avoids a needless Vec::swap panic on `swap(a, a)` and keeps
    /// the drop handler simple — it can call `swap_panes(i, i)` without
    /// first checking `i == j`.
    #[test]
    fn swap_panes_with_self_is_noop() {
        let mut layout = PaneLayout::from_preset(LayoutPreset::Split2H, &sids(2));
        let before = layout.clone();
        assert!(layout.swap_panes(0, 0));
        assert_eq!(layout, before);
    }

    /// Swapping panes with an out-of-range index is rejected (no mutation).
    /// The drop handler may compute a pane index that's stale (e.g., the
    /// layout was just rebuilt by a concurrent cycle-preset); in that case
    /// the swap must fail silently rather than panic.
    #[test]
    fn swap_panes_rejects_out_of_range() {
        let mut layout = PaneLayout::from_preset(LayoutPreset::Split2H, &sids(2));
        let before = layout.clone();
        assert!(!layout.swap_panes(0, 99));
        assert!(!layout.swap_panes(99, 0));
        assert_eq!(layout, before);
    }

    /// Swap-by-session finds the panes displaying each session and swaps
    /// them. This is the wrapper the drag handler uses — it knows the
    /// source session (the tab being dragged) and the target pane's
    /// session (the pane being dropped onto), not their indices.
    #[test]
    fn swap_panes_by_session_swaps_correct_panes() {
        let mut layout = PaneLayout::from_preset(LayoutPreset::Grid4, &sids(4));
        // Before: s0 at pane 0, s2 at pane 2.
        assert!(layout.swap_panes_by_session("s0", "s2"));
        // After: s0 at pane 2, s2 at pane 0.
        assert_eq!(layout.panes[0].session_id, "s2");
        assert_eq!(layout.panes[2].session_id, "s0");
    }

    /// If either session isn't currently displayed in any pane, the swap
    /// is rejected. This covers the case where the user drags a session
    /// that was closed mid-drag, or drops onto a pane that has just been
    /// cleared — the swap must not silently no-op (which would leave the
    /// dragged session nowhere) but instead return false so the caller
    /// can fall back to `set_pane_session`.
    #[test]
    fn swap_panes_by_session_rejects_missing_session() {
        let mut layout = PaneLayout::from_preset(LayoutPreset::Split2H, &sids(2));
        let before = layout.clone();
        // "s9" doesn't exist in any pane.
        assert!(!layout.swap_panes_by_session("s0", "s9"));
        assert!(!layout.swap_panes_by_session("s9", "s0"));
        assert_eq!(layout, before);
    }

    /// After a swap, `pane_rect` still returns the same rectangles for
    /// each pane index — only the session displayed at that rect changed.
    /// This is what the renderer relies on: DOM keys (`pane-{idx}-*`)
    /// stay stable, so Dioxus's reconciler doesn't re-mount any
    /// TerminalView (which would blow away scrollback).
    #[test]
    fn swap_panes_preserves_pane_rects() {
        let mut layout = PaneLayout::from_preset(LayoutPreset::Grid4, &sids(4));
        let r0_before = layout.pane_rect(0, 1000.0, 800.0).unwrap();
        let r2_before = layout.pane_rect(2, 1000.0, 800.0).unwrap();
        layout.swap_panes(0, 2);
        let r0_after = layout.pane_rect(0, 1000.0, 800.0).unwrap();
        let r2_after = layout.pane_rect(2, 1000.0, 800.0).unwrap();
        assert_eq!(r0_before, r0_after);
        assert_eq!(r2_before, r2_after);
    }

    /// Algebraic invariant: any swap is its own inverse. Swapping the
    /// same two panes twice restores the original layout. This is what
    /// makes drag-and-drop rearrangement always undoable — the user can
    /// drag a session back to its original pane to revert a drag.
    ///
    /// More generally, swaps generate the full symmetric group on the
    /// panes, so ANY permutation of sessions across panes can be
    /// expressed as a sequence of `swap_panes` calls. This test pins
    /// the self-inverse property as the foundation of that group
    /// structure.
    #[test]
    fn swap_panes_is_self_inverse() {
        let mut layout = PaneLayout::from_preset(LayoutPreset::Grid4, &sids(4));
        let original = layout.clone();
        // Swap panes 0 and 2, then swap them again — should be identity.
        layout.swap_panes(0, 2);
        layout.swap_panes(0, 2);
        assert_eq!(layout, original);
        // Same for a non-symmetric pair (1, 3).
        layout.swap_panes(1, 3);
        layout.swap_panes(1, 3);
        assert_eq!(layout, original);
    }

    /// A sequence of swaps can express any permutation of panes. This
    /// test verifies that a 3-step rearrangement (which moves every
    /// session to a different pane) leaves no session lost or
    /// duplicated — the grid invariant (4 panes, 4 distinct sessions)
    /// is preserved through arbitrary rearrangements.
    #[test]
    fn swap_panes_arbitrary_rearrangement_preserves_sessions() {
        let mut layout = PaneLayout::from_preset(LayoutPreset::Grid4, &sids(4));
        // Trace the swaps step by step:
        //   start:         [s0, s1, s2, s3]
        //   swap(0,1):     [s1, s0, s2, s3]
        //   swap(2,3):     [s1, s0, s3, s2]
        //   swap(0,2):     [s3, s0, s1, s2]
        // Final: pane 0=s3, pane 1=s0, pane 2=s1, pane 3=s2.
        layout.swap_panes(0, 1);
        layout.swap_panes(2, 3);
        layout.swap_panes(0, 2);
        // All 4 sessions still present (no session lost or duplicated).
        let mut sessions: Vec<String> = layout.panes.iter().map(|p| p.session_id.clone()).collect();
        sessions.sort();
        assert_eq!(sessions, vec!["s0", "s1", "s2", "s3"]);
        // The specific arrangement matches the traced permutation.
        assert_eq!(layout.panes[0].session_id, "s3");
        assert_eq!(layout.panes[1].session_id, "s0");
        assert_eq!(layout.panes[2].session_id, "s1");
        assert_eq!(layout.panes[3].session_id, "s2");
        // Grid positions are still tied to pane indices (the row/col
        // fields reflect the index, not the session — this is what
        // `pane_rect` relies on).
        assert_eq!(layout.panes[0].row, 0);
        assert_eq!(layout.panes[0].col, 0);
        assert_eq!(layout.panes[3].row, 1);
        assert_eq!(layout.panes[3].col, 1);
    }

    /// `set_pane_session` with an empty string clears the pane — the
    /// session_id becomes "" and `session_ids()` skips it. The grid
    /// invariant (`rows * cols == panes.len()`) is preserved because
    /// the pane entry itself isn't removed, only its session_id is
    /// blanked. This is the contract the "drag-to-clear" feature relies
    /// on (e.g., dropping a "no session" placeholder onto a pane).
    #[test]
    fn set_pane_session_with_empty_string_clears_pane() {
        let mut layout = PaneLayout::from_preset(LayoutPreset::Grid4, &sids(4));
        assert!(layout.set_pane_session(2, String::new()));
        assert_eq!(layout.panes[2].session_id, "");
        assert_eq!(layout.session_ids(), vec!["s0", "s1", "s3"]);
        // The pane entry is still there — grid invariant preserved.
        assert_eq!(layout.panes.len(), 4);
        assert_eq!(layout.panes[2].row, 1);
        assert_eq!(layout.panes[2].col, 0);
    }

    // ------------------------------------------------------------------
    // visible_panes iterator
    // ------------------------------------------------------------------

    #[test]
    fn visible_panes_yields_all_when_not_zoomed() {
        let layout = PaneLayout::from_preset(LayoutPreset::Grid4, &sids(4));
        let visible: Vec<_> = layout.visible_panes(1000.0, 800.0).collect();
        assert_eq!(visible.len(), 4);
        // Each entry has a non-zero rect.
        for (_, _, rect) in &visible {
            assert!(rect.2 > 0.0 && rect.3 > 0.0);
        }
    }

    #[test]
    fn visible_panes_yields_only_zoomed_pane() {
        let mut layout = PaneLayout::from_preset(LayoutPreset::Grid4, &sids(4));
        layout.zoom(2);
        let visible: Vec<_> = layout.visible_panes(1000.0, 800.0).collect();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].0, 2);
        let (_, _, rect) = visible[0];
        assert_eq!(rect, (0.0, 0.0, 1000.0, 800.0));
    }

    // ------------------------------------------------------------------
    // Preset metadata
    // ------------------------------------------------------------------

    #[test]
    fn preset_pane_counts() {
        assert_eq!(LayoutPreset::Single.pane_count(), 1);
        assert_eq!(LayoutPreset::Split2H.pane_count(), 2);
        assert_eq!(LayoutPreset::Split2V.pane_count(), 2);
        assert_eq!(LayoutPreset::Grid4.pane_count(), 4);
        assert_eq!(LayoutPreset::Grid8.pane_count(), 8);
    }

    #[test]
    fn preset_grid_dims() {
        assert_eq!(LayoutPreset::Grid4.grid_dims(), (2, 2));
        assert_eq!(LayoutPreset::Grid8.grid_dims(), (2, 4));
    }

    // ------------------------------------------------------------------
    // Edge cases
    // ------------------------------------------------------------------

    #[test]
    fn from_preset_with_too_few_sessions_leaves_empty_slots() {
        // Only 2 sessions for a 4-pane grid — the last 2 panes should be
        // empty (session_id = "").
        let layout = PaneLayout::from_preset(LayoutPreset::Grid4, &sids(2));
        assert_eq!(layout.panes.len(), 4);
        assert_eq!(layout.panes[0].session_id, "s0");
        assert_eq!(layout.panes[1].session_id, "s1");
        assert_eq!(layout.panes[2].session_id, "");
        assert_eq!(layout.panes[3].session_id, "");
        // session_ids() skips the empties.
        assert_eq!(layout.session_ids(), vec!["s0", "s1"]);
    }

    #[test]
    fn from_preset_with_extra_sessions_ignores_extras() {
        let layout = PaneLayout::from_preset(LayoutPreset::Single, &sids(5));
        assert_eq!(layout.panes.len(), 1);
        assert_eq!(layout.panes[0].session_id, "s0");
    }

    #[test]
    fn is_multi_pane_false_for_single() {
        let layout = PaneLayout::from_preset(LayoutPreset::Single, &sids(1));
        assert!(!layout.is_multi_pane());
    }

    #[test]
    fn is_multi_pane_false_when_zoomed_even_in_split() {
        let mut layout = PaneLayout::from_preset(LayoutPreset::Split2H, &sids(2));
        assert!(layout.is_multi_pane());
        layout.zoom(0);
        assert!(!layout.is_multi_pane()); // zoomed → effectively single
    }

    #[test]
    fn pane_rect_returns_none_for_invalid_index() {
        let layout = PaneLayout::from_preset(LayoutPreset::Single, &sids(1));
        assert!(layout.pane_rect(99, 1000.0, 800.0).is_none());
    }

    #[test]
    fn span_handles_single_item() {
        // One column → spans the whole container.
        let (off, sz) = span(&[1.0], 0, 1000.0);
        assert_eq!(off, 0.0);
        assert_eq!(sz, 1000.0);
    }

    #[test]
    fn span_offsets_accumulate() {
        // 3 columns of 1/3 each.
        let fracs = vec![1.0 / 3.0; 3];
        let (off0, sz0) = span(&fracs, 0, 900.0);
        let (off1, sz1) = span(&fracs, 1, 900.0);
        let (off2, sz2) = span(&fracs, 2, 900.0);
        assert!((off0 - 0.0).abs() < 1e-6);
        assert!((sz0 - 300.0).abs() < 1e-6);
        assert!((off1 - 300.0).abs() < 1e-6);
        assert!((sz1 - 300.0).abs() < 1e-6);
        assert!((off2 - 600.0).abs() < 1e-6);
        assert!((sz2 - 300.0).abs() < 1e-6);
    }

    // ------------------------------------------------------------------
    // Task 14 / 15 — additional edge-case coverage pinning the multi-pane
    // display contract (zoom + resize + comparison interactions).
    // ------------------------------------------------------------------

    /// Zooming a pane doesn't change the underlying fractions — when the
    /// user unzooms, the prior proportions are restored exactly. This is
    /// the "fullscreen doesn't destroy your layout" invariant of Task 14.
    #[test]
    fn zoom_does_not_modify_fractions() {
        let mut layout = PaneLayout::from_preset(LayoutPreset::Grid4, &sids(4));
        // Pre-distort the fractions so we can detect any drift.
        layout.resize_col(0, 0.15); // col 0 = 0.65, col 1 = 0.35
        layout.resize_row(0, 0.1); // row 0 = 0.6, row 1 = 0.4
        let cols_before = layout.col_fracs.clone();
        let rows_before = layout.row_fracs.clone();

        layout.zoom(2);
        // While zoomed, the fracs are unchanged.
        assert_eq!(layout.col_fracs, cols_before);
        assert_eq!(layout.row_fracs, rows_before);

        layout.unzoom();
        // After unzoom, still unchanged.
        assert_eq!(layout.col_fracs, cols_before);
        assert_eq!(layout.row_fracs, rows_before);
    }

    /// `visible_panes` always yields panes in row-major index order — the
    /// renderer relies on this to assign stable DOM keys (pane-{idx}-*).
    /// If the order ever became arbitrary, React-style reconcilers would
    /// re-mount every pane on each render, blowing away terminal scrollback.
    #[test]
    fn visible_panes_yields_in_index_order() {
        let layout = PaneLayout::from_preset(LayoutPreset::Grid8, &sids(8));
        let visible: Vec<usize> = layout
            .visible_panes(1000.0, 800.0)
            .map(|(i, _, _)| i)
            .collect();
        assert_eq!(visible, vec![0, 1, 2, 3, 4, 5, 6, 7]);
    }

    /// Comparison mode is independent of the zoom state — toggling
    /// comparison doesn't change which pane is zoomed, and toggling zoom
    /// doesn't change the comparison flag. This is what lets the user
    /// combine the two features (e.g., compare all panes, then zoom one to
    /// inspect it without losing the comparison flag).
    #[test]
    fn comparison_and_zoom_are_independent() {
        let mut layout = PaneLayout::from_preset(LayoutPreset::Grid4, &sids(4));
        // Turn comparison on, then zoom pane 1.
        assert!(layout.toggle_comparison());
        assert_eq!(layout.toggle_zoom(1), Some(1));
        assert!(layout.comparison);
        assert_eq!(layout.zoomed, Some(1));

        // Unzoom — comparison still on.
        assert_eq!(layout.unzoom(), Some(1));
        assert!(layout.comparison);
        assert!(layout.zoomed.is_none());

        // Turn comparison off — zoomed state stays None (already unzoomed),
        // and toggling zoom still works independently.
        assert!(!layout.toggle_comparison());
        assert_eq!(layout.toggle_zoom(2), Some(2));
        assert!(!layout.comparison);
        assert_eq!(layout.zoomed, Some(2));
    }

    /// `pane_rect` for an out-of-range index when zoomed returns `None`
    /// (rather than panicking or returning the zoomed pane's rect). This
    /// is the defensive contract the renderer relies on when a pane is
    /// closed mid-render: an out-of-range lookup must not crash.
    #[test]
    fn pane_rect_out_of_range_returns_none_even_when_zoomed() {
        let mut layout = PaneLayout::from_preset(LayoutPreset::Split2H, &sids(2));
        layout.zoom(0);
        assert!(layout.pane_rect(99, 1000.0, 800.0).is_none());
    }

    /// Resizing one axis doesn't disturb the other. Dragging a column
    /// splitter shouldn't shift the row fractions (and vice versa). This
    /// is the axis-independence invariant of Task 14's adjustable panes.
    #[test]
    fn resize_col_does_not_disturb_row_fractions() {
        let mut layout = PaneLayout::from_preset(LayoutPreset::Grid4, &sids(4));
        // Pre-distort rows so we can detect drift.
        layout.resize_row(0, 0.1); // row 0 = 0.6, row 1 = 0.4
        let rows_before = layout.row_fracs.clone();
        // Drag a column splitter.
        assert!(layout.resize_col(0, 0.2));
        // Rows are unchanged.
        assert_eq!(layout.row_fracs, rows_before);
    }

    #[test]
    fn resize_row_does_not_disturb_col_fractions() {
        let mut layout = PaneLayout::from_preset(LayoutPreset::Grid4, &sids(4));
        layout.resize_col(0, 0.15); // col 0 = 0.65, col 1 = 0.35
        let cols_before = layout.col_fracs.clone();
        assert!(layout.resize_row(0, 0.2));
        assert_eq!(layout.col_fracs, cols_before);
    }

    /// The grid invariant `rows * cols == panes.len()` holds for every
    /// preset. This is what makes the row-major indexing in `pane_rect`
    /// correct — if a preset ever broke this invariant, panes would be
    /// rendered at wrong positions.
    #[test]
    fn every_preset_satisfies_grid_invariant() {
        for &preset in &[
            LayoutPreset::Single,
            LayoutPreset::Split2H,
            LayoutPreset::Split2V,
            LayoutPreset::Grid4,
            LayoutPreset::Grid8,
        ] {
            let (rows, cols) = preset.grid_dims();
            let layout = PaneLayout::from_preset(preset, &sids(preset.pane_count()));
            assert_eq!(
                layout.panes.len(),
                rows * cols,
                "preset {:?}: panes.len() must equal rows*cols",
                preset,
            );
            assert_eq!(layout.rows(), rows);
            assert_eq!(layout.cols(), cols);
        }
    }

    /// Every pane in a freshly-built preset gets a non-empty rect that
    /// lies inside the container bounds. This is the renderer's basic
    /// "every pane is visible and on-screen" contract — if any pane had
    /// a zero or out-of-bounds rect, the TerminalView would render at
    /// size 0 and the PTY would get cols=0/rows=0 (which panics).
    #[test]
    fn every_pane_in_preset_has_in_bounds_nonzero_rect() {
        let container_w = 1200.0_f64;
        let container_h = 800.0_f64;
        for &preset in &[
            LayoutPreset::Split2H,
            LayoutPreset::Split2V,
            LayoutPreset::Grid4,
            LayoutPreset::Grid8,
        ] {
            let layout = PaneLayout::from_preset(preset, &sids(preset.pane_count()));
            for i in 0..layout.panes.len() {
                let (x, y, w, h) = layout
                    .pane_rect(i, container_w, container_h)
                    .unwrap_or_else(|| panic!("preset {:?} pane {} should have a rect", preset, i));
                assert!(w > 0.0, "preset {:?} pane {} has zero width", preset, i);
                assert!(h > 0.0, "preset {:?} pane {} has zero height", preset, i);
                assert!(
                    x >= 0.0 && x < container_w,
                    "preset {:?} pane {} x out of bounds",
                    preset,
                    i
                );
                assert!(
                    y >= 0.0 && y < container_h,
                    "preset {:?} pane {} y out of bounds",
                    preset,
                    i
                );
                assert!(
                    x + w <= container_w + 0.5,
                    "preset {:?} pane {} right edge out of bounds",
                    preset,
                    i
                );
                assert!(
                    y + h <= container_h + 0.5,
                    "preset {:?} pane {} bottom edge out of bounds",
                    preset,
                    i
                );
            }
        }
    }

    // ------------------------------------------------------------------
    // Performance contract tests (Task 16 optimization)
    // ------------------------------------------------------------------
    //
    // These tests pin the O(1)/O(panes) cost characteristics that the
    // drag-and-drop layer relies on. They don't measure wall-clock time
    // (which is flaky in CI); instead, they verify the structural
    // invariants that make the operations cheap:
    //   - swap_panes doesn't add/remove pane entries (no Vec resize)
    //   - swap_panes is O(1) in the number of panes (touches exactly 2)
    //   - set_pane_session doesn't iterate (early-return on out-of-range)
    //   - visible_panes yields exactly panes.len() items (no allocation
    //     beyond the caller's collect())
    //
    // If any of these tests fail, the drag-and-drop layer may start
    // causing per-tick re-renders or per-drop layout thrash.

    /// `swap_panes` must preserve the pane count — it exchanges sessions
    /// in place, never adds or removes pane entries. This is the grid
    /// invariant (`rows * cols == panes.len()`) that `pane_rect` and
    /// `visible_panes` rely on. If swap_panes resized the Vec, every
    /// downstream rect computation would have to re-run.
    #[test]
    fn swap_panes_preserves_pane_count() {
        let mut layout = PaneLayout::from_preset(LayoutPreset::Grid8, &sids(8));
        let count_before = layout.panes.len();
        // Swap a few pairs.
        layout.swap_panes(0, 7);
        layout.swap_panes(1, 6);
        layout.swap_panes(2, 5);
        layout.swap_panes(3, 4);
        assert_eq!(layout.panes.len(), count_before);
        // The grid invariant still holds.
        assert_eq!(layout.rows() * layout.cols(), layout.panes.len());
    }

    /// `set_pane_session` with an out-of-range index must return false
    /// without panicking or iterating — it's an O(1) bounds check. The
    /// drop handler calls this defensively (the drag source may have
    /// been closed mid-drag), so it must be cheap to fail.
    #[test]
    fn set_pane_session_out_of_range_returns_false_without_panicking() {
        let mut layout = PaneLayout::from_preset(LayoutPreset::Split2H, &sids(2));
        // Far out of range — must not panic (no indexing).
        assert!(!layout.set_pane_session(9999, "x".to_string()));
        // Just past the end.
        assert!(!layout.set_pane_session(2, "x".to_string()));
        // usize::MAX — the kind of index a stale closure might capture.
        assert!(!layout.set_pane_session(usize::MAX, "x".to_string()));
        // The layout is unchanged.
        assert_eq!(layout.panes.len(), 2);
        assert_eq!(layout.panes[0].session_id, "s0");
        assert_eq!(layout.panes[1].session_id, "s1");
    }

    /// `visible_panes` must yield exactly `panes.len()` items when not
    /// zoomed — the drag-and-drop render path iterates this and builds a
    // Vec of the same size for the rsx! for loop. If visible_panes
    // yielded more or fewer, the pane_items Vec would be mis-sized and
    // the closures would capture stale session_ids.
    #[test]
    fn visible_panes_yields_exactly_panes_len_when_not_zoomed() {
        for &preset in &[
            LayoutPreset::Split2H,
            LayoutPreset::Split2V,
            LayoutPreset::Grid4,
            LayoutPreset::Grid8,
        ] {
            let layout = PaneLayout::from_preset(preset, &sids(preset.pane_count()));
            let visible: Vec<_> = layout.visible_panes(1000.0, 800.0).collect();
            assert_eq!(
                visible.len(),
                layout.panes.len(),
                "preset {:?} yielded {} visible panes but has {} panes",
                preset,
                visible.len(),
                layout.panes.len()
            );
        }
    }

    // ------------------------------------------------------------------
    // Dynamic-container-size regression tests (Task 17 split-pane fix)
    // ------------------------------------------------------------------
    //
    // These pin the fix for the "显示分辨率不对" bug: `pane_rect` must
    // produce correct rectangles for ANY container size, not just the
    // prior hard-coded 1200×800. The old code used 1200×800 regardless of
    // the actual viewport, so panes were clipped (smaller window) or left
    // empty space (larger window). These tests verify the fractions scale
    // proportionally to whatever container dimensions are passed.

    /// `pane_rect` must scale with the container size — a Split2H layout
    /// in a 1920×1080 container must produce two 960-wide panes (not the
    /// old 600-wide panes that the 1200×800 hardcode would give). This is
    /// the core regression test for the dynamic-container-size fix.
    #[test]
    fn pane_rect_scales_with_arbitrary_container_size() {
        let layout = PaneLayout::from_preset(LayoutPreset::Split2H, &sids(2));
        // Small window (e.g. a phone-as-SSH-client portrait webview).
        let (x0, y0, w0, h0) = layout.pane_rect(0, 400.0, 600.0).unwrap();
        let (x1, y1, w1, h1) = layout.pane_rect(1, 400.0, 600.0).unwrap();
        assert_eq!((x0, y0, w0, h0), (0.0, 0.0, 200.0, 600.0));
        assert_eq!((x1, y1, w1, h1), (200.0, 0.0, 200.0, 600.0));
        // Large window (e.g. a 4K monitor maximized).
        let (x0, y0, w0, h0) = layout.pane_rect(0, 3840.0, 2160.0).unwrap();
        let (x1, y1, w1, h1) = layout.pane_rect(1, 3840.0, 2160.0).unwrap();
        assert_eq!((x0, y0, w0, h0), (0.0, 0.0, 1920.0, 2160.0));
        assert_eq!((x1, y1, w1, h1), (1920.0, 0.0, 1920.0, 2160.0));
        // Odd non-power-of-two size — no rounding artifacts expected at
        // the fraction level (the renderer rounds to integer pixels).
        let (x0, _, w0, _) = layout.pane_rect(0, 1001.0, 801.0).unwrap();
        let (x1, _, w1, _) = layout.pane_rect(1, 1001.0, 801.0).unwrap();
        assert!((x0 + w0 - x1).abs() < 1e-9, "panes must be adjacent");
        assert!(
            (w0 + w1 - 1001.0).abs() < 1e-9,
            "panes must fill the container"
        );
    }

    /// A Grid4 layout must fill the container with no gaps at any size.
    /// The four panes' combined area must equal the container area. This
    /// catches the prior bug where the 1200×800 hardcode would leave the
    /// bottom-right corner empty if the container was larger.
    #[test]
    fn grid4_fills_container_at_any_size() {
        for &(cw, ch) in &[
            (800.0, 600.0),
            (1920.0, 1080.0),
            (2560.0, 1440.0),
            (100.0, 100.0),
        ] {
            let layout = PaneLayout::from_preset(LayoutPreset::Grid4, &sids(4));
            let mut total_area = 0.0_f64;
            for i in 0..4 {
                let (x, y, w, h) = layout.pane_rect(i, cw, ch).unwrap();
                total_area += w * h;
                assert!(
                    x >= 0.0 && x < cw,
                    "pane {} x out of bounds at {}×{}",
                    i,
                    cw,
                    ch
                );
                assert!(
                    y >= 0.0 && y < ch,
                    "pane {} y out of bounds at {}×{}",
                    i,
                    cw,
                    ch
                );
                assert!(
                    x + w <= cw + 0.5,
                    "pane {} right edge out of bounds at {}×{}",
                    i,
                    cw,
                    ch
                );
                assert!(
                    y + h <= ch + 0.5,
                    "pane {} bottom edge out of bounds at {}×{}",
                    i,
                    cw,
                    ch
                );
            }
            assert!(
                (total_area - cw * ch).abs() < 1.0,
                "Grid4 panes' total area {} doesn't match container area {} at {}×{}",
                total_area,
                cw * ch,
                cw,
                ch
            );
        }
    }

    /// Drag-resize: a series of small `resize_col` deltas must accumulate
    /// to the same result as a single large delta of the same total. This
    /// pins the splitter drag behavior — the drag-poll loop applies many
    /// small deltas (one per mousemove event), and the cumulative effect
    /// must match what a single drag to the final position would give.
    #[test]
    fn resize_col_accumulates_many_small_deltas_like_one_large() {
        let mut layout_a = PaneLayout::from_preset(LayoutPreset::Split2H, &sids(2));
        let mut layout_b = PaneLayout::from_preset(LayoutPreset::Split2H, &sids(2));
        // Apply 20 small deltas of 0.01 each to layout_a.
        for _ in 0..20 {
            layout_a.resize_col(0, 0.01);
        }
        // Apply one delta of 0.20 to layout_b.
        layout_b.resize_col(0, 0.20);
        assert!(
            (layout_a.col_fracs[0] - layout_b.col_fracs[0]).abs() < 1e-9,
            "small-drag accumulation {} != single-drag {}",
            layout_a.col_fracs[0],
            layout_b.col_fracs[0]
        );
        assert!((layout_a.col_fracs[1] - layout_b.col_fracs[1]).abs() < 1e-9);
    }

    /// Drag-resize: same as above but for rows. The drag-poll loop applies
    /// many small `resize_row` deltas during a horizontal-splitter drag.
    #[test]
    fn resize_row_accumulates_many_small_deltas_like_one_large() {
        let mut layout_a = PaneLayout::from_preset(LayoutPreset::Split2V, &sids(2));
        let mut layout_b = PaneLayout::from_preset(LayoutPreset::Split2V, &sids(2));
        for _ in 0..15 {
            layout_a.resize_row(0, 0.01);
        }
        layout_b.resize_row(0, 0.15);
        assert!((layout_a.row_fracs[0] - layout_b.row_fracs[0]).abs() < 1e-9);
        assert!((layout_a.row_fracs[1] - layout_b.row_fracs[1]).abs() < 1e-9);
    }

    /// Drag-resize: when a delta is rejected (would push a pane below
    /// `MIN_PANE_FRAC`), the layout must NOT change — the drag effectively
    /// stops at the minimum. This is the behavior the drag-poll loop relies
    /// on: when the user drags a splitter all the way to one side, the pane
    /// clamps at MIN_PANE_FRAC and further mousemove events are no-ops.
    #[test]
    fn resize_col_rejected_delta_does_not_partially_apply() {
        let mut layout = PaneLayout::from_preset(LayoutPreset::Split2H, &sids(2));
        let before = layout.col_fracs.clone();
        // Try to shrink col 0 below MIN_PANE_FRAC (0.1). Col 0 is 0.5, so
        // a delta of -0.45 would make it 0.05 < 0.1 — rejected.
        assert!(!layout.resize_col(0, -0.45));
        assert_eq!(
            layout.col_fracs, before,
            "rejected resize must not modify fracs"
        );
    }

    // ------------------------------------------------------------------
    // Mouse-drag simulation tests (feature #17 task: 模拟用户使用鼠标挪动会话分屏)
    //
    // These tests simulate the user dragging a splitter bar with the mouse.
    // The drag-resize overlay's `onmousemove` handler computes a fractional
    // delta from the pixel delta between the current mouse position and the
    // last-applied position, then calls `resize_layout_col`/`resize_layout_row`.
    // These tests verify that sequence of small deltas produces the same
    // result as a single large delta — which is the correctness criterion
    // for the drag-resize feature.
    //
    // The tests are pure-Rust (no dioxus runtime) because the delta
    // computation is a pure function of the mouse position and the layout
    // state. The overlay's `onmousemove` handler is just a thin wrapper
    // around `resize_layout_col`/`resize_layout_row`.
    // ------------------------------------------------------------------

    /// Simulate a smooth col-splitter drag: the user clicks the splitter at
    /// x=500 (the boundary between two 500px columns in a 1000px container),
    /// then drags rightward in 10px increments to x=600. After each
    /// mousemove, the overlay computes `pixel_delta = current_x -
    /// last_applied_x`, converts to `frac_delta = pixel_delta /
    /// container_w`, and calls `resize_col(0, frac_delta)`. The final col
    /// fracs should match a single drag from x=500 to x=600 (delta=0.1).
    #[test]
    fn drag_col_splitter_rightward_in_small_increments() {
        let mut layout = PaneLayout::from_preset(LayoutPreset::Split2H, &sids(2));
        let container_w = 1000.0_f64;
        // Splitter starts at x=500 (boundary between col 0 and col 1).
        let mut last_applied_x = 500.0_f64;
        // Simulate 10 mousemove events, each 10px rightward.
        for step in 1..=10 {
            let current_x = 500.0 + (step as f64) * 10.0;
            let pixel_delta = current_x - last_applied_x;
            let frac_delta = pixel_delta / container_w;
            assert!(layout.resize_col(0, frac_delta));
            last_applied_x = current_x;
        }
        // After 10 steps of +0.01 each, col 0 should be 0.6, col 1 should be 0.4.
        assert!((layout.col_fracs[0] - 0.6).abs() < 1e-9);
        assert!((layout.col_fracs[1] - 0.4).abs() < 1e-9);
        // The splitter is now at x = 0.6 * 1000 = 600.
        let r0 = layout.pane_rect(0, container_w, 800.0).unwrap();
        let r1 = layout.pane_rect(1, container_w, 800.0).unwrap();
        assert!((r0.2 - 600.0).abs() < 1e-6); // col 0 width
        assert!((r1.0 - 600.0).abs() < 1e-6); // col 1 x
    }

    /// Simulate a smooth col-splitter drag leftward: the user drags from
    /// x=500 to x=400 in 10px increments. This shrinks col 0 and grows col 1.
    #[test]
    fn drag_col_splitter_leftward_in_small_increments() {
        let mut layout = PaneLayout::from_preset(LayoutPreset::Split2H, &sids(2));
        let container_w = 1000.0_f64;
        let mut last_applied_x = 500.0_f64;
        for step in 1..=10 {
            let current_x = 500.0 - (step as f64) * 10.0;
            let pixel_delta = current_x - last_applied_x;
            let frac_delta = pixel_delta / container_w;
            assert!(layout.resize_col(0, frac_delta));
            last_applied_x = current_x;
        }
        assert!((layout.col_fracs[0] - 0.4).abs() < 1e-9);
        assert!((layout.col_fracs[1] - 0.6).abs() < 1e-9);
    }

    /// Simulate a row-splitter drag downward: the user drags from y=400 to
    /// y=500 in 10px increments (container height 800). This grows row 0 and
    /// shrinks row 1.
    #[test]
    fn drag_row_splitter_downward_in_small_increments() {
        let mut layout = PaneLayout::from_preset(LayoutPreset::Split2V, &sids(2));
        let container_h = 800.0_f64;
        let mut last_applied_y = 400.0_f64;
        for step in 1..=10 {
            let current_y = 400.0 + (step as f64) * 10.0;
            let pixel_delta = current_y - last_applied_y;
            let frac_delta = pixel_delta / container_h;
            assert!(layout.resize_row(0, frac_delta));
            last_applied_y = current_y;
        }
        // 10 steps of +0.0125 each = +0.125 total. Row 0: 0.5 + 0.125 = 0.625.
        assert!((layout.row_fracs[0] - 0.625).abs() < 1e-9);
        assert!((layout.row_fracs[1] - 0.375).abs() < 1e-9);
    }

    /// Simulate a drag that hits the MIN_PANE_FRAC clamp: the user drags the
    /// col splitter all the way to the right, trying to shrink col 1 below
    /// 10%. The resize should be rejected at the clamp boundary, and further
    /// mousemove events beyond the boundary should be no-ops.
    #[test]
    fn drag_col_splitter_clamps_at_min_pane_frac() {
        let mut layout = PaneLayout::from_preset(LayoutPreset::Split2H, &sids(2));
        let container_w = 1000.0_f64;
        // Drag rightward in 10px steps. Col 1 starts at 0.5; MIN_PANE_FRAC is
        // 0.1, so we can shrink col 1 by at most 0.4 (40 steps of 0.01 each).
        // Due to floating-point accumulation, the 40th step may push col 1
        // just below 0.1 and be rejected — this is the correct clamping
        // behaviour. We accept either 39 or 40 successful steps.
        let mut last_applied_x = 500.0_f64;
        let mut successful_steps = 0;
        for step in 1..=40 {
            let current_x = 500.0 + (step as f64) * 10.0;
            let pixel_delta = current_x - last_applied_x;
            let frac_delta = pixel_delta / container_w;
            if layout.resize_col(0, frac_delta) {
                successful_steps += 1;
                last_applied_x = current_x;
            }
        }
        // Either 39 or 40 steps succeeded (depending on float rounding).
        assert!(
            successful_steps >= 39,
            "expected at least 39 successful steps, got {}",
            successful_steps
        );
        // Col 0 should be at least 0.89 (39 steps) or 0.90 (40 steps).
        assert!(layout.col_fracs[0] >= 0.89);
        assert!(layout.col_fracs[0] <= 0.91);
        // Col 1 should be at or just above MIN_PANE_FRAC.
        assert!(layout.col_fracs[1] >= MIN_PANE_FRAC - 1e-9);
        assert!(layout.col_fracs[1] <= MIN_PANE_FRAC + 0.02);
        // Now try a much larger delta that would clearly violate the clamp.
        // This must be rejected.
        let before = layout.col_fracs.clone();
        assert!(!layout.resize_col(0, 0.5));
        assert_eq!(
            layout.col_fracs, before,
            "rejected resize must not modify fracs"
        );
    }

    /// Simulate a drag with back-and-forth motion: the user drags right, then
    /// left, then right again. The net effect should be the sum of all deltas
    /// (with clamping at the boundaries). This verifies that the
    /// `last_applied_pos` tracking correctly handles direction changes.
    #[test]
    fn drag_col_splitter_back_and_forth() {
        let mut layout = PaneLayout::from_preset(LayoutPreset::Split2H, &sids(2));
        let container_w = 1000.0_f64;
        let mut last_applied_x = 500.0_f64;
        // Drag right to x=600 (+0.1).
        for step in 1..=10 {
            let current_x = 500.0 + (step as f64) * 10.0;
            let pixel_delta = current_x - last_applied_x;
            let frac_delta = pixel_delta / container_w;
            layout.resize_col(0, frac_delta);
            last_applied_x = current_x;
        }
        assert!((layout.col_fracs[0] - 0.6).abs() < 1e-9);
        // Drag left back to x=500 (-0.1).
        for step in 1..=10 {
            let current_x = 600.0 - (step as f64) * 10.0;
            let pixel_delta = current_x - last_applied_x;
            let frac_delta = pixel_delta / container_w;
            layout.resize_col(0, frac_delta);
            last_applied_x = current_x;
        }
        assert!((layout.col_fracs[0] - 0.5).abs() < 1e-9);
        // Drag right to x=550 (+0.05).
        for step in 1..=5 {
            let current_x = 500.0 + (step as f64) * 10.0;
            let pixel_delta = current_x - last_applied_x;
            let frac_delta = pixel_delta / container_w;
            layout.resize_col(0, frac_delta);
            last_applied_x = current_x;
        }
        assert!((layout.col_fracs[0] - 0.55).abs() < 1e-9);
    }

    /// Simulate a drag on a Grid4 layout: the user drags the col splitter
    /// between the two columns. Both rows' columns should resize together
    /// (because `resize_col` operates on the column fractions, which are
    /// shared across all rows in the current layout model).
    #[test]
    fn drag_col_splitter_in_grid4() {
        let mut layout = PaneLayout::from_preset(LayoutPreset::Grid4, &sids(4));
        let container_w = 1000.0_f64;
        // Drag the col splitter from x=500 to x=600 (+0.1).
        let mut last_applied_x = 500.0_f64;
        for step in 1..=10 {
            let current_x = 500.0 + (step as f64) * 10.0;
            let pixel_delta = current_x - last_applied_x;
            let frac_delta = pixel_delta / container_w;
            layout.resize_col(0, frac_delta);
            last_applied_x = current_x;
        }
        // Col 0 should be 0.6, col 1 should be 0.4.
        assert!((layout.col_fracs[0] - 0.6).abs() < 1e-9);
        assert!((layout.col_fracs[1] - 0.4).abs() < 1e-9);
        // All 4 panes should reflect the new column widths.
        let r0 = layout.pane_rect(0, container_w, 800.0).unwrap();
        let r1 = layout.pane_rect(1, container_w, 800.0).unwrap();
        assert!((r0.2 - 600.0).abs() < 1e-6); // pane 0 width
        assert!((r1.0 - 600.0).abs() < 1e-6); // pane 1 x
    }

    /// Simulate a drag on a Grid4 layout: the user drags the row splitter
    /// between the two rows. Both columns' rows should resize together.
    #[test]
    fn drag_row_splitter_in_grid4() {
        let mut layout = PaneLayout::from_preset(LayoutPreset::Grid4, &sids(4));
        let container_h = 800.0_f64;
        // Drag the row splitter from y=400 to y=500 (+0.125).
        let mut last_applied_y = 400.0_f64;
        for step in 1..=10 {
            let current_y = 400.0 + (step as f64) * 10.0;
            let pixel_delta = current_y - last_applied_y;
            let frac_delta = pixel_delta / container_h;
            layout.resize_row(0, frac_delta);
            last_applied_y = current_y;
        }
        // Row 0 should be 0.625, row 1 should be 0.375.
        assert!((layout.row_fracs[0] - 0.625).abs() < 1e-9);
        assert!((layout.row_fracs[1] - 0.375).abs() < 1e-9);
    }

    /// Simulate a drag where the mouse moves very slowly (1px per mousemove).
    /// This verifies that tiny deltas accumulate correctly without floating-
    /// point drift. 100 steps of 1px each should equal one step of 100px.
    #[test]
    fn drag_col_splitter_1px_increments_no_drift() {
        let mut layout_a = PaneLayout::from_preset(LayoutPreset::Split2H, &sids(2));
        let mut layout_b = PaneLayout::from_preset(LayoutPreset::Split2H, &sids(2));
        let container_w = 1000.0_f64;
        // layout_a: 100 steps of 1px each.
        let mut last_a = 500.0_f64;
        for step in 1..=100 {
            let current_x = 500.0 + (step as f64) * 1.0;
            let pixel_delta = current_x - last_a;
            let frac_delta = pixel_delta / container_w;
            layout_a.resize_col(0, frac_delta);
            last_a = current_x;
        }
        // layout_b: one step of 100px.
        layout_b.resize_col(0, 100.0 / container_w);
        // Both should have col_fracs[0] = 0.6.
        assert!((layout_a.col_fracs[0] - layout_b.col_fracs[0]).abs() < 1e-9);
        assert!((layout_a.col_fracs[1] - layout_b.col_fracs[1]).abs() < 1e-9);
    }

    /// Simulate a drag where the mouse moves very fast (100px per mousemove).
    /// This verifies that large deltas are applied correctly. 1 step of 100px
    /// should equal 10 steps of 10px.
    #[test]
    fn drag_col_splitter_100px_increments() {
        let mut layout_a = PaneLayout::from_preset(LayoutPreset::Split2H, &sids(2));
        let mut layout_b = PaneLayout::from_preset(LayoutPreset::Split2H, &sids(2));
        let container_w = 1000.0_f64;
        // layout_a: 1 step of 100px.
        layout_a.resize_col(0, 100.0 / container_w);
        // layout_b: 10 steps of 10px.
        let mut last_b = 500.0_f64;
        for step in 1..=10 {
            let current_x = 500.0 + (step as f64) * 10.0;
            let pixel_delta = current_x - last_b;
            let frac_delta = pixel_delta / container_w;
            layout_b.resize_col(0, frac_delta);
            last_b = current_x;
        }
        assert!((layout_a.col_fracs[0] - layout_b.col_fracs[0]).abs() < 1e-9);
    }

    /// Simulate a drag with zero container width (defensive: shouldn't happen
    /// in practice, but the overlay's `onmousemove` guards against it). The
    /// resize should be a no-op (frac_delta = 0).
    #[test]
    fn drag_col_splitter_zero_container_width_no_op() {
        let mut layout = PaneLayout::from_preset(LayoutPreset::Split2H, &sids(2));
        let before = layout.col_fracs.clone();
        let container_w = 0.0_f64;
        let pixel_delta = 100.0_f64;
        let frac_delta = if container_w > 0.0 {
            pixel_delta / container_w
        } else {
            0.0
        };
        // frac_delta is 0, so resize_col is a no-op (0.5 + 0 = 0.5, no change).
        let applied = layout.resize_col(0, frac_delta);
        // resize_col returns true if the delta was applied (even if 0).
        // But the fracs should be unchanged.
        assert_eq!(layout.col_fracs, before);
        let _ = applied; // don't assert on `applied` — implementation-defined for 0 delta.
    }

    // ------------------------------------------------------------------
    // Full mouse-drag simulation tests (mousedown → mousemove × N → mouseup)
    //
    // These tests simulate the COMPLETE splitter drag sequence using the
    // actual `compute_split_drag_delta` function from `app.rs` (the pure
    // function that decides, given the drag state and a new viewport
    // position, what fractional delta to apply) COMBINED with the actual
    // `PaneLayout::resize_col`/`resize_row` calls. This is the closest we
    // can get to an end-to-end test without a live dioxus runtime — it
    // verifies that the signal-flow logic (delta computation +
    // last_applied_pos tracking + resize call) produces correct layouts.
    //
    // Why these tests exist: the prior `drag_*_splitter_*` tests above
    // hard-code the delta computation (e.g. `pixel_delta / container_w`)
    // which means they'd pass even if `compute_split_drag_delta` had a bug
    // (e.g. used container-relative coordinates instead of viewport-
    // relative, causing an initial jump). These tests use the ACTUAL
    // delta-computation function, so they catch such bugs.
    // ------------------------------------------------------------------

    /// Simulate a full col-splitter drag: mousedown at viewport-x=700 (with
    /// sidebar offset of 200px, so splitter is at container-x=500), 10
    /// mousemoves of 10px each rightward, then mouseup. The final layout
    /// should have col 0 = 0.6, col 1 = 0.4 — same as a direct +0.1 resize.
    ///
    /// The viewport-coordinate offset (700 vs 500) is the key thing this
    /// tests: if `compute_split_drag_delta` used container-relative
    /// coordinates, the first mousemove would jump by 200px (the sidebar
    /// width) and the final layout would be wrong.
    #[test]
    fn full_drag_col_splitter_rightward_with_viewport_offset() {
        use crate::app::{SplitDragState, compute_split_drag_delta};
        let mut layout = PaneLayout::from_preset(LayoutPreset::Split2H, &sids(2));
        let container_w = 1000.0_f64;
        // Splitter at container-x=500, but viewport-x=700 (sidebar 200px).
        let mut drag = SplitDragState {
            is_col: true,
            idx: 0,
            container_extent: container_w,
            last_applied_pos: 700.0, // viewport-relative
        };
        // Simulate 10 mousemoves, each 10px rightward in viewport space.
        for step in 1..=10u32 {
            let pos = 700.0 + (step as f64) * 10.0;
            if let Some(frac_delta) = compute_split_drag_delta(&drag, pos) {
                assert!(layout.resize_col(0, frac_delta));
                drag.last_applied_pos = pos;
            } else {
                panic!("step {}: expected a delta, got None", step);
            }
        }
        // After 10 steps of +0.01 each, col 0 should be 0.6, col 1 should be 0.4.
        assert!(
            (layout.col_fracs[0] - 0.6).abs() < 1e-9,
            "col 0 should be 0.6, got {} — if it's 0.6+, the drag is using\n\
             container-relative coordinates and jumping by the sidebar width",
            layout.col_fracs[0]
        );
        assert!((layout.col_fracs[1] - 0.4).abs() < 1e-9);
    }

    /// Simulate a full row-splitter drag with viewport offset: mousedown at
    /// viewport-y=500 (tab bar 100px + splitter at container-y=400), 8
    /// mousemoves of 10px each downward, then mouseup. Final layout should
    /// have row 0 = 0.6, row 1 = 0.4.
    #[test]
    fn full_drag_row_splitter_downward_with_viewport_offset() {
        use crate::app::{SplitDragState, compute_split_drag_delta};
        let mut layout = PaneLayout::from_preset(LayoutPreset::Split2V, &sids(2));
        let container_h = 800.0_f64;
        let mut drag = SplitDragState {
            is_col: false,
            idx: 0,
            container_extent: container_h,
            last_applied_pos: 500.0, // viewport-relative (tab bar 100 + container 400)
        };
        for step in 1..=8u32 {
            let pos = 500.0 + (step as f64) * 10.0;
            if let Some(frac_delta) = compute_split_drag_delta(&drag, pos) {
                assert!(layout.resize_row(0, frac_delta));
                drag.last_applied_pos = pos;
            } else {
                panic!("step {}: expected a delta, got None", step);
            }
        }
        // 8 steps of +0.0125 each = +0.1 total. Row 0: 0.5 + 0.1 = 0.6.
        assert!(
            (layout.row_fracs[0] - 0.6).abs() < 1e-9,
            "row 0 should be 0.6, got {}",
            layout.row_fracs[0]
        );
        assert!((layout.row_fracs[1] - 0.4).abs() < 1e-9);
    }

    /// Simulate a drag with clamping: drag the col splitter all the way to
    /// the right edge (trying to shrink col 1 below MIN_PANE_FRAC). The
    /// resize should be rejected at the clamp boundary, and further
    /// mousemoves beyond the boundary should still compute deltas but the
    /// layout should not change (resize_col returns false, so
    /// last_applied_pos is NOT updated — this is what prevents the growing
    /// gap between mouse and splitter when clamped).
    #[test]
    fn full_drag_col_splitter_clamps_at_min_pane_frac() {
        use crate::app::{SplitDragState, compute_split_drag_delta};
        let mut layout = PaneLayout::from_preset(LayoutPreset::Split2H, &sids(2));
        let container_w = 1000.0_f64;
        let mut drag = SplitDragState {
            is_col: true,
            idx: 0,
            container_extent: container_w,
            last_applied_pos: 700.0, // viewport-relative
        };
        // Drag rightward: 700 → 1100 (400px = 0.4 frac, which would shrink
        // col 1 from 0.5 to 0.1 — exactly at MIN_PANE_FRAC).
        for step in 1..=40u32 {
            let pos = 700.0 + (step as f64) * 10.0;
            if let Some(frac_delta) = compute_split_drag_delta(&drag, pos) {
                if layout.resize_col(0, frac_delta) {
                    drag.last_applied_pos = pos;
                }
                // If resize was rejected, last_applied_pos is NOT updated —
                // subsequent deltas will be larger (catching up), which is
                // the desired behavior.
            }
        }
        // Col 0 should be at or near 0.9 (col 1 at MIN_PANE_FRAC = 0.1).
        assert!(
            layout.col_fracs[0] >= 0.89,
            "col 0 should be ≥0.89 (clamped), got {}",
            layout.col_fracs[0]
        );
        assert!(
            layout.col_fracs[0] <= 0.91,
            "col 0 should be ≤0.91 (clamped), got {}",
            layout.col_fracs[0]
        );
        assert!(layout.col_fracs[1] >= MIN_PANE_FRAC - 1e-9);
    }

    /// Simulate a drag with direction reversal: rightward, then leftward,
    /// then rightward again. The final layout should match a direct resize
    /// of the net delta.
    #[test]
    fn full_drag_col_splitter_back_and_forth() {
        use crate::app::{SplitDragState, compute_split_drag_delta};
        let mut layout = PaneLayout::from_preset(LayoutPreset::Split2H, &sids(2));
        let container_w = 1000.0_f64;
        let mut drag = SplitDragState {
            is_col: true,
            idx: 0,
            container_extent: container_w,
            last_applied_pos: 700.0,
        };
        // Rightward: 700 → 800 (+0.1).
        for step in 1..=10u32 {
            let pos = 700.0 + (step as f64) * 10.0;
            let frac = compute_split_drag_delta(&drag, pos).unwrap();
            layout.resize_col(0, frac);
            drag.last_applied_pos = pos;
        }
        assert!((layout.col_fracs[0] - 0.6).abs() < 1e-9);
        // Leftward: 800 → 700 (-0.1).
        for step in 1..=10u32 {
            let pos = 800.0 - (step as f64) * 10.0;
            let frac = compute_split_drag_delta(&drag, pos).unwrap();
            layout.resize_col(0, frac);
            drag.last_applied_pos = pos;
        }
        assert!((layout.col_fracs[0] - 0.5).abs() < 1e-9);
        // Rightward: 700 → 750 (+0.05).
        for step in 1..=5u32 {
            let pos = 700.0 + (step as f64) * 10.0;
            let frac = compute_split_drag_delta(&drag, pos).unwrap();
            layout.resize_col(0, frac);
            drag.last_applied_pos = pos;
        }
        assert!((layout.col_fracs[0] - 0.55).abs() < 1e-9);
    }

    /// Simulate a drag on a Grid4 layout: dragging the col splitter between
    /// the two columns should resize both rows' columns (because col_fracs
    /// are shared across all rows in the current layout model).
    #[test]
    fn full_drag_col_splitter_in_grid4_with_viewport_offset() {
        use crate::app::{SplitDragState, compute_split_drag_delta};
        let mut layout = PaneLayout::from_preset(LayoutPreset::Grid4, &sids(4));
        let container_w = 1000.0_f64;
        let mut drag = SplitDragState {
            is_col: true,
            idx: 0,
            container_extent: container_w,
            last_applied_pos: 700.0, // viewport-relative (sidebar 200 + splitter 500)
        };
        for step in 1..=10u32 {
            let pos = 700.0 + (step as f64) * 10.0;
            let frac = compute_split_drag_delta(&drag, pos).unwrap();
            layout.resize_col(0, frac);
            drag.last_applied_pos = pos;
        }
        assert!((layout.col_fracs[0] - 0.6).abs() < 1e-9);
        assert!((layout.col_fracs[1] - 0.4).abs() < 1e-9);
        // All 4 panes reflect the new column widths.
        let r0 = layout.pane_rect(0, container_w, 800.0).unwrap();
        assert!((r0.2 - 600.0).abs() < 1e-6); // pane 0 width = 600px
    }

    /// Simulate a drag where the mouse moves very slowly (1px per mousemove)
    /// — verifies tiny deltas accumulate without floating-point drift.
    /// 100 steps of 1px each should produce the same result as 1 step of 100px.
    #[test]
    fn full_drag_col_splitter_1px_increments_no_drift() {
        use crate::app::{SplitDragState, compute_split_drag_delta};
        let mut layout = PaneLayout::from_preset(LayoutPreset::Split2H, &sids(2));
        let container_w = 1000.0_f64;
        let mut drag = SplitDragState {
            is_col: true,
            idx: 0,
            container_extent: container_w,
            last_applied_pos: 700.0,
        };
        for step in 1..=100u32 {
            let pos = 700.0 + (step as f64) * 1.0;
            let frac = compute_split_drag_delta(&drag, pos).unwrap();
            layout.resize_col(0, frac);
            drag.last_applied_pos = pos;
        }
        // 100 steps of 1px each = 100px = 0.1 frac. Col 0: 0.5 + 0.1 = 0.6.
        assert!((layout.col_fracs[0] - 0.6).abs() < 1e-9);
    }

    /// Simulate a drag where the mouse moves very fast (100px in one jump).
    /// A single mousemove of 100px should produce the same result as 10
    /// mousemoves of 10px each.
    #[test]
    fn full_drag_col_splitter_100px_jump() {
        use crate::app::{SplitDragState, compute_split_drag_delta};
        let mut layout_a = PaneLayout::from_preset(LayoutPreset::Split2H, &sids(2));
        let mut layout_b = PaneLayout::from_preset(LayoutPreset::Split2H, &sids(2));
        let container_w = 1000.0_f64;
        // layout_a: 1 mousemove of 100px.
        let drag_a = SplitDragState {
            is_col: true,
            idx: 0,
            container_extent: container_w,
            last_applied_pos: 700.0,
        };
        let frac = compute_split_drag_delta(&drag_a, 800.0).unwrap();
        layout_a.resize_col(0, frac);
        // layout_b: 10 mousemoves of 10px each.
        let mut drag_b = SplitDragState {
            is_col: true,
            idx: 0,
            container_extent: container_w,
            last_applied_pos: 700.0,
        };
        for step in 1..=10u32 {
            let pos = 700.0 + (step as f64) * 10.0;
            let frac = compute_split_drag_delta(&drag_b, pos).unwrap();
            layout_b.resize_col(0, frac);
            drag_b.last_applied_pos = pos;
        }
        assert!((layout_a.col_fracs[0] - layout_b.col_fracs[0]).abs() < 1e-9);
    }

    #[test]
    fn floating_move_changes_only_target_pane_and_preserves_sessions() {
        let mut layout = PaneLayout::from_preset(LayoutPreset::Grid4, &sids(4));
        assert!(layout.enable_floating());
        let before = layout.clone();

        assert!(layout.bring_floating_pane_to_front(1));
        assert!(layout.move_floating_pane(1, 120.0, 80.0, 1200.0, 800.0));

        assert_eq!(layout.session_ids(), before.session_ids());
        assert_eq!(layout.panes[0].floating, before.panes[0].floating);
        assert_eq!(layout.panes[2].floating, before.panes[2].floating);
        assert_eq!(layout.panes[3].floating, before.panes[3].floating);
        assert_ne!(layout.panes[1].floating, before.panes[1].floating);
    }

    #[test]
    fn floating_move_is_clamped_inside_container() {
        let mut layout = PaneLayout::from_preset(LayoutPreset::Split2H, &sids(2));
        assert!(layout.move_floating_pane(0, -10_000.0, -10_000.0, 1200.0, 800.0));
        let top_left = layout.panes[0].floating.unwrap();
        assert_eq!(top_left.x_frac, 0.0);
        assert_eq!(top_left.y_frac, 0.0);

        assert!(layout.move_floating_pane(0, 10_000.0, 10_000.0, 1200.0, 800.0));
        let bottom_right = layout.panes[0].floating.unwrap();
        assert!((bottom_right.x_frac + bottom_right.width_frac - 1.0).abs() < 1e-9);
        assert!((bottom_right.y_frac + bottom_right.height_frac - 1.0).abs() < 1e-9);
    }

    #[test]
    fn floating_rect_scales_proportionally_after_container_resize() {
        let mut layout = PaneLayout::from_preset(LayoutPreset::Grid4, &sids(4));
        assert!(layout.move_floating_pane(2, 100.0, -40.0, 1000.0, 800.0));
        let small = layout.pane_rect(2, 1000.0, 800.0).unwrap();
        let large = layout.pane_rect(2, 2000.0, 1600.0).unwrap();

        assert_eq!(
            large,
            (small.0 * 2.0, small.1 * 2.0, small.2 * 2.0, small.3 * 2.0)
        );
    }

    #[test]
    fn bringing_floating_pane_forward_does_not_change_active_geometry() {
        let mut layout = PaneLayout::from_preset(LayoutPreset::Grid4, &sids(4));
        assert!(layout.enable_floating());
        let before_rects: Vec<_> = (0..4)
            .map(|idx| layout.pane_rect(idx, 1200.0, 800.0).unwrap())
            .collect();

        assert!(layout.bring_floating_pane_to_front(0));
        let max_z = layout
            .panes
            .iter()
            .filter_map(|pane| pane.floating.map(|geometry| geometry.z_index))
            .max()
            .unwrap();
        assert_eq!(layout.pane_z_index(0), Some(max_z));
        let after_rects: Vec<_> = (0..4)
            .map(|idx| layout.pane_rect(idx, 1200.0, 800.0).unwrap())
            .collect();
        assert_eq!(before_rects, after_rects);
    }

    #[test]
    fn swapping_floating_panes_keeps_window_geometry_in_place() {
        let mut layout = PaneLayout::from_preset(LayoutPreset::Split2H, &sids(2));
        assert!(layout.enable_floating());
        let first_geometry = layout.panes[0].floating;
        let second_geometry = layout.panes[1].floating;

        assert!(layout.swap_panes(0, 1));

        assert_eq!(layout.panes[0].session_id, "s1");
        assert_eq!(layout.panes[1].session_id, "s0");
        assert_eq!(layout.panes[0].floating, first_geometry);
        assert_eq!(layout.panes[1].floating, second_geometry);
    }
}
