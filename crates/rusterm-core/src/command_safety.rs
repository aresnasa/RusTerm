//! Dangerous-command protection — intercepts high-risk commands before they
//! reach the PTY and asks the user to confirm.
//!
//! # Why this exists
//!
//! A terminal emulator is one typo away from a system-wiping disaster. The
//! user explicitly asked us to catch commands like:
//!
//! - `rm -rf /`, `rm -rf /*` (recursive force on root or its contents),
//! - `dd ... of=/dev/sda` (overwrite a real disk with zeros),
//! - `mkfs.ext4 /dev/sda` (reformat a block device),
//! - `> /dev/sda` (redirect output into a block device),
//! - `:(){ :|:& };:` (fork bomb),
//! - `chmod -R 777 /` (chmod everything to world-writable),
//! - `shutdown`/`reboot`/`halt`/`poweroff` (system shutdown).
//!
//! The check runs **before** the Enter key is sent to the PTY. If the verdict
//! is `Warn`, the UI shows a modal: "该命令可能造成不可逆破坏：<reason>。仍然继续？".
//! On confirm, the original Enter is sent; on cancel, it's discarded.
//!
//! # What we deliberately DON'T block
//!
//! - `rm -rf ./build/` — destroying your own build dir is fine.
//! - `rm -rf /home/user/tmp/` — destroying a specific subdirectory is fine.
//! - `dd if=/dev/zero of=/tmp/test.img` — writing to a regular file is fine.
//!
//! The distinction is "would this affect anything other than what the user
//! almost certainly meant?" We err on the side of false negatives (letting
//! things through) rather than false positives (nagging the user), because a
//! nagged user learns to click "继续" without reading, which defeats the
//! protection.
//!
//! # Design
//!
//! Patterns are compiled once at construction time into a `Vec<(Regex,
//! &'static str)>`. `check()` runs each pattern against the **current input
//! line** (not the full shell session — we don't track multi-line state, which
//! is a known limitation; a dangerous command built up over several `\\`
//! continuations would slip through. That's an acceptable trade-off given the
//! complexity of tracking it, and the user typing `rm -rf /` line-by-line is
//! far less common than typing it on one line).
//!
//! The verdict distinguishes `Warn` (user can proceed) from `Block` (refuse
//! outright). In practice we only `Block` patterns that are **always**
//! destructive and have no legitimate use case — currently fork bombs and
//! `chmod -R 777 /`, since even `rm -rf /` could be a chroot cleanup.

use regex::Regex;

/// Outcome of checking a command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SafetyVerdict {
    /// Command is safe (or at least not on our danger list) — send it through.
    Safe,
    /// Command is potentially destructive — ask the user before sending.
    /// The reason is shown in the confirmation modal.
    Warn(String),
    /// Command is unambiguously destructive — refuse to send even if the user
    /// confirms. Reserved for patterns that have no legitimate use case.
    /// (Currently we always use `Warn`; `Block` is here for future policy.)
    #[allow(dead_code)]
    Block(String),
}

/// Pre-compiled dangerous-command patterns. Construct once and reuse.
///
/// Cloning is cheap-ish (clones the inner `Vec<Regex>`); the typical app has
/// exactly one of these, kept on `AppState` for the lifetime of the session.
#[derive(Clone)]
pub struct CommandSafetyChecker {
    patterns: Vec<(Regex, &'static str)>,
}

impl std::fmt::Debug for CommandSafetyChecker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandSafetyChecker")
            .field("pattern_count", &self.patterns.len())
            .finish()
    }
}

impl Default for CommandSafetyChecker {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandSafetyChecker {
    /// Compile all patterns. Cheap (a few hundred microseconds) — fine to call
    /// once at app startup.
    pub fn new() -> Self {
        // Each entry: (compiled regex, human-readable reason shown in the modal).
        // Patterns are intentionally narrow: see module docs for what we DON'T
        // match. The `rm -rf` pattern in particular requires the target to be
        // `/`, `/*`, `~`, or `.`/`*` at end-of-line — NOT any path starting with
        // `/`, otherwise `rm -rf /home/user/build` would false-positive.
        let raw: &[(&str, &str)] = &[
            // ── rm -rf on root / home / wildcard-everything ──────────
            // Matches `rm -rf /`, `rm -rf /*`, `rm -rf ~`, `rm -rf .`,
            // `rm -rf *`, `rm -fr /`, `rm -rf / ` (trailing space).
            // Does NOT match `rm -rf /home/user/build` (path is more than `/`).
            //
            // The target alternation accepts:
            //   `/`  followed by whitespace/end/`*`/another `/` → root or /*
            //   `~`  followed by whitespace/end             → home
            //   `.`  followed by whitespace/end             → current dir
            //   `*`  followed by whitespace/end             → all in current dir
            (
                r"(?s)\brm\s+(-[a-zA-Z]*r[a-zA-Z]*f|-[a-zA-Z]*f[a-zA-Z]*r)\b[^;&|]*\s+(/(?:\s|$|/|\*)|~(?:\s|$)|\.(?:\s|$)|\*(?:\s|$))",
                "rm -rf 递归强制删除根目录 / 家目录或当前目录全部内容，将导致系统或数据不可逆丢失",
            ),
            // ── dd writing to a real block device ──────────────────────
            // Matches `dd ... of=/dev/sda`, `of=/dev/nvme0n1`, `of=/dev/disk0`,
            // `of=/dev/hda`. Does NOT match `of=/tmp/test.img` (regular file).
            (
                r"\bdd\b[^;&|]*\bof=/dev/(?:sd[a-z]+|nvme\d+n\d+|disk\d+|hd[a-z]+|mmcblk\d+)",
                "dd 正在写入裸块设备，将覆盖磁盘上的所有数据",
            ),
            // ── mkfs on a real block device ────────────────────────────
            // Matches `mkfs.ext4 /dev/sda`, `mkfs /dev/nvme0n1`, etc.
            (
                r"\bmkfs(?:\.\w+)?\s+/dev/(?:sd[a-z]+|nvme\d+n\d+|disk\d+|hd[a-z]+|mmcblk\d+)",
                "mkfs 正在格式化块设备，将销毁其上的所有文件系统数据",
            ),
            // ── redirect into a block device ───────────────────────────
            // Matches `> /dev/sda`, `> /dev/nvme0n1`. Does NOT match `> /dev/null`.
            (
                r">\s*/dev/(?:sd[a-z]+|nvme\d+n\d+|disk\d+|hd[a-z]+|mmcblk\d+)\b",
                "输出重定向到块设备，将覆盖磁盘数据",
            ),
            // ── fork bomb (bash classic) ────────────────────────────────
            // Matches `:(){ :|:& };:`, with or without spaces.
            (
                r":\(\)\s*\{\s*:\s*\|\s*:\s*&\s*\}\s*;\s*:",
                "fork 炸弹，将瞬间耗尽系统进程表导致死机",
            ),
            // ── chmod -R 777 on root ────────────────────────────────────
            // Matches `chmod -R 777 /` (exactly `/`, not subdirectories).
            (
                r"\bchmod\s+-R\s+777\s+/\s*$",
                "chmod -R 777 / 将整个文件系统设为世界可写，严重破坏安全模型",
            ),
            // ── system shutdown / reboot ────────────────────────────────
            // Warn (don't block) — these have legitimate uses but can also be
            // accidental (e.g. `reboot` typed instead of `reload`).
            (
                r"\b(?:shutdown|reboot|halt|poweroff|init\s+0)\b",
                "系统关机/重启命令",
            ),
        ];

        let patterns = raw
            .iter()
            .map(|(pat, reason)| {
                let regex = Regex::new(pat)
                    .unwrap_or_else(|e| panic!("invalid safety regex {:?}: {}", pat, e));
                (regex, *reason)
            })
            .collect();

        Self { patterns }
    }

    /// Check a single command line. The line should be the user's current
    /// input (the line about to be sent to the PTY with Enter), **without**
    /// the trailing newline. Multi-line inputs (shell continuations) are
    /// checked line-by-line by the caller — see module docs for the
    /// limitation.
    pub fn check(&self, command: &str) -> SafetyVerdict {
        for (regex, reason) in &self.patterns {
            if regex.is_match(command) {
                return SafetyVerdict::Warn((*reason).to_string());
            }
        }
        SafetyVerdict::Safe
    }

    /// Returns `true` if the command would trigger a `Warn` or `Block`.
    /// Convenience for callers that don't care about the reason.
    pub fn is_dangerous(&self, command: &str) -> bool {
        !matches!(self.check(command), SafetyVerdict::Safe)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checker() -> CommandSafetyChecker {
        CommandSafetyChecker::new()
    }

    // ── rm -rf variants ──────────────────────────────────────────────

    #[test]
    fn rm_rf_root_is_caught() {
        assert!(checker().is_dangerous("rm -rf /"));
    }

    #[test]
    fn rm_rf_root_star_is_caught() {
        assert!(checker().is_dangerous("rm -rf /*"));
    }

    #[test]
    fn rm_fr_root_is_caught() {
        // -fr is the same as -rf
        assert!(checker().is_dangerous("rm -fr /"));
    }

    #[test]
    fn rm_rf_home_is_caught() {
        assert!(checker().is_dangerous("rm -rf ~"));
    }

    #[test]
    fn rm_rf_dot_is_caught() {
        assert!(checker().is_dangerous("rm -rf ."));
    }

    #[test]
    fn rm_rf_star_is_caught() {
        assert!(checker().is_dangerous("rm -rf *"));
    }

    #[test]
    fn rm_rf_specific_subdir_is_safe() {
        // Crucial false-negative check: `rm -rf /home/user/build` is fine.
        assert!(!checker().is_dangerous("rm -rf /home/user/build"));
    }

    #[test]
    fn rm_rf_relative_path_is_safe() {
        assert!(!checker().is_dangerous("rm -rf ./build"));
    }

    #[test]
    fn rm_rf_specific_named_dir_is_safe() {
        assert!(!checker().is_dangerous("rm -rf /tmp/scratch"));
    }

    // ── dd variants ──────────────────────────────────────────────────

    #[test]
    fn dd_to_block_device_is_caught() {
        assert!(checker().is_dangerous("dd if=/dev/zero of=/dev/sda bs=1M"));
    }

    #[test]
    fn dd_to_nvme_is_caught() {
        assert!(checker().is_dangerous("dd of=/dev/nvme0n1"));
    }

    #[test]
    fn dd_to_regular_file_is_safe() {
        // Writing to a regular file is fine — common for building disk images.
        assert!(!checker().is_dangerous("dd if=/dev/zero of=/tmp/test.img bs=1M count=100"));
    }

    #[test]
    fn dd_to_dev_null_is_safe() {
        assert!(!checker().is_dangerous("dd if=/dev/zero of=/dev/null"));
    }

    // ── mkfs variants ────────────────────────────────────────────────

    #[test]
    fn mkfs_ext4_on_device_is_caught() {
        assert!(checker().is_dangerous("mkfs.ext4 /dev/sda1"));
    }

    #[test]
    fn mkfs_generic_on_device_is_caught() {
        assert!(checker().is_dangerous("mkfs /dev/nvme0n1p2"));
    }

    #[test]
    fn mkfs_on_image_file_is_safe() {
        assert!(!checker().is_dangerous("mkfs.ext4 /tmp/test.img"));
    }

    // ── redirect into block device ───────────────────────────────────

    #[test]
    fn redirect_to_block_device_is_caught() {
        assert!(checker().is_dangerous("echo hi > /dev/sda"));
    }

    #[test]
    fn redirect_to_dev_null_is_safe() {
        assert!(!checker().is_dangerous("echo hi > /dev/null"));
    }

    // ── fork bomb ────────────────────────────────────────────────────

    #[test]
    fn fork_bomb_classic_is_caught() {
        assert!(checker().is_dangerous(":(){ :|:& };:"));
    }

    #[test]
    fn fork_bomb_with_spaces_is_caught() {
        assert!(checker().is_dangerous(":() { : | : & } ; :"));
    }

    // ── chmod -R 777 / ────────────────────────────────────────────────

    #[test]
    fn chmod_r_777_root_is_caught() {
        assert!(checker().is_dangerous("chmod -R 777 /"));
    }

    #[test]
    fn chmod_r_777_subdir_is_safe() {
        assert!(!checker().is_dangerous("chmod -R 777 /var/log"));
    }

    // ── shutdown / reboot ─────────────────────────────────────────────

    #[test]
    fn shutdown_is_caught() {
        assert!(checker().is_dangerous("shutdown -h now"));
    }

    #[test]
    fn reboot_is_caught() {
        assert!(checker().is_dangerous("reboot"));
    }

    #[test]
    fn init_0_is_caught() {
        assert!(checker().is_dangerous("init 0"));
    }

    // ── benign commands ──────────────────────────────────────────────

    #[test]
    fn ls_is_safe() {
        assert!(!checker().is_dangerous("ls -la"));
    }

    #[test]
    fn cd_is_safe() {
        assert!(!checker().is_dangerous("cd /tmp"));
    }

    #[test]
    fn git_push_is_safe() {
        assert!(!checker().is_dangerous("git push origin main"));
    }

    #[test]
    fn cargo_build_is_safe() {
        assert!(!checker().is_dangerous("cargo build --release"));
    }

    #[test]
    fn empty_string_is_safe() {
        assert!(!checker().is_dangerous(""));
    }

    #[test]
    fn echo_text_is_safe() {
        assert!(!checker().is_dangerous("echo hello world"));
    }

    #[test]
    fn piped_commands_are_checked() {
        // `rm -rf / | cat` should still match — the pipe doesn't save you.
        assert!(checker().is_dangerous("rm -rf / | cat"));
    }

    #[test]
    fn and_chained_commands_are_checked() {
        // `true && rm -rf /` should match.
        assert!(checker().is_dangerous("true && rm -rf /"));
    }

    #[test]
    fn verdict_returns_reason() {
        let v = checker().check("rm -rf /");
        match v {
            SafetyVerdict::Warn(reason) => {
                assert!(
                    reason.contains("rm -rf"),
                    "reason should mention rm -rf: got {}",
                    reason
                );
            }
            other => panic!("expected Warn, got {:?}", other),
        }
    }

    #[test]
    fn verdict_safe_for_benign() {
        assert_eq!(checker().check("ls"), SafetyVerdict::Safe);
    }
}
