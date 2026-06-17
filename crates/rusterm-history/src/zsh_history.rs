use std::path::PathBuf;

use chrono::Utc;
use chrono::TimeZone;

use crate::HistoryMatch;

/// Reads zsh history from `~/.zsh_history`.
///
/// zsh extended history format: `: timestamp:duration;command`
/// Multi-line commands have subsequent lines without the `:` prefix.
pub struct ZshHistoryProvider {
    path: PathBuf,
}

impl ZshHistoryProvider {
    pub fn new() -> Option<Self> {
        let path = dirs::home_dir()?.join(".zsh_history");
        if path.exists() {
            Some(Self { path })
        } else {
            None
        }
    }

    /// Search zsh history with prefix matching and frecency ranking.
    pub fn search(&self, query: &str, limit: usize) -> Vec<HistoryMatch> {
        let content = match std::fs::read_to_string(&self.path) {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };

        let query_lower = query.to_lowercase();
        let mut entries: Vec<(String, Option<i64>)> = Vec::new();
        let mut current_cmd = String::new();
        let mut current_ts: Option<i64> = None;

        for line in content.lines() {
            if let Some(rest) = line.strip_prefix(':') {
                // Save previous command
                let trimmed = current_cmd.trim().to_string();
                if !trimmed.is_empty() {
                    entries.push((trimmed, current_ts));
                }
                current_cmd.clear();

                // Parse extended format: `: timestamp:duration;command`
                if let Some(semicolon_pos) = rest.find(';') {
                    let meta = &rest[..semicolon_pos];
                    let cmd = &rest[semicolon_pos + 1..];

                    // Parse timestamp from `timestamp:duration`
                    current_ts = meta.split(':').next().and_then(|s| s.trim().parse::<i64>().ok());
                    current_cmd.push_str(cmd);
                } else {
                    current_cmd.push_str(rest);
                }
            } else {
                // Continuation line for multi-line command
                if !current_cmd.is_empty() {
                    current_cmd.push('\n');
                    current_cmd.push_str(line);
                }
            }
        }

        // Don't forget the last command
        let trimmed = current_cmd.trim().to_string();
        if !trimmed.is_empty() {
            entries.push((trimmed, current_ts));
        }

        // Group by command, track frequency and latest timestamp
        let mut counts: std::collections::HashMap<String, (usize, Option<i64>)> =
            std::collections::HashMap::new();

        for (cmd, ts) in &entries {
            let entry = counts.entry(cmd.clone()).or_insert((0, *ts));
            entry.0 += 1;
            if ts.is_some() && (entry.1.is_none() || ts > &entry.1) {
                entry.1 = *ts;
            }
        }

        let now_secs = Utc::now().timestamp();

        let mut results: Vec<HistoryMatch> = counts
            .into_iter()
            .filter(|(cmd, _)| cmd.to_lowercase().starts_with(&query_lower))
            .map(|(command, (count, ts))| {
                let recency = if let Some(t) = ts {
                    let age = now_secs - t;
                    if age < 3600 { 90.0 }
                    else if age < 86400 { 70.0 }
                    else if age < 259200 { 50.0 }
                    else if age < 604800 { 30.0 }
                    else if age < 2592000 { 15.0 }
                    else { 5.0 }
                } else {
                    5.0
                };

                let timestamp = ts.and_then(|t| Utc.timestamp_opt(t, 0).single());
                HistoryMatch::new(
                    command,
                    None,
                    None,
                    timestamp,
                    (count as f32).ln() * 20.0 + recency,
                )
            })
            .collect();

        results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        results.truncate(limit);
        results
    }
}
