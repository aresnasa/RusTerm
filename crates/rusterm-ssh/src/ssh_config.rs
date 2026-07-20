//! Reading the user's local OpenSSH configuration (`~/.ssh/config` and the
//! `~/.ssh/id_*` identity files) so the connection dialog can **suggest**
//! paths and hosts instead of forcing the user to type them by hand.
//!
//! ## Why this module exists
//!
//! Before this module, the New/Edit SSH Connection dialog required the user
//! to manually type every field — the host alias, the username, the port,
//! the identity file path. That's tedious and error-prone: most users already
//! have a working `~/.ssh/config` with `Host` aliases that encode all of
//! this information, plus a set of `~/.ssh/id_*` private keys. This module
//! reads those files and exposes a small, well-typed surface that the UI
//! can render as `<datalist>` autocomplete suggestions and "auto-fill from
//! ~/.ssh/config" behaviour.
//!
//! ## What we expose
//!
//! - [`list_ssh_config_hosts`] — parse `~/.ssh/config` (or an arbitrary
//!   path) and return one [`SshHostSuggestion`] per `Host` directive whose
//!   pattern is a literal host alias (we skip wildcard patterns like `*`
//!   or `*.example.com` because they're not selectable autocomplete
//!   entries — they're pattern matchers, not hostnames).
//! - [`list_identity_files`] — scan `~/.ssh/` (or an arbitrary directory)
//!   for `id_*` private keys, returning absolute paths sorted lexically.
//!   We deliberately exclude `.pub` files, `known_hosts`, `authorized_keys`,
//!   `config`, and `environment` — only files whose name starts with `id`
//!   and don't end in `.pub` are returned, matching the OpenSSH convention
//!   for identity files.
//! - [`lookup_host`] — query a specific host alias via `russh-config`'s
//!   `parse`/`parse_path` API and return the resolved
//!   [`rusterm_core::config::SshConfig`]-shaped data (user/port/hostname/
//!   identity_file/proxy_jump). The UI uses this to auto-fill the form when
//!   the user types a host alias that matches a `~/.ssh/config` entry.
//!
//! ## Failure mode
//!
//! Every function is **fallible but tolerant**: if `~/.ssh/config` doesn't
//! exist (e.g. fresh install, non-Unix OS), or if `~/.ssh/` doesn't exist,
//! the function returns `Ok(vec![])` rather than an error. The dialog
//! should always render; the suggestion list is just empty. Real I/O
//! errors (permission denied, etc.) are returned as `Err` so the caller
//! can log them at `tracing::warn!` if it wants — but the UI's rendering
//! path treats `Err` the same as `Ok(vec![])` (empty list) because a
//! missing suggestion list is never fatal.
//!
//! ## Why we don't use `russh-config` for `list_ssh_config_hosts`
//!
//! `russh-config`'s public API is `parse(file, host) -> Config` — it
//! queries a *single host* by name. The `SshConfig` struct (with the
//! parsed entries list) is private, so we can't iterate over all `Host`
//! directives through `russh-config`'s API. We do a minimal hand-parse
//! here for the list-all-hosts use case, and defer to `russh-config`'s
//! proper parser (which handles `Include`, `Match`, percent tokens, etc.)
//! for the single-host lookup via [`lookup_host`].
//!
//! ## Path handling
//!
//! All returned paths are **absolute** strings (no `~` prefix) so they can
//! be stored verbatim in `SshConfig` (which expects resolved paths —
//! russh doesn't expand `~` for us) and rendered in the UI without further
//! processing. The dialog's "host alias from ~/.ssh/config" suggestion
//! list keeps the alias as the user typed it (e.g. `my-server`), and the
//! auto-fill on match expands the identity file path to absolute.

use std::fs;
use std::path::{Path, PathBuf};

use rusterm_core::config::SshAuth;

/// A single `Host` alias from `~/.ssh/config` plus the resolved settings
/// for that alias.
///
/// `alias` is the literal hostname from the `Host` directive (e.g.
/// `my-server`, never `*` or `*.example.com`). The remaining fields are
/// `Option`s because OpenSSH's config is sparse — most entries only set a
/// few fields, and `None` means "not specified in the config" (the caller
/// should fall back to its defaults: user = current user, port = 22, etc.).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshHostSuggestion {
    /// The literal `Host` alias from `~/.ssh/config`. Never a wildcard.
    pub alias: String,
    /// `HostName` (the real hostname or IP the alias resolves to), if set.
    pub hostname: Option<String>,
    /// `User`, if set.
    pub user: Option<String>,
    /// `Port`, if set.
    pub port: Option<u16>,
    /// `IdentityFile`, if set. Absolute path (tilde expanded).
    pub identity_file: Option<String>,
    /// `ProxyJump`, if set.
    pub proxy_jump: Option<String>,
}

impl SshHostSuggestion {
    /// Build a suggestion with only the alias populated. Used as a sentinel
    /// when the parse encounters a `Host` directive with no following
    /// key/value pairs (rare but legal — it's a no-op entry).
    pub fn from_alias(alias: impl Into<String>) -> Self {
        Self {
            alias: alias.into(),
            hostname: None,
            user: None,
            port: None,
            identity_file: None,
            proxy_jump: None,
        }
    }
}

/// The default `~/.ssh/config` path under the user's home directory.
///
/// Returns `None` if the home directory can't be resolved (very unusual —
/// would mean `HOME` is unset on Unix or the OS profile is corrupt on
/// Windows). The caller should treat `None` as "no config to read".
pub fn default_ssh_config_path() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    Some(home.join(".ssh").join("config"))
}

/// The default `~/.ssh/` directory under the user's home directory.
///
/// Returns `None` under the same conditions as [`default_ssh_config_path`].
pub fn default_ssh_dir() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".ssh"))
}

/// Parse `~/.ssh/config` (or an arbitrary path) and return one
/// [`SshHostSuggestion`] per `Host` directive with a **literal** alias.
///
/// `Host` directives with wildcard patterns (`*`, `?`, `!negated`, etc.)
/// are skipped because they're pattern matchers, not selectable hostnames
/// — putting `*` in an autocomplete dropdown is meaningless.
///
/// Multi-host `Host` directives (`Host a b c`) expand into one suggestion
/// per literal alias.
///
/// The parser is deliberately minimal: it splits on whitespace, lowercases
/// directive keywords (OpenSSH config is case-insensitive on keywords),
/// and handles the small set of directives the dialog cares about
/// (`HostName`, `User`, `Port`, `IdentityFile`, `ProxyJump`). Unknown
/// directives are ignored (they don't break the parse — the user's config
/// is still fully readable, we just don't surface every possible option).
/// `Include` directives are NOT followed (the file the user pointed us at
/// is parsed as-is); if you need `Include` support, use [`lookup_host`]
/// which goes through `russh-config`'s full parser.
///
/// Tilde expansion: `IdentityFile` values starting with `~/` are expanded
/// to the user's home directory (resolved via [`dirs::home_dir`], not
/// string substitution — that handles the `$HOME` env var correctly on
/// Unix and the profile dir on Windows).
pub fn list_ssh_config_hosts_at(path: &Path) -> Vec<SshHostSuggestion> {
    let contents = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    parse_ssh_config_text(&contents)
}

/// Convenience: [`list_ssh_config_hosts_at`] pointed at [`default_ssh_config_path`].
///
/// Returns an empty `Vec` if the config path can't be resolved or the file
/// doesn't exist (the common case on a fresh install).
pub fn list_ssh_config_hosts() -> Vec<SshHostSuggestion> {
    match default_ssh_config_path() {
        Some(p) => list_ssh_config_hosts_at(&p),
        None => Vec::new(),
    }
}

/// Pure parser: take the textual contents of an OpenSSH config file and
/// return one [`SshHostSuggestion`] per literal `Host` alias.
///
/// Exposed separately from [`list_ssh_config_hosts_at`] so it can be
/// unit-tested without touching the filesystem.
pub fn parse_ssh_config_text(contents: &str) -> Vec<SshHostSuggestion> {
    let home = dirs::home_dir();
    let mut out: Vec<SshHostSuggestion> = Vec::new();
    // The "current" suggestion: the `HostConfig` block being accumulated
    // for the most recent `Host` directive(s). `None` means we haven't
    // seen a `Host` directive yet (top-of-file directives before any
    // `Host` apply to all hosts — we don't model that here, mirroring
    // `russh-config`'s behaviour of ignoring pre-Host directives).
    let mut current: Option<SshHostSuggestion> = None;
    // The list of aliases declared by the most recent `Host` directive.
    // OpenSSH allows `Host a b c` to share one block of settings across
    // multiple aliases; we accumulate them here and emit one suggestion
    // per alias at the next `Host` directive (or end of file).
    let mut current_aliases: Vec<String> = Vec::new();

    for raw_line in contents.lines() {
        // Strip comments: OpenSSH treats `#` (and, less commonly, `;`) at
        // the start of a line or after whitespace as a comment to end of
        // line. We trim leading whitespace first so indented comments work
        // too. We also strip INLINE comments (a `#` after whitespace in
        // the middle of a line) by splitting the line at the first ` #`
        // or `\t#` and keeping only the part before it.
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        // Strip inline comments: find the first `#` preceded by
        // whitespace and discard it + everything after. This handles
        // `HostName 1.2.3.4 # the IP` → `HostName 1.2.3.4`. We only
        // strip `#` (not `;`) inline because `;` as an inline
        // separator is rare and OpenSSH's own docs are ambiguous about
        // it (the man page says `#` is the inline comment char).
        let line = strip_inline_comment(line);
        // Split into keyword + value. OpenSSH is whitespace-tolerant: any
        // run of spaces or tabs separates the keyword from the value, and
        // the value itself may contain spaces (e.g. `HostName foo bar`
        // is one value "foo bar"). We split on the FIRST whitespace run
        // only, so `HostName` with a multi-word value (rare but legal)
        // keeps the value intact.
        let mut parts = line.splitn(2, char::is_whitespace);
        let keyword = parts.next().unwrap_or("").to_ascii_lowercase();
        let value_str = parts.next().unwrap_or("").trim();
        // Collapse internal whitespace runs in the value to single spaces
        // (OpenSSH treats runs of whitespace as a single separator in
        // most directives, and this normalises `Host   a   b` to
        // `Host a b`).
        let value: String = value_str.split_whitespace().collect::<Vec<_>>().join(" ");
        if keyword.is_empty() || value.is_empty() {
            continue;
        }

        match keyword.as_str() {
            "host" => {
                // Flush the previous block (if any) — one suggestion per
                // literal alias.
                if !current_aliases.is_empty() {
                    let cfg = current.take().unwrap_or_else(empty_suggestion);
                    for alias in current_aliases.drain(..) {
                        if is_wildcard_pattern(&alias) {
                            continue;
                        }
                        let mut s = cfg.clone();
                        s.alias = alias;
                        out.push(s);
                    }
                }
                // Start a new block: each token in the value is an alias.
                // We collect ALL of them (including wildcards) here so
                // that the next block's settings apply to all aliases;
                // wildcards are filtered out at the emit step above.
                current_aliases = value.split(' ').map(|s| s.to_string()).collect();
                current = None;
            }
            "hostname" => {
                let s = current.get_or_insert_with(empty_suggestion);
                s.hostname = Some(value);
            }
            "user" => {
                let s = current.get_or_insert_with(empty_suggestion);
                s.user = Some(value);
            }
            "port" => {
                if let Ok(p) = value.parse::<u16>() {
                    let s = current.get_or_insert_with(empty_suggestion);
                    s.port = Some(p);
                }
            }
            "identityfile" => {
                let expanded = expand_tilde(&value, home.as_deref());
                let s = current.get_or_insert_with(empty_suggestion);
                // If multiple IdentityFile directives appear in the same
                // block, the first one wins (OpenSSH tries them in order;
                // the first that authenticates is used). We model that by
                // keeping the first and ignoring subsequent ones, which
                // matches the autocomplete use case: suggest the primary
                // identity file.
                if s.identity_file.is_none() {
                    s.identity_file = Some(expanded);
                }
            }
            "proxyjump" => {
                let s = current.get_or_insert_with(empty_suggestion);
                s.proxy_jump = Some(value);
            }
            _ => {
                // Unknown directive — ignore (we still keep accumulating
                // into `current` so a later `HostName` etc. attaches to
                // the right block).
            }
        }
    }

    // Flush the final block.
    if !current_aliases.is_empty() {
        let cfg = current.take().unwrap_or_else(empty_suggestion);
        for alias in current_aliases {
            // Skip wildcard patterns: `*`, `*.example.com`, `?`, `!negated`.
            // A "literal alias" is one with no glob characters.
            if is_wildcard_pattern(&alias) {
                continue;
            }
            let mut s = cfg.clone();
            s.alias = alias;
            out.push(s);
        }
    }

    out
}

fn empty_suggestion() -> SshHostSuggestion {
    SshHostSuggestion {
        alias: String::new(),
        hostname: None,
        user: None,
        port: None,
        identity_file: None,
        proxy_jump: None,
    }
}

/// Strip an inline `#` comment from a config line.
///
/// OpenSSH treats a `#` preceded by whitespace as the start of a comment
/// that runs to end of line. We find the first such `#` and return the
/// part before it (trimmed). If there's no inline comment, return the
/// line as-is (trimmed).
///
/// Examples:
/// - `HostName 1.2.3.4` → `HostName 1.2.3.4`
/// - `HostName 1.2.3.4 # the IP` → `HostName 1.2.3.4`
/// - `# full line comment` → (caller checks `starts_with('#')` first)
/// - `Port 22#weird` → `Port 22#weird` (no whitespace before `#`, so
///   it's part of the value — OpenSSH's actual behaviour)
fn strip_inline_comment(line: &str) -> &str {
    // Look for ` #` or `\t#` (whitespace then `#`).
    for (i, c) in line.char_indices() {
        if c == '#' && i > 0 {
            let prev = line.as_bytes()[i - 1];
            if prev == b' ' || prev == b'\t' {
                return line[..i].trim_end();
            }
        }
    }
    line.trim_end()
}

/// Returns true if the alias contains OpenSSH wildcard characters (`*`,
/// `?`, or `!`-negation prefix). These are pattern matchers, not literal
/// hostnames, so they shouldn't appear in an autocomplete dropdown.
pub fn is_wildcard_pattern(alias: &str) -> bool {
    alias.contains('*') || alias.contains('?') || alias.starts_with('!')
}

/// Expand a leading `~/` (or just `~`) to the user's home directory.
///
/// Returns the original string if `home` is `None` (no home dir resolvable)
/// or if the path doesn't start with `~`. Handles the bare `~` case (with
/// no trailing slash) too — `~` alone expands to the home directory.
pub fn expand_tilde(path: &str, home: Option<&Path>) -> String {
    let Some(home) = home else {
        return path.to_string();
    };
    if path == "~" {
        return home.to_string_lossy().into_owned();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return home.join(rest).to_string_lossy().into_owned();
    }
    path.to_string()
}

/// Scan a directory for OpenSSH identity files (private keys).
///
/// Returns the **absolute paths** of files whose name starts with `id`
/// and does NOT end in `.pub`. This matches the OpenSSH convention:
/// `id_rsa`, `id_ed25519`, `id_ecdsa`, `id_dsa` are private keys;
/// `id_rsa.pub`, etc. are public keys (we don't suggest those — the user
/// wants to authenticate with the private key). We also explicitly skip
/// `config`, `known_hosts`, `authorized_keys`, and `environment` in case
/// any of those happen to start with `id` (they don't by convention, but
/// being explicit is cheaper than reasoning about edge cases).
///
/// Sorted lexically by file name so the suggestion list is stable across
/// runs (the OS's directory iteration order is unspecified).
pub fn list_identity_files_at(dir: &Path) -> Vec<String> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue; // non-UTF-8 name — skip
        };
        if !is_identity_file(name) {
            continue;
        }
        let path = entry.path();
        // Only suggest files (not directories named `id_something`).
        if !path.is_file() {
            continue;
        }
        // Store the original entry's path (a PathBuf) — we convert to
        // String only at the end so the sort operates on the filename
        // portion (not the full path, which could differ in prefix only).
        files.push((name.to_string(), path));
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
        .into_iter()
        .map(|(_, p)| p.to_string_lossy().into_owned())
        .collect()
}

/// Convenience: [`list_identity_files_at`] pointed at [`default_ssh_dir`].
///
/// Returns an empty `Vec` if the home directory can't be resolved or the
/// directory doesn't exist.
pub fn list_identity_files() -> Vec<String> {
    match default_ssh_dir() {
        Some(d) => list_identity_files_at(&d),
        None => Vec::new(),
    }
}

/// Returns true if the given file name looks like an OpenSSH private key.
///
/// Rules:
/// - Must start with `id` (the OpenSSH convention for identity files:
///   `id_rsa`, `id_ed25519`, `id_ecdsa`, `id_dsa`, and custom names like
///   `id_github` that some users create).
/// - Must NOT end with `.pub` (public key — not useful for auth).
/// - Must NOT be one of the well-known non-key files in `~/.ssh/` even
///   though they don't start with `id` (defensive — these are excluded
///   by the `starts_with("id")` check, but listing them explicitly
///   documents the intent).
pub fn is_identity_file(name: &str) -> bool {
    if !name.starts_with("id") {
        return false;
    }
    if name.ends_with(".pub") {
        return false;
    }
    !matches!(
        name,
        "config" | "known_hosts" | "authorized_keys" | "environment"
    )
}

/// The fully-resolved SSH settings for a single host alias, as looked up
/// via `russh-config`'s parser (which handles `Include`, `Match`, percent
/// tokens, etc. — the full OpenSSH config semantics).
///
/// This is the "auto-fill me" payload the UI uses when the user types a
/// host alias that matches a `~/.ssh/config` entry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedHost {
    /// The resolved hostname (from `HostName`, or the alias itself if no
    /// `HostName` directive applies). Never empty.
    pub host: String,
    /// The resolved port (from `Port`, or 22 if not specified).
    pub port: u16,
    /// The resolved user (from `User`, or the current OS user if not
    /// specified — `russh-config` falls back to `whoami::username()`).
    pub user: String,
    /// The resolved identity file path (from `IdentityFile`), if any.
    /// Absolute (tilde expanded by `russh-config`).
    pub identity_file: Option<String>,
    /// The resolved `ProxyJump` value, if any.
    pub proxy_jump: Option<String>,
}

/// Look up the resolved SSH settings for a single host alias.
///
/// This is a two-step process:
/// 1. Check our own [`list_ssh_config_hosts`] to see if the alias is a
///    *literal* `Host` entry in the config. If not, return `None` — we
///    don't want to auto-fill the form with `russh-config`'s default
///    values (current username, port 22) for a host the user just typed
///    that happens to not be in their config.
/// 2. If the alias is in the config, defer to `russh-config`'s full parser
///    (via `parse_path` / `parse_home`) for the *authoritative* resolution.
///    This correctly handles `Include` directives, `Match` blocks, percent
///    tokens (`%h`, `%p`, etc.), and the full OpenSSH config semantics —
///    things our minimal hand-parser in [`parse_ssh_config_text`] doesn't
///    model.
///
/// Returns `None` if:
/// - `path` is `None` and `~/.ssh/config` doesn't exist or can't be read
/// - `path` is `Some(p)` and `p` doesn't exist or can't be read
/// - The host alias isn't found as a literal `Host` entry in the config
///
/// The caller should treat `None` as "no config-based auto-fill available"
/// and leave the form fields at their current values.
///
/// ## Why we don't rely on `russh-config`'s `query()` for "found" detection
///
/// `russh-config`'s `parse_path` *always* returns a `Config` — it folds
/// over all matching `Host` entries and returns `HostConfig::default()`
/// (all `None`) when nothing matches. That means `parse_path("not-in-config",
/// ...)` succeeds with port=22 and user=current_user, which would clobber
/// the user's in-progress form input with defaults. We use our own parser
/// to detect "is this alias literally in the config?" and only then defer
/// to `russh-config` for the full resolution.
pub fn lookup_host(alias: &str, path: Option<&Path>) -> Option<ResolvedHost> {
    // Step 1: confirm the alias is a literal `Host` entry. We do this by
    // parsing with our own parser and checking if the alias appears in
    // the resulting suggestion list.
    let hosts = match path {
        Some(p) => list_ssh_config_hosts_at(p),
        None => list_ssh_config_hosts(),
    };
    if !hosts.iter().any(|h| h.alias == alias) {
        return None;
    }
    // Step 2: defer to `russh-config` for the authoritative resolution.
    // This handles `Include`, `Match`, percent tokens, etc.
    let cfg = match path {
        Some(p) => russh_config::parse_path(p, alias).ok()?,
        None => russh_config::parse_home(alias).ok()?,
    };
    let identity_file = cfg
        .host_config
        .identity_file
        .as_ref()
        .and_then(|v| v.first())
        .map(|p| p.to_string_lossy().into_owned());
    Some(ResolvedHost {
        host: cfg.host().to_string(),
        port: cfg.port(),
        user: cfg.user(),
        identity_file,
        proxy_jump: cfg.host_config.proxy_jump.clone(),
    })
}

/// Convert a [`ResolvedHost`] into the `SshAuth` variant the rest of the
/// codebase expects.
///
/// If `resolved.identity_file` is `Some`, returns [`SshAuth::Key`] with
/// the path and no passphrase (the user can fill in the passphrase in the
/// dialog if needed). Otherwise returns [`SshAuth::Agent`] — the OpenSSH
/// convention is that if `IdentityFile` isn't set in the config, the agent
/// is consulted. We deliberately don't fall back to `SshAuth::Password`
/// because the password is never stored in `~/.ssh/config` (it's always
/// interactive), so we can't auto-fill it.
pub fn resolved_host_to_auth(resolved: &ResolvedHost) -> SshAuth {
    match &resolved.identity_file {
        Some(path) => SshAuth::Key {
            private_key_path: path.clone(),
            passphrase: None,
        },
        None => SshAuth::Agent,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    /// Helper: write a `~/.ssh/config`-shaped file in a temp dir and
    /// return its path.
    fn write_config(contents: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config");
        fs::write(&config_path, contents).expect("write config");
        // Keep the TempDir alive by returning it — dropping it would
        // delete the file.
        (dir, config_path)
    }

    // ---- parse_ssh_config_text ----

    #[test]
    fn parse_empty_returns_empty() {
        assert!(parse_ssh_config_text("").is_empty());
    }

    #[test]
    fn parse_skips_comments_and_blanks() {
        let text = "# a comment\n\n   \n; also a comment\nHost server\n  HostName 1.2.3.4\n";
        let hosts = parse_ssh_config_text(text);
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].alias, "server");
        assert_eq!(hosts[0].hostname.as_deref(), Some("1.2.3.4"));
    }

    #[test]
    fn parse_basic_host_block() {
        let text = "\
Host my-server
  HostName 192.168.1.10
  User admin
  Port 2222
  IdentityFile ~/.ssh/id_server
";
        let hosts = parse_ssh_config_text(text);
        assert_eq!(hosts.len(), 1);
        let h = &hosts[0];
        assert_eq!(h.alias, "my-server");
        assert_eq!(h.hostname.as_deref(), Some("192.168.1.10"));
        assert_eq!(h.user.as_deref(), Some("admin"));
        assert_eq!(h.port, Some(2222));
        assert!(h.identity_file.is_some());
        // tilde expansion: the path should NOT start with `~`
        assert!(!h.identity_file.as_ref().unwrap().starts_with('~'));
    }

    #[test]
    fn parse_multi_host_directive_emits_one_per_alias() {
        let text = "\
Host a b c
  HostName example.com
  User shared
";
        let hosts = parse_ssh_config_text(text);
        assert_eq!(hosts.len(), 3);
        for h in &hosts {
            assert_eq!(h.hostname.as_deref(), Some("example.com"));
            assert_eq!(h.user.as_deref(), Some("shared"));
        }
        let aliases: Vec<&str> = hosts.iter().map(|h| h.alias.as_str()).collect();
        assert_eq!(aliases, vec!["a", "b", "c"]);
    }

    #[test]
    fn parse_skips_wildcard_patterns() {
        let text = "\
Host *
  User defaultuser
Host *.example.com
  User wildcarduser
Host real-server
  HostName 10.0.0.1
";
        let hosts = parse_ssh_config_text(text);
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].alias, "real-server");
    }

    #[test]
    fn parse_skips_negated_wildcard() {
        let text = "\
Host !bad good
  HostName 1.2.3.4
";
        let hosts = parse_ssh_config_text(text);
        // `!bad` is a wildcard (negation prefix), `good` is literal.
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].alias, "good");
    }

    #[test]
    fn parse_keeps_directives_without_host() {
        // Top-of-file directives before any `Host` are ignored (we don't
        // model "default for all hosts" — that's russh-config's job when
        // it resolves a specific host).
        let text = "\
User topuser
Host server
  HostName 1.2.3.4
";
        let hosts = parse_ssh_config_text(text);
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].user, None); // top-of-file User ignored
        assert_eq!(hosts[0].hostname.as_deref(), Some("1.2.3.4"));
    }

    #[test]
    fn parse_proxy_jump_directive() {
        let text = "\
Host bastion
  HostName bastion.example.com
Host private
  HostName 10.0.0.5
  ProxyJump bastion
";
        let hosts = parse_ssh_config_text(text);
        assert_eq!(hosts.len(), 2);
        let private = hosts.iter().find(|h| h.alias == "private").unwrap();
        assert_eq!(private.proxy_jump.as_deref(), Some("bastion"));
    }

    #[test]
    fn parse_first_identity_file_wins() {
        let text = "\
Host server
  HostName 1.2.3.4
  IdentityFile ~/.ssh/id_primary
  IdentityFile ~/.ssh/id_secondary
";
        let hosts = parse_ssh_config_text(text);
        let h = &hosts[0];
        assert!(
            h.identity_file.as_ref().unwrap().ends_with("id_primary"),
            "first IdentityFile should win, got {:?}",
            h.identity_file
        );
    }

    #[test]
    fn parse_invalid_port_is_ignored() {
        let text = "\
Host server
  Port not-a-number
  HostName 1.2.3.4
";
        let hosts = parse_ssh_config_text(text);
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].port, None);
        assert_eq!(hosts[0].hostname.as_deref(), Some("1.2.3.4"));
    }

    #[test]
    fn parse_case_insensitive_keywords() {
        let text = "\
HOST server
  HOSTNAME 1.2.3.4
  USER admin
  PORT 2222
";
        let hosts = parse_ssh_config_text(text);
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].alias, "server");
        assert_eq!(hosts[0].hostname.as_deref(), Some("1.2.3.4"));
        assert_eq!(hosts[0].user.as_deref(), Some("admin"));
        assert_eq!(hosts[0].port, Some(2222));
    }

    // ---- list_ssh_config_hosts_at ----

    #[test]
    fn list_at_nonexistent_file_returns_empty() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("does-not-exist");
        assert!(list_ssh_config_hosts_at(&path).is_empty());
    }

    #[test]
    fn list_at_reads_file_and_parses() {
        let (_dir, path) =
            write_config("Host server1\n  HostName 1.1.1.1\nHost server2\n  HostName 2.2.2.2\n");
        let hosts = list_ssh_config_hosts_at(&path);
        assert_eq!(hosts.len(), 2);
        assert_eq!(hosts[0].alias, "server1");
        assert_eq!(hosts[1].alias, "server2");
    }

    // ---- is_wildcard_pattern ----

    #[test]
    fn wildcard_detection() {
        assert!(is_wildcard_pattern("*"));
        assert!(is_wildcard_pattern("*.example.com"));
        assert!(is_wildcard_pattern("server?"));
        assert!(is_wildcard_pattern("!negated"));
        assert!(!is_wildcard_pattern("server"));
        assert!(!is_wildcard_pattern("my-server.example.com"));
    }

    // ---- expand_tilde ----

    #[test]
    fn expand_tilde_with_home() {
        let home = Path::new("/home/user");
        assert_eq!(expand_tilde("~", Some(home)), "/home/user");
        assert_eq!(expand_tilde("~/foo", Some(home)), "/home/user/foo");
        assert_eq!(expand_tilde("/abs/path", Some(home)), "/abs/path");
        assert_eq!(expand_tilde("relative", Some(home)), "relative");
    }

    #[test]
    fn expand_tilde_without_home_returns_original() {
        assert_eq!(expand_tilde("~/foo", None), "~/foo");
        assert_eq!(expand_tilde("~", None), "~");
    }

    // ---- is_identity_file ----

    #[test]
    fn identity_file_detection() {
        assert!(is_identity_file("id_rsa"));
        assert!(is_identity_file("id_ed25519"));
        assert!(is_identity_file("id_ecdsa"));
        assert!(is_identity_file("id_github"));
        assert!(!is_identity_file("id_rsa.pub"));
        assert!(!is_identity_file("config"));
        assert!(!is_identity_file("known_hosts"));
        assert!(!is_identity_file("authorized_keys"));
        assert!(!is_identity_file("environment"));
        assert!(!is_identity_file("random_file"));
        assert!(!is_identity_file("readme.txt"));
    }

    // ---- list_identity_files_at ----

    #[test]
    fn list_identity_files_at_nonexistent_dir_returns_empty() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("no-such-dir");
        assert!(list_identity_files_at(&path).is_empty());
    }

    #[test]
    fn list_identity_files_at_skips_pub_and_non_id() {
        let dir = tempdir().unwrap();
        let ssh_dir = dir.path();
        // Create a mix of files: real keys, .pub, non-id files, a dir.
        fs::write(ssh_dir.join("id_rsa"), "key").unwrap();
        fs::write(ssh_dir.join("id_ed25519"), "key").unwrap();
        fs::write(ssh_dir.join("id_rsa.pub"), "pubkey").unwrap(); // skip
        fs::write(ssh_dir.join("known_hosts"), "hosts").unwrap(); // skip
        fs::write(ssh_dir.join("config"), "cfg").unwrap(); // skip
        fs::write(ssh_dir.join("random.txt"), "txt").unwrap(); // skip
        fs::create_dir(ssh_dir.join("id_dir")).unwrap(); // skip (dir)

        let files = list_identity_files_at(ssh_dir);
        assert_eq!(files.len(), 2, "got {:?}", files);
        // Sorted lexically by filename
        assert!(files[0].ends_with("id_ed25519"));
        assert!(files[1].ends_with("id_rsa"));
    }

    // ---- lookup_host ----

    #[test]
    fn lookup_host_returns_none_for_missing_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("no-such-file");
        assert!(lookup_host("anything", Some(&path)).is_none());
    }

    #[test]
    fn lookup_host_returns_none_for_missing_alias() {
        let (_dir, path) = write_config("Host server\n  HostName 1.2.3.4\n");
        assert!(lookup_host("not-in-config", Some(&path)).is_none());
    }

    #[test]
    fn lookup_host_resolves_simple_block() {
        let (_dir, path) =
            write_config("Host my-server\n  HostName 1.2.3.4\n  User admin\n  Port 2222\n");
        let resolved = lookup_host("my-server", Some(&path)).expect("found");
        assert_eq!(resolved.host, "1.2.3.4");
        assert_eq!(resolved.port, 2222);
        assert_eq!(resolved.user, "admin");
        assert_eq!(resolved.identity_file, None);
        assert_eq!(resolved.proxy_jump, None);
    }

    #[test]
    fn lookup_host_falls_back_to_alias_when_no_hostname() {
        let (_dir, path) = write_config("Host server\n  User admin\n");
        let resolved = lookup_host("server", Some(&path)).expect("found");
        assert_eq!(resolved.host, "server"); // alias used as hostname
        assert_eq!(resolved.user, "admin");
        assert_eq!(resolved.port, 22); // default
    }

    // ---- resolved_host_to_auth ----

    #[test]
    fn resolved_with_identity_file_becomes_key_auth() {
        let resolved = ResolvedHost {
            host: "1.2.3.4".into(),
            port: 22,
            user: "admin".into(),
            identity_file: Some("/home/user/.ssh/id_rsa".into()),
            proxy_jump: None,
        };
        match resolved_host_to_auth(&resolved) {
            SshAuth::Key {
                private_key_path,
                passphrase,
            } => {
                assert_eq!(private_key_path, "/home/user/.ssh/id_rsa");
                assert_eq!(passphrase, None);
            }
            other => panic!("expected Key, got {:?}", other),
        }
    }

    #[test]
    fn resolved_without_identity_file_becomes_agent_auth() {
        let resolved = ResolvedHost {
            host: "1.2.3.4".into(),
            port: 22,
            user: "admin".into(),
            identity_file: None,
            proxy_jump: None,
        };
        assert!(matches!(resolved_host_to_auth(&resolved), SshAuth::Agent));
    }

    // ---- Real-world config scenarios ----

    #[test]
    fn parse_real_world_config_with_comments_and_unknown_directives() {
        // Mirrors the structure of a real ~/.ssh/config: comments at the
        // top, unknown directives (Compression, ForwardAgent, etc.) that
        // we should skip without breaking, and a Host block with a real
        // hostname + user.
        let text = "\
# Host *\n#    ControlPersist yes\n#    ControlMaster auto\n\nHost platform.example.com\n  HostName platform.example.com\n  Compression yes\n  ForwardAgent yes\n  ForwardX11 yes\n  ForwardX11Trusted yes\n  User dev-tools.user+root.sjtuai4s-ceshi.ws\n";
        let hosts = parse_ssh_config_text(text);
        assert_eq!(hosts.len(), 1);
        let h = &hosts[0];
        assert_eq!(h.alias, "platform.example.com");
        assert_eq!(h.hostname.as_deref(), Some("platform.example.com"));
        assert_eq!(
            h.user.as_deref(),
            Some("dev-tools.user+root.sjtuai4s-ceshi.ws")
        );
        // Unknown directives (Compression, ForwardAgent, etc.) are
        // silently skipped — they don't break the parse, and the fields
        // we care about (HostName, User) are still picked up.
        assert_eq!(h.port, None); // not set in the config
        assert_eq!(h.identity_file, None); // not set
        assert_eq!(h.proxy_jump, None); // not set
    }

    #[test]
    fn parse_duplicate_host_aliases_produces_duplicate_suggestions() {
        // Real configs sometimes have multiple `Host` blocks with the
        // same alias (e.g. different users for different workflows).
        // We emit one suggestion per block — the UI's datalist will show
        // the alias twice (browsers dedupe visually, but the user can
        // still type the alias and pick which resolution they want by
        // editing the form after auto-fill). The `onchange` auto-fill
        // uses the LAST matching block (because `lookup_host` goes
        // through `russh-config`, which folds over all matching entries
        // in order — later entries override earlier ones via `merge`).
        let text = "\
Host server
  User first_user
Host server
  User second_user\n";
        let hosts = parse_ssh_config_text(text);
        assert_eq!(hosts.len(), 2);
        assert_eq!(hosts[0].alias, "server");
        assert_eq!(hosts[0].user.as_deref(), Some("first_user"));
        assert_eq!(hosts[1].alias, "server");
        assert_eq!(hosts[1].user.as_deref(), Some("second_user"));
    }

    #[test]
    fn parse_indented_and_tab_separated_directives() {
        // OpenSSH config is whitespace-tolerant: directives can be
        // indented with spaces OR tabs, and the keyword/value separator
        // can be any whitespace run. Make sure we handle tabs.
        let text = "Host server\n\tHostName\t1.2.3.4\n\tUser\tadmin\n";
        let hosts = parse_ssh_config_text(text);
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].alias, "server");
        assert_eq!(hosts[0].hostname.as_deref(), Some("1.2.3.4"));
        assert_eq!(hosts[0].user.as_deref(), Some("admin"));
    }

    #[test]
    fn parse_strips_inline_comments_after_directives() {
        // OpenSSH treats `#` after whitespace as a comment to end of
        // line. We strip inline comments so directive values don't end
        // up with stray `# ...` text appended.
        let text = "Host server\n  HostName 1.2.3.4 # the IP\n  User admin # my user\n";
        let hosts = parse_ssh_config_text(text);
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].hostname.as_deref(), Some("1.2.3.4"));
        assert_eq!(hosts[0].user.as_deref(), Some("admin"));
    }

    #[test]
    fn parse_keeps_hash_in_value_when_no_preceding_whitespace() {
        // A `#` NOT preceded by whitespace is part of the value (rare
        // but legal). OpenSSH's actual behaviour is to treat `#` as a
        // comment separator only when preceded by whitespace.
        let text = "Host server\n  HostName 1.2.3.4#weird\n";
        let hosts = parse_ssh_config_text(text);
        assert_eq!(hosts.len(), 1);
        // The `#weird` is kept because there's no whitespace before `#`.
        assert_eq!(hosts[0].hostname.as_deref(), Some("1.2.3.4#weird"));
    }
}
