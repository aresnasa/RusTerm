# RusTerm Logging & Sensitive Data Policy

This document describes what RusTerm logs, what it never logs, where logs
live, how long they're kept, and how sensitive user data is protected at
rest. It is the canonical reference for developers adding new log calls or
handling user data.

## TL;DR

| Concern | Storage | Encrypted | Sent off-machine |
|---|---|---|---|
| Runtime logs (`tracing`) | `data_local_dir/rusterm/logs/` | No (operational only) | Never |
| Session I/O logs (`SessionLog`) | `data_dir/rusterm/session_logs/` | Yes (AES-256-GCM, per-session key) | Never |
| Connection credentials | `settings.json` next to binary | Yes (AES-256-GCM, master key) | Never |
| Master key | OS keyring (or Argon2-derived from machine ID) | OS-managed | Never |

## 1. Runtime logs (`tracing`)

### What is logged

Runtime logs are **operational only**. They contain:

- Application lifecycle (start, shutdown, version).
- Session lifecycle (created, closed) — identified by **opaque session ID**
  (a UUID), never by hostname, username, or connection name.
- Connection outcomes (success / failure with error *category* — not raw
  error messages that could embed credentials).
- Performance counters (bytes processed, render latency, plugin invocation
  duration, scrollback size).
- Crash-level errors with stack-free context.

### What is NEVER logged

The following must never appear in runtime logs:

- **Terminal I/O**: keystrokes, command output, pane contents, scrollback
  buffer. This data is handled separately by `SessionLog` (see §2).
- **Credentials**: passwords, SSH keys, passphrases, tokens, cookies, bearer
  tokens, API keys, OAuth codes, OneKey `send` values.
- **Personally identifying information**: usernames, real names, emails,
  hostnames, IP addresses, home-directory paths (log relative paths only,
  never absolute paths under the user's home).
- **Encryption keys**: the master key, per-session keys, derived keys,
  nonces, salts used for key derivation.
- **Command-line arguments** that may carry secrets (e.g.
  `--password=...`). Log only the program name and a redacted arg list.
- **Environment variable values**. Log only the *names* of env vars that
  affect behavior, never their values.

### Where logs live

```
macOS:   ~/Library/Application Support/rusterm/logs/rusterm.log
Linux:   ~/.local/share/rusterm/logs/rusterm.log
Windows: %LOCALAPPDATA%\rusterm\logs\rusterm.log
```

Logs are written via `tracing_appender::rolling::daily`, which produces
files named `rusterm.log.YYYY-MM-DD`. The previous day's file is rotated
automatically. Logs **never leave the user's machine** — there is no
telemetry, crash-report upload, or remote log shipping.

### Log format

JSON (one record per line), no ANSI colors. Example:

```json
{"timestamp":"2024-...","level":"INFO","target":"rusterm_core::session",
 "fields":{"message":"session created","id":"abc12345"}}
```

### Log level

Default level is `rusterm=info`. Override with `RUST_LOG`:

```sh
RUST_LOG=rusterm=debug cargo run       # verbose
RUST_LOG=rusterm=trace cargo run       # very verbose (per-keystroke lifecycle)
RUST_LOG=off cargo run                 # silence everything
```

### Redaction safety net

Even if a developer accidentally writes
`tracing::info!("password={}", pwd)`, the `RedactingMakeWriter` installed by
`rusterm_core::logging::init_logging` scans each formatted record for known
secret patterns and replaces them with `<redacted>` *before* the record
reaches disk. Patterns caught include:

- `password=...`, `passwd=...`, `pass=...`
- `token=...`, `api_key=...`, `apikey=...`, `secret=...`, `bearer=...`,
  `credential=...`
- `Authorization: Bearer ...` / `Basic ...` headers
- PEM private-key blocks (`-----BEGIN ... PRIVATE KEY-----`)
- JWT-shaped strings (`ey...`)
- Generic long base64/hex values assigned to key-ish names

This is **defense in depth**. Code must still avoid logging secrets in the
first place; the redactor is a backstop, not a license to log freely.

### Third-party crate silence

The following third-party crates are silenced via `EnvFilter` `target=off`
directives, because they may log URLs, headers, or args that could carry
credentials:

- `hyper`, `reqwest` (HTTP clients — URLs may embed `user:pass@host`)
- `russh`, `russh_keys` (SSH — could log key paths / auth state)
- `async_openai` (AI — could log prompts)
- `wasmtime`, `wasmtime_wasi` (plugins — could log anything a plugin does)

### Retention

`tracing_appender::rolling::daily` does **not** auto-delete old files.
Retention is the user's responsibility (the logs dir is documented above so
users can `rm` old files or set up logrotate/cron). A future enhancement
may cap retention to N days automatically.

## 2. Session I/O logs (`SessionLog`)

`SessionLog` is **not** part of the runtime log. It records what the user
typed and what the terminal displayed during a session, so the user can
review past sessions. This is sensitive user data and is treated
completely differently from runtime logs.

### Privacy contract

- **Local only**. Never sent anywhere.
- **Encrypted at rest** with AES-256-GCM.
- **Per-session keys**: each session's log uses a key derived from the
  RusTerm master key + the session ID. Compromising one log file does not
  reveal data from other sessions.
- **No plaintext on disk** at any time, including in temporary buffers.
- **Filename**: `<safe_session_id>_<timestamp>.rusl`. The session ID in the
  filename is sanitized to alphanumerics + `-`/`_` and truncated to 36
  chars, so it cannot leak arbitrary user-controlled text.

### File format

```text
magic:    b"RUSL"  (4 bytes)
version:  u8 = 1
reserved: [u8; 3] = [0, 0, 0]
records:  zero or more of:
  length:     u32 big-endian (size of ciphertext that follows)
  ciphertext: <length> bytes  = nonce[12] || aead-sealed plaintext
```

The plaintext payload of each record is a small JSON object:

```json
{"t":"<RFC3339 timestamp>","d":"<IN|OUT>","b":"<base64 bytes>"}
```

JSON is used so the encrypted record carries its own timestamp and
direction metadata, avoiding the need for a binary schema and making
future format additions backward-compatible.

### Key derivation

```
master_key  ──┐
              ├── Argon2id ──> per-session AEAD key (32 bytes)
session_id ──┘
```

The master key itself comes from either:

1. The user's master password (via `ConfigManager::with_master_password`),
   Argon2id-derived; or
2. The OS keyring (legacy / no-master-password mode); or
3. The machine ID (last-resort fallback when the OS keyring is
   unavailable).

The per-session AEAD key is held in `Zeroizing` memory and wiped on drop.

### What happens when the app is locked

If `ConfigManager` is not available (the app is locked / first-run /
no master password set), `create_terminal` **skips creating a session log**
rather than falling back to plaintext. It's better to lose session-log
functionality than to write terminal I/O to disk unencrypted.

## 3. Credentials at rest

SSH passwords, SSH key passphrases, and OneKey `send` values are stored
encrypted in `settings.json` next to the binary (or under
`$RUSTERM_CONFIG_DIR` / the platform config dir). The on-disk format uses
`EncryptedValue { _encrypted: String }` where the string is base64 of
`nonce || AES-256-GCM ciphertext`.

The `Debug` impl of `EncryptedValue`, `SshAuth`, `OneKeyStep`, and
`ShellConfig` (env values) all redact their secret fields, so
`tracing::debug!(?config)` cannot accidentally leak credentials.

## 4. Developer guidelines

When adding new code:

1. **Never log raw user data**. If you need to log "user did X", log
   `event=user_action kind=enter` — not the keystroke itself.
2. **Never log `Vec<u8>` payloads**. Log their length:
   `tracing::debug!(bytes=data.len())`.
3. **Never log absolute paths under the user's home**. Log relative paths
   or just filenames.
4. **Never log env var values**. Log only the names of env vars you read.
5. **Never log hostnames, usernames, IP addresses**. Log opaque IDs.
6. **Never derive `Debug` on a struct that holds secrets**. Implement
   `Debug` manually and redact the secret fields. See `SshAuth` for an
   example.
7. **Prefer structured fields over formatted strings**:
   `tracing::info!(session_id = %id, kind = ?kind)` — this lets the
   redactor inspect field values individually and is easier to grep.
8. **When in doubt, leave it out**. Logs are for ops debugging; if a field
   isn't directly useful for ops, don't log it.

## 5. Testing

- `crates/rusterm-core/src/logging.rs` has unit tests for each redaction
  pattern.
- `crates/rusterm-core/src/session_log.rs` has unit tests verifying that
  plaintext never appears on disk and that round-trip decryption works.
- `crates/rusterm-core/tests/logging_privacy.rs` is an integration test
  that emits a fake password / JWT / PEM block through a real subscriber
  and asserts none of them appear in the captured output.

When adding a new redaction pattern or a new secret-bearing type, add a
test that would fail if the protection were removed.
