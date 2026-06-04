use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::event::SessionEvent;

pub type SessionId = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionType {
    Ssh,
    Serial,
    Telnet,
    Shell,
    Tcp,
}

#[derive(Debug)]
pub struct Session {
    pub id: SessionId,
    pub name: String,
    pub kind: SessionType,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub input_tx: mpsc::UnboundedSender<Vec<u8>>,
    pub resize_tx: mpsc::UnboundedSender<(u16, u16, u32, u32)>,
    pub close_tx: mpsc::UnboundedSender<()>,
}

impl Session {
    pub fn new(
        name: String,
        kind: SessionType,
        input_tx: mpsc::UnboundedSender<Vec<u8>>,
        resize_tx: mpsc::UnboundedSender<(u16, u16, u32, u32)>,
        close_tx: mpsc::UnboundedSender<()>,
    ) -> Self {
        Self::with_id(
            Uuid::new_v4().to_string(),
            name,
            kind,
            input_tx,
            resize_tx,
            close_tx,
        )
    }

    pub fn with_id(
        id: SessionId,
        name: String,
        kind: SessionType,
        input_tx: mpsc::UnboundedSender<Vec<u8>>,
        resize_tx: mpsc::UnboundedSender<(u16, u16, u32, u32)>,
        close_tx: mpsc::UnboundedSender<()>,
    ) -> Self {
        Self {
            id,
            name,
            kind,
            created_at: chrono::Utc::now(),
            input_tx,
            resize_tx,
            close_tx,
        }
    }

    pub fn send_input(&self, data: &[u8]) -> anyhow::Result<()> {
        self.input_tx.send(data.to_vec())?;
        Ok(())
    }

    pub fn resize(&self, cols: u16, rows: u16, pw: u32, ph: u32) -> anyhow::Result<()> {
        self.resize_tx.send((cols, rows, pw, ph))?;
        Ok(())
    }

    pub fn close(&self) -> anyhow::Result<()> {
        self.close_tx.send(())?;
        Ok(())
    }
}

pub struct SessionManager {
    sessions: Arc<Mutex<Vec<Arc<Session>>>>,
    event_tx: mpsc::UnboundedSender<SessionEvent>,
}

impl SessionManager {
    pub fn new(event_tx: mpsc::UnboundedSender<SessionEvent>) -> Self {
        Self {
            sessions: Arc::new(Mutex::new(Vec::new())),
            event_tx,
        }
    }

    pub fn add(&self, session: Session) -> Arc<Session> {
        let id = session.id.clone();
        let arc = Arc::new(session);
        self.sessions.lock().push(arc.clone());
        let _ = self.event_tx.send(SessionEvent::Created(id));
        arc
    }

    pub fn remove(&self, id: &str) -> Option<Arc<Session>> {
        let mut sessions = self.sessions.lock();
        if let Some(pos) = sessions.iter().position(|s| s.id == id) {
            let session = sessions.remove(pos);
            let _ = self.event_tx.send(SessionEvent::Closed(id.to_string()));
            Some(session)
        } else {
            None
        }
    }

    pub fn get(&self, id: &str) -> Option<Arc<Session>> {
        self.sessions
            .lock()
            .iter()
            .find(|s| s.id == id)
            .cloned()
    }

    pub fn list(&self) -> Vec<Arc<Session>> {
        self.sessions.lock().clone()
    }

    pub fn event_sender(&self) -> mpsc::UnboundedSender<SessionEvent> {
        self.event_tx.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_creation() {
        let (input_tx, _) = mpsc::unbounded_channel();
        let (resize_tx, _) = mpsc::unbounded_channel();
        let (close_tx, _) = mpsc::unbounded_channel();

        let session = Session::new("test-host".to_string(), SessionType::Ssh, input_tx, resize_tx, close_tx);

        assert_eq!(session.name, "test-host");
        assert_eq!(session.kind, SessionType::Ssh);
        assert!(!session.id.is_empty());
    }

    #[test]
    fn test_session_manager_add_remove() {
        let (event_tx, _rx) = mpsc::unbounded_channel();
        let manager = SessionManager::new(event_tx);

        let (input_tx, _) = mpsc::unbounded_channel();
        let (resize_tx, _) = mpsc::unbounded_channel();
        let (close_tx, _) = mpsc::unbounded_channel();

        let session = Session::new("host1".to_string(), SessionType::Ssh, input_tx, resize_tx, close_tx);
        let id = session.id.clone();

        let _arc = manager.add(session);
        assert_eq!(manager.list().len(), 1);

        let found = manager.get(&id);
        assert!(found.is_some());

        let removed = manager.remove(&id);
        assert!(removed.is_some());
        assert_eq!(manager.list().len(), 0);
    }

    #[test]
    fn test_session_manager_multiple_sessions() {
        let (event_tx, _rx) = mpsc::unbounded_channel();
        let manager = SessionManager::new(event_tx);

        let kinds = [SessionType::Ssh, SessionType::Serial, SessionType::Telnet, SessionType::Shell];
        for kind in kinds {
            let (input_tx, _) = mpsc::unbounded_channel();
            let (resize_tx, _) = mpsc::unbounded_channel();
            let (close_tx, _) = mpsc::unbounded_channel();
            manager.add(Session::new(format!("{:?}", kind), kind, input_tx, resize_tx, close_tx));
        }

        assert_eq!(manager.list().len(), 4);
    }

    #[test]
    fn test_session_remove_nonexistent() {
        let (event_tx, _rx) = mpsc::unbounded_channel();
        let manager = SessionManager::new(event_tx);

        let result = manager.remove("nonexistent");
        assert!(result.is_none());
    }
}
