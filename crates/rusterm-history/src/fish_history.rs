use std::path::PathBuf;

use chrono::Utc;
use chrono::TimeZone;

use crate::HistoryMatch;

/// Reads fish shell history from `~/.local/share/fish/fish_history`.
///
/// fish history format (YAML-like):
/// ```text
/// - cmd: command here
///   when: 1234567890
///   paths:
///     - /home/user
/// ```
pub struct FishHistoryProvider {
    path: PathBuf,
}

impl FishHistoryProvider {
    pub fn new() -> Option<Self> {
        let path = dirs::data_dir()?.join("fish").join("fish_history");
        if path.exists() {
            Some(Self { path })
        } else {
            None
        }
    }

    /// Search fish history with prefix matching and frecency ranking.
    pub fn search(&self, query: &str, limit: usize) -> Vec<HistoryMatch> {
        let content = match std::fs::read_to_string(&self.path) {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };

        let query_lower = query.to_lowercase();
        let mut counts: std::collections::HashMap<String, (usize, Option<i64>)> =
            std::collections::HashMap::new();

        let mut current_cmd: Option<String> = None;
        let mut current_ts: Option<i64> = None;

        for line in content.lines() {
            let line = line.trim();

            if let Some(cmd_rest) = line.strip_prefix("- cmd:") {
                // Save previous entry
                if let Some(cmd) = current_cmd.take() {
                    let trimmed = cmd.trim().to_string();
                    if !trimmed.is_empty() {
                        let entry = counts.entry(trimmed).or_insert((0, current_ts));
                        entry.0 += 1;
                        if current_ts > entry.1 {
                            entry.1 = current_ts;
                        }
                    }
                }
                current_cmd = Some(cmd_rest.trim().to_string());
                current_ts = None;
            } else if let Some(ts_rest) = line.strip_prefix("when:") {
                current_ts = ts_rest.trim().parse::<i64>().ok();
            }
            // Skip `paths:` and other fields
        }

        // Save last entry
        if let Some(cmd) = current_cmd.take() {
            let trimmed = cmd.trim().to_string();
            if !trimmed.is_empty() {
                let entry = counts.entry(trimmed).or_insert((0, current_ts));
                entry.0 += 1;
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
