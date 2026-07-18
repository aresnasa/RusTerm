use std::path::PathBuf;

use crate::HistoryMatch;

/// Reads bash history from `~/.bash_history`.
pub struct BashHistoryProvider {
    path: PathBuf,
}

impl BashHistoryProvider {
    pub fn new() -> Option<Self> {
        let path = dirs::home_dir()?.join(".bash_history");
        if path.exists() {
            Some(Self { path })
        } else {
            None
        }
    }

    /// Search bash history with prefix matching and frecency ranking.
    pub fn search(&self, query: &str, limit: usize) -> Vec<HistoryMatch> {
        let content = match std::fs::read_to_string(&self.path) {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };

        let query_lower = query.to_lowercase();
        let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let line_lower = line.to_lowercase();
            if line_lower.starts_with(&query_lower) {
                *counts.entry(line.to_string()).or_insert(0) += 1;
            }
        }

        let mut results: Vec<HistoryMatch> = counts
            .into_iter()
            .map(|(command, count)| {
                HistoryMatch::new(
                    command,
                    None,
                    None,
                    None,
                    (count as f32).ln() * 20.0 + 5.0,
                    // bash history file format has no exit code — leave None so
                    // downstream (DB import) can mark NULL. The HAVING clause
                    // keeps NULL rows as "unknown, assume success"; that's the
                    // best we can do for sources without exit-code info.
                    None,
                )
            })
            .collect();

        results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        results.truncate(limit);
        results
    }
}
