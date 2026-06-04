use serde::{Deserialize, Serialize};

use crate::session::SessionId;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TerminalEvent {
    Output(Vec<u8>),
    Bell,
    Title(String),
    Clipboard(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SessionEvent {
    Created(SessionId),
    Connected(SessionId),
    Disconnected(SessionId, String),
    Output(SessionId, Vec<u8>),
    Error(SessionId, String),
    Resized(SessionId, u16, u16),
    Closed(SessionId),
}
