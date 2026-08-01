//! Cross-pane output diff engine for comparison mode.
//!
//! When comparison mode is ON, the visible text output of every occupied pane
//! is compared line-by-line. Lines that differ across panes are highlighted
//! so the user can instantly spot where outputs diverge — the typical use
//! case is running the same command on multiple servers and scanning for
//! differences.
//!
//! ## Algorithm
//!
//! The first occupied pane is the **reference**. For each row index (up to the
//! tallest pane), the text at that row is compared across *all* panes. If any
//! pane's text differs, the row is marked [`RowDiff::Different`] in every pane;
//! otherwise it's [`RowDiff::Same`]. This produces a uniform highlighting
//! experience regardless of which pane the user is looking at.
//!
//! ## Threshold
//!
//! If the proportion of differing rows exceeds [`DIFF_THRESHOLD_FRAC`], the
//! outputs are likely fundamentally different (not a few-line delta) and
//! highlighting everything would be visually noisy. In that case the caller
//! should prompt the user before applying highlights (see
//! [`diff_summary`]).

use rusterm_core::terminal::{RenderCell, RenderRow};

/// Background highlight applied to rows whose content differs across panes.
/// A muted red so it reads as "attention" without overwhelming the terminal
/// text underneath.
pub const DIFF_ROW_BG: &str = "background:rgba(247,118,142,0.12);";

/// Per-row diff status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum RowDiff {
    /// Line content matches across all compared panes (no highlight).
    #[default]
    Same,
    /// Line content differs in at least one pane (highlighted).
    Different,
}

/// If the fraction of differing rows exceeds this value, the outputs are
/// considered "too different" and the caller should warn the user before
/// applying highlights. 50% means half the visible area would be highlighted.
pub const DIFF_THRESHOLD_FRAC: f64 = 0.5;

/// Convert a [`RenderRow`] to its trimmed text content, skipping wide-char
/// continuation cells (they duplicate the preceding cell's character).
fn row_to_text(row: &RenderRow) -> String {
    let s: String = row
        .cells
        .iter()
        .filter(|c: &&RenderCell| !c.wide_next)
        .map(|c| c.character)
        .collect();
    s.trim_end().to_string()
}

/// Extract the visible text lines from a slice of [`RenderRow`]s. Trailing
/// whitespace is trimmed per line so that cosmetic padding differences don't
/// register as false diffs.
pub fn extract_pane_lines(rows: &[RenderRow]) -> Vec<String> {
    rows.iter().map(row_to_text).collect()
}

/// Compute per-row diff status for a set of panes.
///
/// `pane_texts` is `(session_id, text_lines)` for each occupied pane, in
/// layout order. Returns one `Vec<RowDiff>` per pane (same order), where each
/// entry corresponds to a visible row in that pane.
///
/// The comparison is positional: row *i* of every pane is compared. If the
/// panes have different row counts, shorter panes contribute empty strings
/// for the missing rows (so those rows are marked `Different` in all panes
/// that *do* have content there, and `Different` in shorter panes too since
/// the content is missing).
pub fn compute_comparison_diffs(
    pane_texts: &[(String, Vec<String>)],
) -> Vec<(String, Vec<RowDiff>)> {
    if pane_texts.is_empty() {
        return Vec::new();
    }
    // Single pane — nothing to compare, all rows are "same".
    if pane_texts.len() == 1 {
        return pane_texts
            .iter()
            .map(|(sid, lines)| (sid.clone(), lines.iter().map(|_| RowDiff::Same).collect()))
            .collect();
    }

    let max_rows = pane_texts
        .iter()
        .map(|(_, lines)| lines.len())
        .max()
        .unwrap_or(0);

    // For each row index, determine whether all panes agree.
    let mut row_statuses: Vec<RowDiff> = Vec::with_capacity(max_rows);
    for row_idx in 0..max_rows {
        let mut texts: Vec<&str> = Vec::with_capacity(pane_texts.len());
        for (_, lines) in pane_texts {
            texts.push(lines.get(row_idx).map(|s| s.as_str()).unwrap_or(""));
        }
        let all_same = texts.windows(2).all(|w| w[0] == w[1]);
        row_statuses.push(if all_same {
            RowDiff::Same
        } else {
            RowDiff::Different
        });
    }

    // Each pane gets the same row-level status vector (the comparison is
    // across panes, not within one pane). A pane shorter than max_rows gets
    // statuses up to its own row count.
    pane_texts
        .iter()
        .map(|(sid, lines)| {
            let pane_statuses: Vec<RowDiff> = (0..lines.len())
                .map(|i| row_statuses.get(i).copied().unwrap_or(RowDiff::Same))
                .collect();
            (sid.clone(), pane_statuses)
        })
        .collect()
}

/// Summary of a diff computation, used to decide whether to warn the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffSummary {
    /// Total number of rows that differ across panes (counted once per row
    /// index, not per pane — i.e. a row that differs in 3 panes counts as 1).
    pub diff_rows: usize,
    /// Total visible rows in the reference pane (the tallest pane).
    pub total_rows: usize,
}

impl DiffSummary {
    /// Fraction of visible rows that differ (`diff_rows / total_rows`).
    /// Returns 0.0 if there are no rows.
    pub fn diff_fraction(&self) -> f64 {
        if self.total_rows == 0 {
            0.0
        } else {
            self.diff_rows as f64 / self.total_rows as f64
        }
    }

    /// Whether the diff exceeds the noise threshold — the caller should
    /// prompt the user before applying highlights.
    pub fn exceeds_threshold(&self) -> bool {
        self.diff_fraction() > DIFF_THRESHOLD_FRAC
    }
}

/// Compute a [`DiffSummary`] from per-pane diff vectors.
pub fn diff_summary(diffs: &[(String, Vec<RowDiff>)]) -> DiffSummary {
    let total_rows = diffs.iter().map(|(_, d)| d.len()).max().unwrap_or(0);
    // Count unique row indices that are Different in any pane.
    let diff_rows = (0..total_rows)
        .filter(|&row| {
            diffs
                .iter()
                .any(|(_, d)| d.get(row) == Some(&RowDiff::Different))
        })
        .count();
    DiffSummary {
        diff_rows,
        total_rows,
    }
}

/// Whether a comparison result should pause for the large-diff warning.
/// The persisted preference and the current comparison session's one-time
/// confirmation are intentionally separate concerns.
pub fn should_warn_for_large_diff(
    summary: &DiffSummary,
    warning_enabled: bool,
    confirmed_for_session: bool,
) -> bool {
    warning_enabled && !confirmed_for_session && summary.exceeds_threshold()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusterm_core::terminal::RenderRow;

    fn make_row(text: &str) -> RenderRow {
        let cells: Vec<_> = text
            .chars()
            .map(|c| RenderCell {
                character: c,
                ..Default::default()
            })
            .collect();
        RenderRow {
            cells,
            wrapped: false,
        }
    }

    #[test]
    fn identical_outputs_produce_all_same() {
        let lines = vec!["hello".to_string(), "world".to_string()];
        let panes = vec![
            ("a".to_string(), lines.clone()),
            ("b".to_string(), lines.clone()),
        ];
        let diffs = compute_comparison_diffs(&panes);
        assert_eq!(diffs.len(), 2);
        assert!(diffs[0].1.iter().all(|d| *d == RowDiff::Same));
        assert!(diffs[1].1.iter().all(|d| *d == RowDiff::Same));
    }

    #[test]
    fn differing_lines_marked_different() {
        let panes = vec![
            (
                "a".to_string(),
                vec!["same".to_string(), "diff_a".to_string()],
            ),
            (
                "b".to_string(),
                vec!["same".to_string(), "diff_b".to_string()],
            ),
        ];
        let diffs = compute_comparison_diffs(&panes);
        assert_eq!(diffs[0].1[0], RowDiff::Same);
        assert_eq!(diffs[0].1[1], RowDiff::Different);
        assert_eq!(diffs[1].1[0], RowDiff::Same);
        assert_eq!(diffs[1].1[1], RowDiff::Different);
    }

    #[test]
    fn different_row_counts_mark_extra_rows() {
        let panes = vec![
            (
                "a".to_string(),
                vec![
                    "line1".to_string(),
                    "line2".to_string(),
                    "line3".to_string(),
                ],
            ),
            (
                "b".to_string(),
                vec!["line1".to_string(), "line2".to_string()],
            ),
        ];
        let diffs = compute_comparison_diffs(&panes);
        // First two lines match.
        assert_eq!(diffs[0].1[0], RowDiff::Same);
        assert_eq!(diffs[0].1[1], RowDiff::Same);
        // Third line only in pane a → different.
        assert_eq!(diffs[0].1[2], RowDiff::Different);
        // Pane b only has 2 rows.
        assert_eq!(diffs[1].1.len(), 2);
    }

    #[test]
    fn single_pane_all_same() {
        let panes = vec![("a".to_string(), vec!["x".to_string(), "y".to_string()])];
        let diffs = compute_comparison_diffs(&panes);
        assert!(diffs[0].1.iter().all(|d| *d == RowDiff::Same));
    }

    #[test]
    fn empty_panes_returns_empty() {
        let diffs = compute_comparison_diffs(&[]);
        assert!(diffs.is_empty());
    }

    #[test]
    fn row_to_text_skips_wide_continuations() {
        let mut row = make_row("abc");
        // Add a wide continuation cell.
        row.cells.push(RenderCell {
            character: '\u{0}',
            wide_next: true,
            ..Default::default()
        });
        assert_eq!(row_to_text(&row), "abc");
    }

    #[test]
    fn extract_pane_lines_trims_trailing_whitespace() {
        let rows = vec![make_row("hello   "), make_row("world")];
        let lines = extract_pane_lines(&rows);
        assert_eq!(lines, vec!["hello", "world"]);
    }

    #[test]
    fn diff_summary_counts_correctly() {
        let panes = vec![
            (
                "a".to_string(),
                vec!["same".to_string(), "diff".to_string(), "same2".to_string()],
            ),
            (
                "b".to_string(),
                vec!["same".to_string(), "DIFF".to_string(), "same2".to_string()],
            ),
        ];
        let diffs = compute_comparison_diffs(&panes);
        let summary = diff_summary(&diffs);
        assert_eq!(summary.diff_rows, 1);
        assert_eq!(summary.total_rows, 3);
        assert!(!summary.exceeds_threshold());
    }

    #[test]
    fn diff_summary_threshold_detection() {
        // 3 out of 4 lines differ → 75% > 50% threshold.
        let panes = vec![
            (
                "a".to_string(),
                vec![
                    "s".to_string(),
                    "d1".to_string(),
                    "d2".to_string(),
                    "d3".to_string(),
                ],
            ),
            (
                "b".to_string(),
                vec![
                    "s".to_string(),
                    "x1".to_string(),
                    "x2".to_string(),
                    "x3".to_string(),
                ],
            ),
        ];
        let diffs = compute_comparison_diffs(&panes);
        let summary = diff_summary(&diffs);
        assert_eq!(summary.diff_rows, 3);
        assert_eq!(summary.total_rows, 4);
        assert!(summary.exceeds_threshold());
    }

    #[test]
    fn diff_fraction_zero_for_empty() {
        let summary = DiffSummary {
            diff_rows: 0,
            total_rows: 0,
        };
        assert_eq!(summary.diff_fraction(), 0.0);
        assert!(!summary.exceeds_threshold());
    }

    #[test]
    fn three_pane_comparison() {
        let panes = vec![
            (
                "a".to_string(),
                vec!["same".to_string(), "line".to_string()],
            ),
            (
                "b".to_string(),
                vec!["same".to_string(), "line".to_string()],
            ),
            (
                "c".to_string(),
                vec!["same".to_string(), "DIFFERENT".to_string()],
            ),
        ];
        let diffs = compute_comparison_diffs(&panes);
        // Row 0: all same.
        assert!(diffs.iter().all(|(_, d)| d[0] == RowDiff::Same));
        // Row 1: differs (c is different).
        assert!(diffs.iter().all(|(_, d)| d[1] == RowDiff::Different));
    }
}
