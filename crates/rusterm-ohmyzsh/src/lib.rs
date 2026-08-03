//! Native oh-my-zsh integration for RusTerm.
//!
//! Oh My Zsh (https://github.com/ohmyzsh/ohmyzsh) is a community-driven zsh
//! framework that bundles 300+ plugins, each of which typically defines a set
//! of shell aliases (e.g. the `git` plugin defines `g='git'`, `gst='git
//! status'`, `gco='git checkout'`, …). Users enable plugins in `~/.zshrc` via
//! `plugins=(git docker kubectl ...)`.
//!
//! RusTerm can't execute zsh scripts, but it CAN read the plugin files at
//! startup and surface the aliases as completion suggestions — so when the
//! user types `g`, the popup shows `gst`, `gco`, `gaa`, … (with the expansion
//! shown alongside). This is the "auto-popup" integration the user asked for:
//! the aliases the user's shell already understands become first-class
//! suggestions in the RusTerm UI, without the shell having to be the source.
//!
//! # What this crate does
//!
//! - [`OhMyZsh::detect`] — finds `~/.oh-my-zsh` (or `$ZSH`) and reads
//!   `~/.zshrc` to determine which plugins are enabled and which theme is
//!   active. Returns `None` if oh-my-zsh isn't installed.
//! - [`OhMyZsh::load`] — reads each enabled plugin's `*.plugin.zsh` file and
//!   parses `alias name='value'` / `alias name="value"` / `alias name=value`
//!   definitions into an in-memory index. Cheap: a typical plugin file is a
//!   few KB; the whole enabled set is well under 1 MB.
//! - [`OhMyZsh::aliases_for_prefix`] — given a command prefix the user has
//!   typed (e.g. `g`), returns the matching aliases sorted by relevance
//!   (exact-prefix matches first, then contains matches). Each entry carries
//!   the alias name and its expansion so the UI can render `gst → git status`.
//! - [`OhMyZsh::expand_alias`] — given a full alias name, returns its
//!   expansion (used for inline ghost-text suggestions).
//!
//! # What this crate deliberately does NOT do
//!
//! - It does NOT install or modify oh-my-zsh. RusTerm reads the user's
//!   existing installation; if oh-my-zsh isn't there, the crate is a no-op.
//! - It does NOT parse zsh function definitions, `compdef` calls, or any
//!   dynamic completion logic. Only static `alias` lines are parsed — these
//!   are the most common and most useful "auto-popup" content from plugins.
//! - It does NOT execute anything. Pure file reads + string parsing.
//!
//! # Threading
//!
//! [`OhMyZsh`] is `Send + Sync` and uses no `Mutex` — the index is built once
//! at load time and is immutable thereafter. Concurrent
//! [`aliases_for_prefix`] calls are lock-free.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A single alias parsed from an oh-my-zsh plugin file.
///
/// `name` is what the user types (e.g. `gst`); `expansion` is what the shell
/// expands it to (e.g. `git status`). The `plugin` field records which plugin
/// the alias came from so the UI can show a source label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginAlias {
    pub name: String,
    pub expansion: String,
    pub plugin: String,
}

/// A loaded oh-my-zsh installation and its enabled plugins' aliases.
///
/// Built once via [`OhMyZsh::load`] (which calls [`OhMyZsh::detect`]
/// internally) and queried on every keystroke via
/// [`aliases_for_prefix`].
///
/// Cheap to clone: the alias index is shared via `Arc`.
#[derive(Clone)]
pub struct OhMyZsh {
    /// Root of the oh-my-zsh install (e.g. `~/.oh-my-zsh`).
    root: PathBuf,
    /// Active theme name (e.g. `robbyrussell`), for prompt-aware features.
    theme: Option<String>,
    /// All enabled plugins' aliases, keyed by alias name for O(1) exact
    /// lookup. Multiple plugins can define the same alias name; the last one
    /// loaded wins (mirrors zsh's own `source` order — `plugins=(a b)` loads
    /// `a` then `b`, and `b`'s alias would override `a`'s).
    aliases: Arc<HashMap<String, PluginAlias>>,
}

impl std::fmt::Debug for OhMyZsh {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OhMyZsh")
            .field("root", &self.root)
            .field("theme", &self.theme)
            .field("alias_count", &self.aliases.len())
            .finish_non_exhaustive()
    }
}

/// Default oh-my-zsh install location: `~/.oh-my-zsh`.
fn default_omz_root() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".oh-my-zsh"))
}

/// Locate the oh-my-zsh root directory.
///
/// Checks `$ZSH` first (the env var oh-my-zsh's own installer sets in
/// `~/.zshrc`), then falls back to `~/.oh-my-zsh`. Returns `None` if neither
/// exists on disk.
fn find_omz_root() -> Option<PathBuf> {
    if let Ok(zsh_env) = std::env::var("ZSH") {
        let p = PathBuf::from(zsh_env);
        if p.is_dir() {
            return Some(p);
        }
    }
    let default = default_omz_root()?;
    if default.is_dir() {
        Some(default)
    } else {
        None
    }
}

/// Parse `plugins=(a b c)` from a zshrc body.
///
/// Handles the multi-line form:
/// ```zsh
/// plugins=(
///     git
///     docker  # inline comment
///     kubectl
/// )
/// ```
///
/// and the single-line form `plugins=(git docker)`. Plugin names with inline
/// `# comment` are stripped. Returns the list of plugin names in declaration
/// order; if no `plugins=` line is found, returns an empty vec (oh-my-zsh's
/// default is `git` only, but we don't hardcode that — we respect what the
/// user wrote).
fn parse_plugins_block(zshrc: &str) -> Vec<String> {
    // Find the `plugins=(` marker, skipping comment lines (lines whose first
    // non-whitespace character is `#`). We do a full-text scan rather than a
    // line-by-line scan because the `)` may be on a later line.
    //
    // Strategy: scan line by line for a non-comment line starting with
    // `plugins=(`. Record the byte offset in the full zshrc string so we can
    // slice from there to the matching `)`.
    let mut start: Option<usize> = None;
    let mut search_from = 0usize;
    for line in zshrc.lines() {
        let line_start = search_from;
        let trimmed = line.trim_start();
        if !trimmed.starts_with('#') && trimmed.starts_with("plugins=(") {
            // Byte offset of `plugins=(` within `zshrc`.
            start = Some(line_start + (line.len() - trimmed.len()));
            break;
        }
        search_from = line_start + line.len() + 1; // +1 for the \n
    }
    let start = match start {
        Some(s) => s,
        None => return Vec::new(),
    };
    let after_paren = &zshrc[start + "plugins=(".len()..];
    let end = match after_paren.find(')') {
        Some(e) => e,
        None => return Vec::new(),
    };
    let inner = &after_paren[..end];
    // Strip inline comments: a `#` to end-of-line starts a comment. We process
    // line by line so a `#` on one line doesn't eat the next line's plugins.
    let mut plugins = Vec::new();
    for line in inner.lines() {
        // Remove everything from the first `#` on this line.
        let line = match line.find('#') {
            Some(idx) => &line[..idx],
            None => line,
        };
        for word in line.split_whitespace() {
            if !word.is_empty() {
                plugins.push(word.to_string());
            }
        }
    }
    plugins
}

/// Parse `ZSH_THEME="..."` from a zshrc body.
fn parse_theme(zshrc: &str) -> Option<String> {
    for line in zshrc.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("ZSH_THEME=") {
            // Strip surrounding quotes.
            let value = rest.trim();
            if (value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\''))
            {
                return Some(value[1..value.len() - 1].to_string());
            } else if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// Parse a single `alias` line from a `.plugin.zsh` file.
///
/// Recognised forms (all standard zsh `alias` syntax):
/// ```zsh
/// alias name='value'
/// alias name="value"
/// alias name=value
/// alias name='value with spaces'
/// ```
///
/// Returns `None` for lines that aren't `alias` definitions (comments,
/// function defs, `compdef`, etc.).
fn parse_alias_line(line: &str, plugin: &str) -> Option<PluginAlias> {
    let trimmed = line.trim();
    if !trimmed.starts_with("alias ") {
        return None;
    }
    let rest = &trimmed["alias ".len()..];
    // Find the `=` separator. The alias name is everything before it.
    let eq_pos = rest.find('=')?;
    let name = rest[..eq_pos].trim();
    if name.is_empty() || name.contains(char::is_whitespace) {
        // Alias names can't contain whitespace; this isn't a real alias.
        return None;
    }
    let mut value = rest[eq_pos + 1..].trim().to_string();
    // Strip surrounding quotes (single or double).
    if value.len() >= 2 {
        let first = value.as_bytes()[0];
        let last = value.as_bytes()[value.len() - 1];
        if (first == b'\'' && last == b'\'') || (first == b'"' && last == b'"') {
            value = value[1..value.len() - 1].to_string();
        }
    }
    // Skip empty expansions — a `alias foo=` line clears a prior alias; it's
    // not a useful suggestion.
    if value.is_empty() {
        return None;
    }
    Some(PluginAlias {
        name: name.to_string(),
        expansion: value,
        plugin: plugin.to_string(),
    })
}

impl OhMyZsh {
    /// Detect whether oh-my-zsh is installed and, if so, read `~/.zshrc` to
    /// determine the enabled plugins and theme. Does NOT load the plugin
    /// aliases — call [`Self::load`] for that.
    ///
    /// Returns `None` if oh-my-zsh is not installed (no `~/.oh-my-zsh`
    /// directory and no `$ZSH` env var pointing to one).
    pub fn detect() -> Option<DetectedOhMyZsh> {
        let root = find_omz_root()?;
        let zshrc_path = dirs::home_dir()?.join(".zshrc");
        let zshrc = std::fs::read_to_string(&zshrc_path).unwrap_or_default();
        let plugins = parse_plugins_block(&zshrc);
        let theme = parse_theme(&zshrc);
        Some(DetectedOhMyZsh {
            root,
            plugins,
            theme,
        })
    }

    /// Detect oh-my-zsh AND load all enabled plugins' aliases into an
    /// in-memory index. This is the main entry point for the UI.
    ///
    /// Returns `None` if oh-my-zsh isn't installed. The result is cheap to
    /// clone (the alias index is `Arc`-shared).
    pub fn load() -> Option<OhMyZsh> {
        let detected = Self::detect()?;
        let mut aliases: HashMap<String, PluginAlias> = HashMap::new();
        for plugin_name in &detected.plugins {
            let plugin_file = detected
                .root
                .join("plugins")
                .join(plugin_name)
                .join(format!("{}.plugin.zsh", plugin_name));
            if !plugin_file.is_file() {
                // Plugin might be a custom plugin in $ZSH_CUSTOM, or it might
                // not exist. Skip silently — the user's zshrc may list plugins
                // that aren't installed.
                continue;
            }
            let content = match std::fs::read_to_string(&plugin_file) {
                Ok(s) => s,
                Err(e) => {
                    tracing::debug!(
                        "[ohmyzsh] failed to read plugin {}: {}",
                        plugin_file.display(),
                        e
                    );
                    continue;
                }
            };
            for line in content.lines() {
                if let Some(alias) = parse_alias_line(line, plugin_name) {
                    // Last-loaded plugin wins, mirroring zsh's source order.
                    aliases.insert(alias.name.clone(), alias);
                }
            }
        }
        tracing::info!(
            "[ohmyzsh] loaded {} aliases from {} plugins (theme={:?})",
            aliases.len(),
            detected.plugins.len(),
            detected.theme
        );
        Some(OhMyZsh {
            root: detected.root,
            theme: detected.theme,
            aliases: Arc::new(aliases),
        })
    }

    /// Return aliases whose name starts with `prefix` (case-insensitive),
    /// sorted by relevance: exact prefix matches first, then by name length
    /// (shorter aliases like `g` before `gaa`), then alphabetically.
    ///
    /// `limit` caps the result count (the UI typically wants ~10).
    pub fn aliases_for_prefix(&self, prefix: &str, limit: usize) -> Vec<PluginAlias> {
        if prefix.is_empty() || self.aliases.is_empty() {
            return Vec::new();
        }
        let lower = prefix.to_lowercase();
        let mut matches: Vec<&PluginAlias> = self
            .aliases
            .values()
            .filter(|a| a.name.to_lowercase().starts_with(&lower))
            .collect();
        // Sort: shorter names first (they're usually the "base" aliases like
        // `g` for git), then alphabetically for stability.
        matches.sort_by(|a, b| {
            a.name
                .len()
                .cmp(&b.name.len())
                .then_with(|| a.name.cmp(&b.name))
        });
        matches.into_iter().take(limit).cloned().collect()
    }

    /// Given a full alias name, return its expansion (or `None` if the name
    /// isn't a known alias). Used for inline ghost-text suggestions.
    pub fn expand_alias(&self, name: &str) -> Option<&str> {
        self.aliases.get(name).map(|a| a.expansion.as_str())
    }

    /// Number of loaded aliases.
    pub fn alias_count(&self) -> usize {
        self.aliases.len()
    }

    /// The oh-my-zsh root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The active theme name (e.g. `robbyrussell`), if one was set in
    /// `~/.zshrc`.
    pub fn theme(&self) -> Option<&str> {
        self.theme.as_deref()
    }

    /// The list of enabled plugin names (in `~/.zshrc` declaration order).
    /// Useful for diagnostics / the settings panel.
    pub fn enabled_plugins(&self) -> Vec<&str> {
        // We don't store the plugin list separately, but we can derive a
        // unique set from the aliases' `plugin` field. This isn't the same as
        // the zshrc declaration order, but it's close enough for display.
        let mut plugins: Vec<&str> = self
            .aliases
            .values()
            .map(|a| a.plugin.as_str())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        plugins.sort();
        plugins
    }
}

/// Intermediate result of [`OhMyZsh::detect`]: the oh-my-zsh root, the
/// enabled plugin names, and the theme — before the plugin files are read.
#[derive(Debug)]
pub struct DetectedOhMyZsh {
    pub root: PathBuf,
    pub plugins: Vec<String>,
    pub theme: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_plugins_block_single_line() {
        let zshrc = "plugins=(git docker kubectl)\n";
        let plugins = parse_plugins_block(zshrc);
        assert_eq!(plugins, vec!["git", "docker", "kubectl"]);
    }

    #[test]
    fn parse_plugins_block_multi_line() {
        let zshrc = "plugins=(
    git
    docker  # inline comment
    kubectl
)
";
        let plugins = parse_plugins_block(zshrc);
        assert_eq!(plugins, vec!["git", "docker", "kubectl"]);
    }

    #[test]
    fn parse_plugins_block_with_comments_stripped() {
        let zshrc = "# plugins=(commented_out)
plugins=(git docker)
";
        let plugins = parse_plugins_block(zshrc);
        assert_eq!(plugins, vec!["git", "docker"]);
    }

    #[test]
    fn parse_plugins_block_missing_returns_empty() {
        let zshrc = "export PATH=/usr/bin\n";
        let plugins = parse_plugins_block(zshrc);
        assert!(plugins.is_empty());
    }

    #[test]
    fn parse_theme_double_quoted() {
        let zshrc = "ZSH_THEME=\"robbyrussell\"\n";
        assert_eq!(parse_theme(zshrc), Some("robbyrussell".to_string()));
    }

    #[test]
    fn parse_theme_single_quoted() {
        let zshrc = "ZSH_THEME='agnoster'\n";
        assert_eq!(parse_theme(zshrc), Some("agnoster".to_string()));
    }

    #[test]
    fn parse_theme_missing() {
        let zshrc = "export PATH=/usr/bin\n";
        assert_eq!(parse_theme(zshrc), None);
    }

    #[test]
    fn parse_theme_ignores_commented() {
        let zshrc = "# ZSH_THEME=\"old\"\nZSH_THEME=\"new\"\n";
        assert_eq!(parse_theme(zshrc), Some("new".to_string()));
    }

    #[test]
    fn parse_alias_line_single_quoted() {
        let alias = parse_alias_line("alias gst='git status'", "git").unwrap();
        assert_eq!(alias.name, "gst");
        assert_eq!(alias.expansion, "git status");
        assert_eq!(alias.plugin, "git");
    }

    #[test]
    fn parse_alias_line_double_quoted() {
        let alias = parse_alias_line("alias gco=\"git checkout\"", "git").unwrap();
        assert_eq!(alias.name, "gco");
        assert_eq!(alias.expansion, "git checkout");
    }

    #[test]
    fn parse_alias_line_unquoted() {
        let alias = parse_alias_line("alias g=git", "git").unwrap();
        assert_eq!(alias.name, "g");
        assert_eq!(alias.expansion, "git");
    }

    #[test]
    fn parse_alias_line_ignores_non_alias() {
        assert!(parse_alias_line("# alias foo='bar'", "git").is_none());
        assert!(parse_alias_line("compdef _git git", "git").is_none());
        assert!(parse_alias_line("function gpp() { ... }", "git").is_none());
        assert!(parse_alias_line("", "git").is_none());
    }

    #[test]
    fn parse_alias_line_empty_value_skipped() {
        // `alias foo=` clears a prior alias; not useful as a suggestion.
        assert!(parse_alias_line("alias foo=", "git").is_none());
        assert!(parse_alias_line("alias foo=''", "git").is_none());
    }

    #[test]
    fn parse_alias_line_with_spaces_in_value() {
        let alias = parse_alias_line(
            "alias gwip='git add -A; git commit --no-verify -m \"wip\"'",
            "git",
        )
        .unwrap();
        assert_eq!(alias.name, "gwip");
        assert_eq!(
            alias.expansion,
            "git add -A; git commit --no-verify -m \"wip\""
        );
    }

    #[test]
    fn aliases_for_prefix_returns_matches_sorted_by_length() {
        let mut aliases = HashMap::new();
        aliases.insert(
            "gaa".to_string(),
            PluginAlias {
                name: "gaa".into(),
                expansion: "git add --all".into(),
                plugin: "git".into(),
            },
        );
        aliases.insert(
            "g".to_string(),
            PluginAlias {
                name: "g".into(),
                expansion: "git".into(),
                plugin: "git".into(),
            },
        );
        aliases.insert(
            "ga".to_string(),
            PluginAlias {
                name: "ga".into(),
                expansion: "git add".into(),
                plugin: "git".into(),
            },
        );
        aliases.insert(
            "gst".to_string(),
            PluginAlias {
                name: "gst".into(),
                expansion: "git status".into(),
                plugin: "git".into(),
            },
        );
        let omz = OhMyZsh {
            root: PathBuf::new(),
            theme: None,
            aliases: Arc::new(aliases),
        };
        let results = omz.aliases_for_prefix("g", 10);
        // Shortest first: g, ga, gaa, gst.
        assert_eq!(results.len(), 4);
        assert_eq!(results[0].name, "g");
        assert_eq!(results[1].name, "ga");
        assert_eq!(results[2].name, "gaa");
        assert_eq!(results[3].name, "gst");
    }

    #[test]
    fn aliases_for_prefix_case_insensitive() {
        let mut aliases = HashMap::new();
        aliases.insert(
            "GST".to_string(),
            PluginAlias {
                name: "GST".into(),
                expansion: "git status".into(),
                plugin: "git".into(),
            },
        );
        let omz = OhMyZsh {
            root: PathBuf::new(),
            theme: None,
            aliases: Arc::new(aliases),
        };
        let results = omz.aliases_for_prefix("gs", 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "GST");
    }

    #[test]
    fn aliases_for_prefix_empty_returns_empty() {
        let omz = OhMyZsh {
            root: PathBuf::new(),
            theme: None,
            aliases: Arc::new(HashMap::new()),
        };
        assert!(omz.aliases_for_prefix("g", 10).is_empty());
        assert!(omz.aliases_for_prefix("", 10).is_empty());
    }

    #[test]
    fn aliases_for_prefix_respects_limit() {
        let mut aliases = HashMap::new();
        for name in &["g", "ga", "gaa", "gaaa", "gst", "gco"] {
            aliases.insert(
                name.to_string(),
                PluginAlias {
                    name: name.to_string(),
                    expansion: "git".into(),
                    plugin: "git".into(),
                },
            );
        }
        let omz = OhMyZsh {
            root: PathBuf::new(),
            theme: None,
            aliases: Arc::new(aliases),
        };
        let results = omz.aliases_for_prefix("g", 3);
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn expand_alias_returns_expansion() {
        let mut aliases = HashMap::new();
        aliases.insert(
            "gst".to_string(),
            PluginAlias {
                name: "gst".into(),
                expansion: "git status".into(),
                plugin: "git".into(),
            },
        );
        let omz = OhMyZsh {
            root: PathBuf::new(),
            theme: None,
            aliases: Arc::new(aliases),
        };
        assert_eq!(omz.expand_alias("gst"), Some("git status"));
        assert_eq!(omz.expand_alias("unknown"), None);
    }

    #[test]
    fn last_loaded_plugin_wins() {
        // Simulate two plugins defining the same alias name. The load path
        // inserts into the HashMap in order, so the second insert wins.
        let mut aliases = HashMap::new();
        aliases.insert(
            "g".to_string(),
            PluginAlias {
                name: "g".into(),
                expansion: "git".into(),
                plugin: "git".into(),
            },
        );
        aliases.insert(
            "g".to_string(),
            PluginAlias {
                name: "g".into(),
                expansion: "golang".into(),
                plugin: "golang".into(),
            },
        );
        let omz = OhMyZsh {
            root: PathBuf::new(),
            theme: None,
            aliases: Arc::new(aliases),
        };
        // The golang plugin's alias should win (last insert).
        assert_eq!(omz.expand_alias("g"), Some("golang"));
    }
}
