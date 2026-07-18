//! Persistent window state — remembers the user's preferred window size,
//! position, and maximization state across app launches.
//!
//! The state is stored as a small JSON file (`window_state.json`) in the same
//! directory as `settings.json` (next to the binary, or in the platform config
//! dir if the binary's directory isn't writable). It's intentionally separate
//! from `settings.json` so it isn't gated behind the master password — the
//! window should restore correctly even before the user unlocks the app.
//!
//! The App component polls the live window geometry every 250ms and calls
//! `save()` when it changes. The 250ms poll interval acts as a natural debounce
//! so dragging the window doesn't write hundreds of times per second.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const WINDOW_STATE_FILE_NAME: &str = "window_state.json";

/// Persisted window geometry + maximization flag.
///
/// `maximized` takes precedence over `width`/`height`/`x`/`y` on restore: if the
/// user last closed the app maximized, we re-maximize on launch (and skip the
/// size/position restore, since the OS will pick the maximized geometry).
///
/// All sizes are in **logical** pixels (DPI-independent), matching
/// `tao`'s `LogicalSize` / `LogicalPosition`. On a 2x Retina display a
/// 1200x800 logical window is 2400x1600 physical pixels.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct WindowState {
    /// Window width in logical pixels.
    #[serde(default)]
    pub width: f64,
    /// Window height in logical pixels.
    #[serde(default)]
    pub height: f64,
    /// Window outer X position in logical pixels (screen-relative).
    #[serde(default)]
    pub x: f64,
    /// Window outer Y position in logical pixels (screen-relative).
    #[serde(default)]
    pub y: f64,
    /// Whether the window was maximized when last persisted.
    #[serde(default)]
    pub maximized: bool,
}

impl WindowState {
    /// Resolve the path to the window-state file.
    ///
    /// Mirrors `ConfigManager::resolve_config_path`'s precedence:
    /// 1. `RUSTERM_CONFIG_DIR` env var (test/config override),
    /// 2. next to the binary (same dir as `settings.json`),
    /// 3. platform config dir fallback.
    ///
    /// This keeps the window state co-located with the rest of the app's
    /// per-machine data, so e.g. a portable install on a USB stick carries its
    /// window state with it.
    pub fn resolve_path() -> Result<PathBuf> {
        if let Ok(dir) = std::env::var("RUSTERM_CONFIG_DIR") {
            let path = PathBuf::from(dir);
            std::fs::create_dir_all(&path).ok();
            return Ok(path.join(WINDOW_STATE_FILE_NAME));
        }
        if let Ok(exe) = std::env::current_exe() {
            if let Some(parent) = exe.parent() {
                return Ok(parent.join(WINDOW_STATE_FILE_NAME));
            }
        }
        let config_dir = dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("rusterm");
        std::fs::create_dir_all(&config_dir).ok();
        Ok(config_dir.join(WINDOW_STATE_FILE_NAME))
    }

    /// Load the saved window state. Returns `None` if the file doesn't exist
    /// or can't be parsed — the caller should fall back to defaults in that
    /// case (first launch, or corrupt file).
    pub fn load() -> Option<WindowState> {
        let path = Self::resolve_path().ok()?;
        Self::load_from(&path)
    }

    /// Load from a specific path — used by tests to avoid env-var races and
    /// by callers that already know the path.
    pub fn load_from(path: &PathBuf) -> Option<WindowState> {
        let content = std::fs::read_to_string(path).ok()?;
        match serde_json::from_str::<WindowState>(&content) {
            Ok(state) => {
                // Sanity-check the loaded values: a zero/negative width or
                // height would make the window invisible. If the values look
                // bogus, discard them and start fresh.
                if state.width < 100.0 || state.height < 100.0 {
                    tracing::warn!(
                        "Discarding window_state.json: bogus size {}x{}",
                        state.width,
                        state.height
                    );
                    return None;
                }
                Some(state)
            }
            Err(e) => {
                tracing::warn!("Failed to parse window_state.json: {}", e);
                None
            }
        }
    }

    /// Persist the window state to disk atomically (write-temp-then-rename)
    /// so a crash mid-write doesn't corrupt the file.
    pub fn save(&self) -> Result<()> {
        let path = Self::resolve_path()?;
        self.save_to(&path)
    }

    /// Save to a specific path — used by tests to avoid env-var races.
    pub fn save_to(&self, path: &PathBuf) -> Result<()> {
        let json =
            serde_json::to_string_pretty(self).context("Failed to serialize window state")?;
        let temp_path = path.with_extension("json.tmp");
        std::fs::write(&temp_path, &json).context("Failed to write temp window state")?;
        std::fs::rename(&temp_path, path).context("Failed to rename window state file")?;
        Ok(())
    }

    /// Returns `true` if the state has a usable size (non-zero width/height).
    /// Used to decide whether to restore the size on launch.
    pub fn has_size(&self) -> bool {
        self.width > 0.0 && self.height > 0.0
    }

    /// Returns `true` if the state has a usable position (non-zero x/y).
    /// Used to decide whether to restore the position on launch.
    pub fn has_position(&self) -> bool {
        self.x != 0.0 || self.y != 0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_state_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("window_state.json");

        let state = WindowState {
            width: 1200.0,
            height: 800.0,
            x: 100.0,
            y: 50.0,
            maximized: false,
        };
        state.save_to(&path).unwrap();
        let loaded = WindowState::load_from(&path).unwrap();
        assert_eq!(state, loaded);
    }

    #[test]
    fn load_returns_none_on_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("window_state.json");
        assert!(WindowState::load_from(&path).is_none());
    }

    #[test]
    fn load_returns_none_on_corrupt_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("window_state.json");
        std::fs::write(&path, "not valid json").unwrap();
        assert!(WindowState::load_from(&path).is_none());
    }

    #[test]
    fn load_returns_none_on_bogus_size() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("window_state.json");
        let state = WindowState {
            width: 0.0,
            height: 0.0,
            x: 0.0,
            y: 0.0,
            maximized: true,
        };
        state.save_to(&path).unwrap();
        assert!(
            WindowState::load_from(&path).is_none(),
            "zero-size window state should be discarded"
        );
    }
}
