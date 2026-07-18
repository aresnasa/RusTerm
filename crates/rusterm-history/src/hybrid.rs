use crate::HistoryMatch;
use crate::atuin_db::AtuinDbProvider;
use crate::bash_history::BashHistoryProvider;
use crate::fish_history::FishHistoryProvider;
use crate::zsh_history::ZshHistoryProvider;

/// Unified history provider that queries multiple sources:
/// 1. atuin DB (if installed)
/// 2. zsh history file
/// 3. bash history file
/// 4. fish history file
///
/// Results are merged, deduplicated, and re-ranked by score.
pub struct HybridHistoryProvider {
    atuin: Option<AtuinDbProvider>,
    zsh: Option<ZshHistoryProvider>,
    bash: Option<BashHistoryProvider>,
    fish: Option<FishHistoryProvider>,
}

impl HybridHistoryProvider {
    pub fn new() -> Self {
        Self {
            atuin: AtuinDbProvider::new(),
            zsh: ZshHistoryProvider::new(),
            bash: BashHistoryProvider::new(),
            fish: FishHistoryProvider::new(),
        }
    }

    /// Search across all history sources. Merges results, deduplicates by
    /// command text (keeping the highest score), and returns ranked results.
    pub fn search(&self, query: &str, limit: usize) -> Vec<HistoryMatch> {
        let mut all: Vec<HistoryMatch> = Vec::new();

        // 1. atuin (most metadata, highest quality)
        if let Some(ref atuin) = self.atuin {
            if let Ok(results) = atuin.search(query, limit) {
                all.extend(results);
            }
        }

        // 2. zsh history
        if let Some(ref zsh) = self.zsh {
            all.extend(zsh.search(query, limit));
        }

        // 3. bash history
        if let Some(ref bash) = self.bash {
            all.extend(bash.search(query, limit));
        }

        // 4. fish history
        if let Some(ref fish) = self.fish {
            all.extend(fish.search(query, limit));
        }

        if all.is_empty() {
            return Vec::new();
        }

        // Deduplicate by command, keeping the best score
        let mut best: std::collections::HashMap<String, HistoryMatch> =
            std::collections::HashMap::new();

        for m in all {
            let entry = best.entry(m.command.clone()).or_insert_with(|| {
                HistoryMatch::new(
                    m.command.clone(),
                    m.cwd.clone(),
                    m.hostname.clone(),
                    m.timestamp,
                    m.score,
                    m.exit_code,
                )
            });
            // Keep the highest score; prefer entries with metadata
            if m.score > entry.score
                || (m.score == entry.score && (m.cwd.is_some() || m.hostname.is_some()))
            {
                entry.score = m.score;
                if m.cwd.is_some() {
                    entry.cwd = m.cwd;
                }
                if m.hostname.is_some() {
                    entry.hostname = m.hostname;
                }
                if m.timestamp.is_some() {
                    entry.timestamp = m.timestamp;
                }
                // Preserve exit_code: prefer a known non-None value. If both
                // are Some, prefer a non-zero one (bias toward "don't suggest"
                // — a command that ever failed in atuin should be treated as
                // failed until proven successful in RusTerm).
                if m.exit_code.is_some() {
                    match entry.exit_code {
                        None => entry.exit_code = m.exit_code,
                        Some(0) => entry.exit_code = m.exit_code,
                        Some(_) => {} // keep existing non-zero
                    }
                }
            }
        }

        let mut results: Vec<HistoryMatch> = best.into_values().collect();
        results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        results.truncate(limit);
        results
    }

    pub fn has_atuin(&self) -> bool {
        self.atuin.is_some()
    }

    pub fn has_zsh(&self) -> bool {
        self.zsh.is_some()
    }

    pub fn has_bash(&self) -> bool {
        self.bash.is_some()
    }

    pub fn has_fish(&self) -> bool {
        self.fish.is_some()
    }
}
