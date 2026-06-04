use anyhow::Result;

use crate::atuin_db::AtuinDbProvider;
use crate::HistoryMatch;

pub struct HybridHistoryProvider {
    atuin: Option<AtuinDbProvider>,
}

impl HybridHistoryProvider {
    pub fn new() -> Self {
        Self {
            atuin: AtuinDbProvider::new(),
        }
    }

    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<HistoryMatch>> {
        // Try atuin first
        if let Some(ref atuin) = self.atuin {
            if let Ok(results) = atuin.search(query, limit) {
                if !results.is_empty() {
                    return Ok(results);
                }
            }
        }

        // Fall back to rusterm-db (caller handles this)
        Ok(Vec::new())
    }

    pub fn recent(&self, limit: usize) -> Result<Vec<HistoryMatch>> {
        if let Some(ref atuin) = self.atuin {
            if let Ok(results) = atuin.recent(limit) {
                if !results.is_empty() {
                    return Ok(results);
                }
            }
        }

        Ok(Vec::new())
    }

    pub fn has_atuin(&self) -> bool {
        self.atuin.is_some()
    }
}
