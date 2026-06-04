use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use rusterm_core::config::ConnectionConfig;
use rusterm_core::config_manager::ConfigManager;
use rusterm_core::session::SessionType;
use rusterm_core::session_log::SessionLog;
use rusterm_core::terminal::{RenderOutput, Terminal};

pub type TerminalHandle = Arc<Mutex<TerminalEntry>>;

pub struct TerminalEntry {
    pub terminal: Terminal,
    pub parser: vte::ansi::Processor,
    pub scroll_offset: usize,
}

impl TerminalEntry {
    pub fn process_and_render(&mut self, data: &[u8]) -> rusterm_core::terminal::RenderOutput {
        let parser = &mut self.parser;
        self.terminal.process(data, parser);
        if self.scroll_offset == 0 {
            self.terminal.render_with_scroll(0)
        } else {
            self.terminal.render_with_scroll(self.scroll_offset)
        }
    }

    pub fn scroll_up(&mut self, rows: usize) -> rusterm_core::terminal::RenderOutput {
        let max_scroll = self.terminal.scrollback_len();
        self.scroll_offset = (self.scroll_offset + rows).min(max_scroll);
        self.terminal.render_with_scroll(self.scroll_offset)
    }

    pub fn scroll_down(&mut self, rows: usize) -> rusterm_core::terminal::RenderOutput {
        self.scroll_offset = self.scroll_offset.saturating_sub(rows);
        self.terminal.render_with_scroll(self.scroll_offset)
    }

    pub fn scroll_to_bottom(&mut self) -> rusterm_core::terminal::RenderOutput {
        self.scroll_offset = 0;
        self.terminal.render_with_scroll(0)
    }

    pub fn render_current(&self) -> rusterm_core::terminal::RenderOutput {
        self.terminal.render_with_scroll(self.scroll_offset)
    }
}

impl std::fmt::Debug for TerminalEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalEntry").finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum UnlockState {
    #[default]
    FirstRun,
    Locked,
    Unlocked,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppState {
    pub sessions: Vec<SessionTab>,
    pub active_session: Option<String>,
    pub sidebar_open: bool,
    pub connections: Vec<ConnectionConfig>,
    pub theme: Theme,
    #[serde(skip)]
    pub close_senders: Vec<(String, mpsc::UnboundedSender<()>)>,
    #[serde(skip)]
    pub resize_senders: HashMap<String, mpsc::UnboundedSender<(u16, u16, u32, u32)>>,
    #[serde(skip)]
    pub config_manager: Option<ConfigManager>,
    #[serde(skip)]
    pub terminals: HashMap<String, TerminalHandle>,
    #[serde(skip)]
    pub session_logs: HashMap<String, Arc<Mutex<SessionLog>>>,
    #[serde(skip)]
    pub unlock_state: UnlockState,
    #[serde(skip)]
    pub master_password_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionTab {
    pub id: String,
    pub name: String,
    pub kind: SessionType,
    #[serde(skip)]
    pub render_output: RenderOutput,
    pub version: u64,
    pub suggestion: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Theme {
    Dark,
    Light,
}

impl Default for AppState {
    fn default() -> Self {
        let unlock_state = if ConfigManager::check_config_exists() {
            UnlockState::Locked
        } else {
            UnlockState::FirstRun
        };
        Self {
            sessions: Vec::new(),
            active_session: None,
            sidebar_open: true,
            connections: Vec::new(),
            theme: Theme::Dark,
            close_senders: Vec::new(),
            resize_senders: HashMap::new(),
            config_manager: None,
            terminals: HashMap::new(),
            session_logs: HashMap::new(),
            unlock_state,
            master_password_error: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Modal {
    None,
    NewConnection,
    Settings,
    AiSuggest,
}
