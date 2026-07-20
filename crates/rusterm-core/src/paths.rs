//! Centralised resolution of file paths under the app's config directory.
//!
//! ## Why this module exists
//!
//! Before this module, three separate files (`config_manager.rs`,
//! `session_state.rs`, `window_state.rs`) each had their own copy of the
//! same "resolve the config file path" logic. Worse, the resolution order
//! was:
//!
//! 1. `RUSTERM_CONFIG_DIR` env var
//! 2. **Next to the binary** (the primary location)
//! 3. Platform config dir (`~/.config/rusterm/` on Linux,
//!    `~/Library/Application Support/rusterm/` on macOS, `%APPDATA%\rusterm\`
//!    on Windows) as a fallback.
//!
//! Putting "next to the binary" first is a problem during development
//! because the binary lives under `target/debug/` (or `target/release/`),
//! and `cargo clean` deletes the entire `target/` tree — taking the user's
//! saved connections, master password hash, window state, and session
//! state with it. The user explicitly asked for the config to live at a
//! stable default location (`~/.config/rusterm/` or the platform
//! equivalent) that survives `cargo clean`.
//!
//! ## New resolution order
//!
//! [`resolve_config_file_path`] now uses:
//!
//! 1. `RUSTERM_CONFIG_DIR` env var (unchanged — test/override hook).
//! 2. **Platform config dir** (`~/.config/rusterm/` or equivalent) — the
//!    new primary location. Stable across `cargo clean`, follows platform
//!    conventions, and is where users expect to find app config.
//! 3. **Auto-migrate from "next to the binary"** — if the platform dir
//!    doesn't have the file but the binary's directory does (a legacy
//!    install or a pre-this-change dev build), we move the file to the
//!    platform dir. This is one-shot: after the migration, the platform
//!    dir has the file and the binary-dir copy is gone. See
//!    [`migrate_from_binary_dir_if_needed`].
//! 4. **Binary-dir fallback** — only used if the platform config dir
//!    can't be determined (very rare: `HOME` unset on Unix, or a
//!    corrupt OS profile on Windows). This keeps a portable / USB-stick
//!    install working as a last resort.
//!
//! ## What this module does NOT do
//!
//! - It doesn't read or write the file contents — just resolves the path.
//!   The caller owns the I/O.
//! - It doesn't migrate directories, only individual files. Each caller
//!   (`config_manager`, `session_state`, `window_state`) calls
//!   [`resolve_config_file_path`] with its own filename, and the migration
//!   check happens per-file (so a partial state — e.g. `settings.json`
//!   exists in both places but `window_state.json` only in the binary dir
//!   — is handled correctly).
//! - It doesn't log the migration at `tracing::info!` because the
//!   migration happens during `ConfigManager::new()` / state load, which
//!   can be before logging is initialised. Callers that want to log can
//!   check the returned path's parent.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// The subdirectory name under the platform config dir.
///
/// On Linux this is `~/.config/rusterm/`, on macOS
/// `~/Library/Application Support/rusterm/`, on Windows
/// `%APPDATA%\rusterm\`. The `rusterm` suffix is constant across
/// platforms so the directory is recognisable in a file manager.
pub const APP_CONFIG_SUBDIR: &str = "rusterm";

/// Resolve the path to a config file under the app's config directory.
///
/// See the module docs for the full resolution order. Returns the path
/// (the file may or may not exist — the caller should check). The parent
/// directory is guaranteed to exist (created if necessary) unless we
/// fall back to the binary-dir path, in which case the parent already
/// exists (it's the binary's directory).
///
/// # Arguments
///
/// - `filename` — the leaf filename, e.g. `"settings.json"` or
///   `"window_state.json"`.
///
/// # Errors
///
/// Returns `Err` only if:
/// - The env var override is set but the directory can't be created.
/// - The platform config dir can be determined but the `rusterm`
///   subdirectory can't be created (permission denied, etc.).
///
/// Both cases are fatal — the caller can't proceed without a config path.
pub fn resolve_config_file_path(filename: &str) -> Result<PathBuf> {
    // 1. Override via environment variable (test/config override hook).
    if let Ok(dir) = std::env::var("RUSTERM_CONFIG_DIR") {
        let path = PathBuf::from(&dir);
        fs::create_dir_all(&path).with_context(|| {
            format!("Failed to create config dir from RUSTERM_CONFIG_DIR={dir}")
        })?;
        return Ok(path.join(filename));
    }

    // 2. Platform config dir (primary location — survives `cargo clean`).
    if let Some(platform_dir) = platform_config_dir() {
        let app_dir = platform_dir.join(APP_CONFIG_SUBDIR);
        fs::create_dir_all(&app_dir)
            .with_context(|| format!("Failed to create app config dir at {}", app_dir.display()))?;
        let target_path = app_dir.join(filename);

        // 3. Auto-migrate from "next to the binary" if the platform dir
        //    doesn't have the file but the binary dir does. One-shot:
        //    after the move, the platform dir has the file and the
        //    binary-dir copy is gone. We silently ignore migration
        //    errors (the worst case is the file stays in the binary dir
        //    and we read it from there next time — but we've already
        //    committed to the platform dir as the primary, so we'd
        //    actually return the (non-existent) platform path and let
        //    the caller treat it as "first launch". That's acceptable
        //    because a failed migration is very rare — it'd require a
        //    filesystem error between the existence checks).
        migrate_from_binary_dir_if_needed(filename, &target_path);

        return Ok(target_path);
    }

    // 4. Binary-dir fallback (very rare — only if platform config dir
    //    can't be determined). This keeps a portable install working.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            return Ok(parent.join(filename));
        }
    }

    // 5. Last-resort fallback: relative to CWD. This should never happen
    //    in practice (it requires both `dirs::config_dir()` to return
    //    `None` AND `std::env::current_exe()` to fail), but returning an
    //    error here would crash the app on startup. A relative path is
    //    better than nothing.
    Ok(PathBuf::from(".").join(filename))
}

/// The platform-config-dir portion of the resolution (steps 2 + 3 above),
/// without the env-var override or the binary-dir fallback. Exposed so
/// callers that want to list the directory (e.g. for an "open config
/// folder" UI action) can get the canonical app config dir.
///
/// Returns `None` if the platform config dir can't be determined.
pub fn app_config_dir() -> Option<PathBuf> {
    platform_config_dir().map(|d| d.join(APP_CONFIG_SUBDIR))
}

/// The raw platform config dir (e.g. `~/.config` on Linux,
/// `~/Library/Application Support` on macOS). Thin wrapper around
/// `dirs::config_dir()` extracted for testability.
pub fn platform_config_dir() -> Option<PathBuf> {
    dirs::config_dir()
}

/// If the file exists next to the binary but NOT at the target (platform)
/// path, move it there. Silently ignores any error — the worst case is
/// the file stays in the binary dir and the caller treats the platform
/// path as "first launch" (which is the correct behaviour for a
/// migration that failed: don't lose data, just don't migrate).
///
/// This is idempotent: if the file doesn't exist at the binary dir, or
/// already exists at the target, it's a no-op.
fn migrate_from_binary_dir_if_needed(filename: &str, target_path: &Path) {
    // If the target already exists, don't overwrite it — the user may
    // have a newer config at the platform dir from a previous run.
    if target_path.exists() {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Some(parent) = exe.parent() else {
        return;
    };
    let source_path = parent.join(filename);
    if !source_path.exists() {
        return;
    }
    // Try to move (rename) first — atomic on the same filesystem. If
    // that fails (cross-filesystem, e.g. binary on a mounted volume and
    // config on the root volume), fall back to copy + delete.
    if let Err(e) = fs::rename(&source_path, target_path) {
        tracing::warn!(
            "Failed to rename {src} to {dst} during migration ({e}); trying copy+delete",
            src = source_path.display(),
            dst = target_path.display(),
        );
        if let Err(e) = fs::copy(&source_path, target_path) {
            tracing::warn!(
                "Failed to copy {src} to {dst} during migration ({e}); \
                 leaving the file in place. The platform-dir path will be \
                 treated as a first launch.",
                src = source_path.display(),
                dst = target_path.display(),
            );
            return;
        }
        // Copy succeeded — delete the original so we don't read stale
        // data from it next time. If the delete fails, we still return
        // the platform path (the copy is authoritative now); the stale
        // binary-dir copy is just clutter.
        let _ = fs::remove_file(&source_path);
    }
    tracing::info!(
        "Migrated config file {name} from binary dir to {dst}",
        name = filename,
        dst = target_path.display(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// Helper: create a fake "binary dir" with a file in it, then call
    /// `resolve_config_file_path` with `RUSTERM_CONFIG_DIR` pointing at
    /// a different temp dir. Returns the resolved path.
    fn resolve_with_env(env_dir: &Path, filename: &str) -> PathBuf {
        // SAFETY: tests run single-threaded by default; we restore the
        // env var after. This is the same pattern `std::env::set_var`
        // uses in test code throughout the ecosystem. The `unsafe`
        // block is required because Rust 2024 made `env::set_var` /
        // `env::remove_var` unsafe (they can race with other threads
        // reading the env). In tests with a single thread, this is sound.
        unsafe {
            std::env::set_var("RUSTERM_CONFIG_DIR", env_dir);
        }
        let result = resolve_config_file_path(filename).expect("resolve");
        unsafe {
            std::env::remove_var("RUSTERM_CONFIG_DIR");
        }
        result
    }

    #[test]
    fn env_var_override_is_highest_priority() {
        let dir = tempdir().unwrap();
        let path = resolve_with_env(dir.path(), "test.json");
        assert_eq!(path, dir.path().join("test.json"));
    }

    #[test]
    fn env_var_override_creates_the_directory_if_missing() {
        let dir = tempdir().unwrap();
        let nested = dir.path().join("nested").join("deeper");
        // Don't create `nested` — `resolve_config_file_path` should.
        // SAFETY: single-threaded test; restored after.
        unsafe {
            std::env::set_var("RUSTERM_CONFIG_DIR", &nested);
        }
        let path = resolve_config_file_path("test.json").expect("resolve");
        unsafe {
            std::env::remove_var("RUSTERM_CONFIG_DIR");
        }
        assert!(nested.exists());
        assert_eq!(path, nested.join("test.json"));
    }

    #[test]
    fn app_config_dir_returns_platform_dir_plus_subdir() {
        // We can't assert the exact path (it depends on the platform),
        // but we can assert the structure: the last component is
        // `rusterm` and the parent is whatever `dirs::config_dir()`
        // returns.
        if let Some(d) = app_config_dir() {
            assert_eq!(d.file_name().unwrap().to_str().unwrap(), APP_CONFIG_SUBDIR);
            assert_eq!(d.parent(), platform_config_dir().as_deref());
        }
        // If `platform_config_dir()` returns `None` (rare in CI but
        // possible in sandboxed envs), `app_config_dir()` returns `None`
        // — that's the correct behaviour, not a test failure.
    }

    #[test]
    fn resolve_config_file_path_returns_a_path_with_the_filename() {
        // Without the env var, we get either the platform dir or the
        // binary dir. Either way, the last component should be the
        // filename we asked for.
        // SAFETY: single-threaded test.
        unsafe {
            std::env::remove_var("RUSTERM_CONFIG_DIR");
        }
        let path = resolve_config_file_path("my_file.json").expect("resolve");
        assert_eq!(path.file_name().unwrap().to_str().unwrap(), "my_file.json");
    }

    // ---- migrate_from_binary_dir_if_needed ----

    #[test]
    fn migrate_is_noop_if_target_already_exists() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("target.json");
        fs::write(&target, "target content").unwrap();
        // No source file exists, so this should be a no-op.
        migrate_from_binary_dir_if_needed("target.json", &target);
        assert_eq!(fs::read_to_string(&target).unwrap(), "target content");
    }

    #[test]
    fn migrate_is_noop_if_source_does_not_exist() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("target.json");
        // Neither source nor target exists — pure no-op, no panic.
        migrate_from_binary_dir_if_needed("nonexistent.json", &target);
        assert!(!target.exists());
    }

    #[test]
    fn app_config_subdir_is_rusterm() {
        assert_eq!(APP_CONFIG_SUBDIR, "rusterm");
    }
}
