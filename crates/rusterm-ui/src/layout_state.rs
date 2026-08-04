//! Persistent pane-layout state — remembers the user's custom multi-pane
//! arrangement (split tree, column/row fractions, floating window geometry,
//! comparison flag) across app launches.
//!
//! ## Why a separate file (not part of `session_state.enc`)
//!
//! `PaneLayout` lives in `rusterm-ui`, while `SessionState` lives in
//! `rusterm-core`. `rusterm-core` cannot depend on `rusterm-ui` (that would
//! be a circular dependency), so the layout snapshot can't be typed directly
//! inside `SessionState`. Instead we mirror the proven `window_state.json`
//! pattern: a small **plaintext** JSON file managed entirely by the UI layer.
//!
//! The layout contains no secrets (only geometry + session *names*), so
//! plaintext is acceptable — exactly like `window_state.json`.
//!
//! ## The session-id ↔ name bridge
//!
//! `PaneLayout::panes[i].session_id` holds a live session UUID at runtime.
//! UUIDs are regenerated on every launch (see `restore_sessions`), so we
//! can't persist them directly. Instead, at save time each pane's
//! `session_id` is swapped for the session's display **name** (which is
//! stable and matchable across launches, e.g. `"user@host"`). At restore
//! time — after all sessions have been reopened — we map each name back to
//! the new session id. Panes whose name no longer resolves (the session
//! wasn't restored) are left empty, which the renderer treats as a blank
//! drop target.
//!
//! ## Persistence cadence
//!
//! The UI polls every [`SAVE_INTERVAL_SECS`] seconds and writes the current
//! layout snapshot, plus on app exit. This mirrors the session-state save
//! loop — a balance between not losing user customisation and not hammering
//! the disk.

use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::layout::PaneLayout;

const FILE_NAME: &str = "layout_state.json";

/// How often the periodic save loop writes the layout snapshot. Kept aligned
/// with the session-state save interval (30s) so both snapshots are roughly
/// in sync, but exposed as a named constant so callers (and tests) can
/// reason about it.
pub const SAVE_INTERVAL_SECS: u64 = 30;

/// Top-level persisted layout snapshot. One per app exit; one per app launch.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct LayoutState {
    /// Schema version — bump when the on-disk layout changes. Older versions
    /// are rejected on load (the data is ephemeral; the user just loses their
    /// last layout customisation, which is acceptable on a format change).
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,

    /// When this snapshot was taken (UTC). Informational.
    #[serde(default)]
    pub saved_at: Option<DateTime<Utc>>,

    /// One entry per workspace tab that had a non-default (multi-pane or
    /// otherwise customised) layout. Tabs with a plain single-pane layout
    /// are omitted to keep the file small.
    #[serde(default)]
    pub tabs: Vec<PersistedTabLayout>,
}

fn default_schema_version() -> u32 {
    1
}

/// A single workspace tab's layout, persisted as an independent JSON segment.
///
/// `anchor_name` is the display name of the tab's anchor session — the stable
/// key used to reattach this layout to the correct tab after sessions are
/// restored with fresh UUIDs.
///
/// `layout` is the full `PaneLayout`. At save time, every pane's
/// `session_id` has been replaced with the session's display name; at
/// restore time the names are mapped back to live session ids.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PersistedTabLayout {
    pub anchor_name: String,
    #[serde(default)]
    pub layout: PaneLayout,
}

impl LayoutState {
    /// Resolve the path to the layout-state file, mirroring
    /// `WindowState::resolve_path` / `SessionState::resolve_path`.
    pub fn resolve_path() -> Result<PathBuf> {
        rusterm_core::paths::resolve_config_file_path(FILE_NAME)
    }

    /// Load the saved layout state. Returns `Ok(None)` if the file doesn't
    /// exist (first launch). Returns `Ok(default)` on a parse error so a
    /// corrupt file never blocks startup — the user just gets a fresh layout.
    pub fn load() -> Option<LayoutState> {
        let path = Self::resolve_path().ok()?;
        Self::load_from(&path)
    }

    /// Load from a specific path — used by tests to avoid env-var races.
    pub fn load_from(path: &PathBuf) -> Option<LayoutState> {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => {
                tracing::warn!("Failed to read layout_state.json: {}", e);
                return None;
            }
        };
        match serde_json::from_str::<LayoutState>(&content) {
            Ok(state) if state.schema_version == default_schema_version() => Some(state),
            Ok(state) => {
                tracing::warn!(
                    "Discarding layout_state.json: schema version {} (expected {})",
                    state.schema_version,
                    default_schema_version()
                );
                None
            }
            Err(e) => {
                tracing::warn!("Failed to parse layout_state.json: {}", e);
                None
            }
        }
    }

    /// Persist atomically (write-temp-then-rename) so a crash mid-write
    /// can't corrupt the file — same strategy as `WindowState::save`.
    pub fn save(&self) -> Result<()> {
        let path = Self::resolve_path()?;
        self.save_to(&path)
    }

    /// Save to a specific path — used by tests.
    pub fn save_to(&self, path: &PathBuf) -> Result<()> {
        let json =
            serde_json::to_string_pretty(self).context("Failed to serialize layout state")?;
        let temp_path = path.with_extension("json.tmp");
        std::fs::write(&temp_path, &json).context("Failed to write temp layout state")?;
        std::fs::rename(&temp_path, path).context("Failed to rename layout state file")?;
        Ok(())
    }

    /// Remove the persisted layout-state file, if present.
    ///
    /// Called when the current workspace has no non-trivial layouts to save:
    /// without this, the last multi-pane arrangement would fossilise on disk
    /// and be re-applied to every future launch even after the user went back
    /// to plain single-pane tabs — restoring sessions then collapses separate
    /// same-named tabs into one tab's split panes.
    pub fn delete() -> Result<()> {
        let path = Self::resolve_path()?;
        Self::delete_from(&path)
    }

    /// Delete from a specific path — used by tests. A missing file is fine.
    pub fn delete_from(path: &PathBuf) -> Result<()> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).context("Failed to delete layout state file"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{LayoutPreset, PaneLayout};

    #[test]
    fn layout_state_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("layout_state.json");

        let mut layout = PaneLayout::from_preset(
            LayoutPreset::Split2H,
            &["alpha".to_string(), "beta".to_string()],
        );
        layout.comparison = true;

        let state = LayoutState {
            schema_version: 1,
            saved_at: Some(Utc::now()),
            tabs: vec![PersistedTabLayout {
                anchor_name: "alpha".to_string(),
                layout,
            }],
        };
        state.save_to(&path).unwrap();
        let loaded = LayoutState::load_from(&path).unwrap();
        assert_eq!(state, loaded);
    }

    #[test]
    fn delete_removes_file_and_ignores_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("layout_state.json");

        // Missing file: no-op, no error.
        LayoutState::delete_from(&path).unwrap();

        let state = LayoutState {
            schema_version: 1,
            saved_at: None,
            tabs: vec![],
        };
        state.save_to(&path).unwrap();
        assert!(path.exists());

        LayoutState::delete_from(&path).unwrap();
        assert!(!path.exists());
        assert!(LayoutState::load_from(&path).is_none());
    }

    #[test]
    fn load_returns_none_on_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("layout_state.json");
        assert!(LayoutState::load_from(&path).is_none());
    }

    #[test]
    fn load_returns_none_on_corrupt_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("layout_state.json");
        std::fs::write(&path, "not valid json").unwrap();
        assert!(LayoutState::load_from(&path).is_none());
    }

    #[test]
    fn load_returns_none_on_wrong_schema_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("layout_state.json");
        let state = LayoutState {
            schema_version: 999,
            saved_at: None,
            tabs: vec![],
        };
        state.save_to(&path).unwrap();
        assert!(
            LayoutState::load_from(&path).is_none(),
            "future schema version should be discarded"
        );
    }

    #[test]
    fn empty_tabs_serialize_compactly() {
        let state = LayoutState {
            schema_version: 1,
            saved_at: None,
            tabs: vec![],
        };
        let json = serde_json::to_string(&state).unwrap();
        // tabs should be present but empty, not null.
        assert!(json.contains(r#""tabs":[]"#));
    }
}
