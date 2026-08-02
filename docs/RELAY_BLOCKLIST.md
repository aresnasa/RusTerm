# Relay API Dangerous-Command Blocklist

The RusTerm relay API (`rusterm-relay`) validates every command submitted to
`POST /api/v1/exec` (and `POST /api/v1/script`) against a **layered,
non-bypassable** dangerous-command policy before it ever reaches an SSH exec
channel.

## Layered defense

Validation runs in this strict order — each layer can only *add*
restrictions, never remove the ones above it:

1. **Hardcoded terminal safety patterns** (from `CommandSafetyChecker`).
   Catches `rm -rf /`, `dd of=/dev/sd*`, `mkfs`, fork bombs,
   `chmod -R 777 /`, shutdown/reboot. **Non-bypassable.**
2. **Hardcoded API-specific patterns**. Catches API-only abuse: `curl|sh`,
   `eval`, base64-obfuscated exec, `kill -9 1`, firewall disabling, SELinux
   off, authorized_keys/sshd tampering, history wiping, user management,
   recursive chmod on system trees, crontab/kernel-module tampering,
   long-form `rm --recursive --force /`, `find / -delete`, `telinit 0/6`,
   sysrq-trigger, nsenter into PID 1, `pivot_root` on `/`. **Non-bypassable.**
3. **User + skill patterns** from `relay-blocklist.json` (see below).
   Operator- and skill-contributed regexes. Can only add blocks; the hard
   floor above always wins.
4. **Read-only mutation check** — read-only accounts may not run commands
   classified as mutating.
5. **Per-account allowlist** — a positive regex set; if non-empty, only
   matching commands are allowed.

Because the catastrophic patterns (layers 1–2) run first and
unconditionally, no user/skill/allowlist configuration can weaken them. A
misconfigured per-account allowlist of `.*` still cannot run `rm -rf /`.

## The blocklist config file (`relay-blocklist.json`)

Layers 1 and 2 are compiled into the binary and cannot be changed without a
rebuild. Layer 3 is **user-extensible** via a JSON file,
`relay-blocklist.json`, which lives in the app config directory alongside
`relay.json`:

| Platform | Path |
|----------|------|
| Linux    | `~/.config/rusterm/relay-blocklist.json` |
| macOS    | `~/Library/Application Support/rusterm/relay-blocklist.json` |
| Windows  | `%APPDATA%\rusterm\relay-blocklist.json` |

The `RUSTERM_CONFIG_DIR` environment variable overrides the directory.

### File format

```json
{
  "patterns": [
    { "regex": "\\bnc\\s+-e", "reason": "reverse shell via nc" },
    { "regex": "^python3 -c 'import os;.*os\\.system'", "reason": "python -c shell escape" }
  ],
  "skills": [
    {
      "name": "dbadmin-skill",
      "patterns": [
        { "regex": "\\bDROP\\s+DATABASE", "reason": "DROP DATABASE via skill" }
      ]
    }
  ]
}
```

| Field | Type | Description |
|-------|------|-------------|
| `patterns` | array | Operator-contributed patterns, applied to every account. |
| `patterns[].regex` | string | Regular expression matched against the full command line. Uses the [`regex` crate](https://docs.rs/regex) syntax (not shell-glob — escape metacharacters you want to match literally). |
| `patterns[].reason` | string | Human-readable reason shown in the audit log and the 403 response body. |
| `skills` | array | Skill-contributed pattern groups. Flattened at load time; the skill name is prepended to each pattern's reason for attribution. |
| `skills[].name` | string | Skill identifier (e.g. `"dbadmin-skill"`). Free-form, for audit logs. |
| `skills[].patterns` | array | Same shape as top-level `patterns`. |

### Behavior

- **Missing file**: treated as an empty blocklist (first-launch normal state).
  Only the hardcoded patterns apply.
- **Invalid regex**: reported via `tracing::warn!` at startup but does **not**
  abort. The bad pattern is skipped; the rest are loaded. This means one bad
  pattern from a skill can't take down the API.
- **Match order**: hardcoded catastrophic patterns always run first. User/skill
  patterns run after. A user pattern can only *block* a command that the hard
  floor would have allowed — it can never *un-block* a catastrophic command.
- **Attribution**: when a skill-contributed pattern matches, the block reason
  is prefixed with `skill:<name>: ` so the audit log shows which skill flagged
  the command.

### Example: blocking reverse shells

```json
{
  "patterns": [
    { "regex": "\\bnc\\s+-e\\b", "reason": "reverse shell via nc -e" },
    { "regex": "\\bbash\\s+-i\\s+>\\s*&\\s*/dev/tcp", "reason": "bash reverse shell via /dev/tcp" },
    { "regex": "\\bpython3?\\s+-c\\s+['\"].*socket", "reason": "python reverse shell via -c" }
  ]
}
```

### Example: a skill contributing patterns

A skill named `dbadmin-skill` that wants to block destructive SQL:

```json
{
  "skills": [
    {
      "name": "dbadmin-skill",
      "patterns": [
        { "regex": "\\bDROP\\s+DATABASE\\b", "reason": "DROP DATABASE" },
        { "regex": "\\bTRUNCATE\\s+TABLE\\b", "reason": "TRUNCATE TABLE" },
        { "regex": "\\bDELETE\\s+FROM\\s+\\w+\\s*(?:;|$)", "reason": "DELETE without WHERE" }
      ]
    }
  ]
}
```

When `psql -c 'DROP DATABASE prod'` is submitted, the API returns:

```
403 Forbidden
{ "error": "skill:dbadmin-skill: DROP DATABASE" }
```

## Hardcoded patterns (non-bypassable)

These patterns are compiled into the binary and cannot be weakened by
configuration. They are defined in `crates/rusterm-relay/src/validator.rs`
(the API-specific list) and `crates/rusterm-core/src/command_safety.rs` (the
terminal safety list). Key examples:

| Pattern | Reason |
|---------|--------|
| `rm -rf /` (and `/*`, `~`, `.`, `*`) | recursive force delete of root/home/cwd |
| `rm --recursive --force /` | GNU long-form variant of the above |
| `dd ... of=/dev/sd*` | overwrite a real disk |
| `mkfs.* /dev/sd*` | reformat a block device |
| `> /dev/sd*` | redirect output into a block device |
| `:(){ :|:& };:` | fork bomb |
| `chmod -R 777 /` | world-writable filesystem |
| `chmod 000 /` | lock out the filesystem |
| `find / -delete` | delete everything via find |
| `shutdown`/`reboot`/`halt`/`poweroff` | system shutdown |
| `systemctl poweroff|reboot|halt` | systemctl system power control |
| `telinit 0/6` | shutdown/reboot runlevel |
| `curl ... \| sh` | download-and-execute |
| `eval ...` | eval of untrusted text |
| `kill -9 1` | kill init |
| `iptables -F` | flush firewall rules |
| `setenforce 0` | disable SELinux |
| `authorized_keys` | modifying SSH authorized_keys |
| `> /etc/sshd_config` / `/etc/passwd` / `/etc/shadow` / `/etc/sudoers` | overwriting system auth config |
| `history -c` | clearing shell history |
| `useradd`/`userdel`/`usermod`/`passwd` | account management |
| `chmod -R ... /etc` / `/boot` / `/usr` / ... | recursive permission change on system tree |
| `crontab -r` | crontab modification |
| `insmod`/`rmmod` | kernel module load/unload |
| `nsenter --target 1` | host namespace escape |
| `pivot_root /` | host filesystem takeover |
| `> /proc/sysrq-trigger` | force crash/reboot |
| `$(rm -rf /)` / `` `rm -rf /` `` | dangerous command inside command substitution |

## Security notes

- **Denylist-only is weak by design.** The blocklist is a safety net, not a
  primary control. For accounts with a bounded command set, use the
  per-account `allowed_commands` allowlist (in `relay.json`) — it's far
  stronger than deny-listing.
- **String matching has limits.** A determined attacker can obscure commands
  via env vars, aliases, or shell chaining. The validator checks for chaining
  (`;`, `&&`, `|`) and command substitution (`$(...)`, backticks), but
  multi-line state (shell continuations) is not tracked. This is an accepted
  trade-off — the hard floor catches the catastrophic cases, and the
  per-account allowlist confines the rest.
- **No "allow" concept in the blocklist.** The blocklist file can only *add*
  blocks. There is no way to un-block a hardcoded catastrophic pattern via
  config — that would defeat the purpose.
