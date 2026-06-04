pub mod atuin_db;
pub mod hybrid;

pub use atuin_db::AtuinDbProvider;
pub use hybrid::HybridHistoryProvider;

use chrono::{DateTime, Utc};

#[derive(Debug, Clone)]
pub struct HistoryMatch {
    pub command: String,
    pub cwd: Option<String>,
    pub hostname: Option<String>,
    pub timestamp: Option<DateTime<Utc>>,
    pub score: f32,
}
