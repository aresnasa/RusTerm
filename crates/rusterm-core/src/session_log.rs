use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::Result;
use chrono::Local;

pub struct SessionLog {
    writer: Mutex<Option<fs::File>>,
    session_id: String,
}

impl std::fmt::Debug for SessionLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionLog")
            .field("session_id", &self.session_id)
            .finish()
    }
}

impl SessionLog {
    pub fn new(session_id: &str) -> Result<Self> {
        let log_dir = dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("rusterm")
            .join("session_logs");
        fs::create_dir_all(&log_dir)?;

        let timestamp = Local::now().format("%Y%m%d_%H%M%S");
        let filename = format!("{}_{}.log", session_id, timestamp);
        let path = log_dir.join(&filename);

        let file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;

        Ok(Self {
            writer: Mutex::new(Some(file)),
            session_id: session_id.to_string(),
        })
    }

    pub fn log_output(&self, data: &[u8]) {
        self.write_entry("OUT", data);
    }

    pub fn log_input(&self, data: &[u8]) {
        self.write_entry("IN", data);
    }

    fn write_entry(&self, direction: &str, data: &[u8]) {
        if let Ok(mut guard) = self.writer.lock() {
            if let Some(ref mut file) = *guard {
                let timestamp = Local::now().format("%H:%M:%S%.3f");
                let text = String::from_utf8_lossy(data);
                let line = format!("[{}] [{}] {}\n", timestamp, direction, text);
                let _ = file.write_all(line.as_bytes());
                let _ = file.flush();
            }
        }
    }

    pub fn close(&self) {
        if let Ok(mut guard) = self.writer.lock() {
            *guard = None;
        }
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }
}

impl Drop for SessionLog {
    fn drop(&mut self) {
        self.close();
    }
}
