use tokio::net::TcpStream;
use tokio::sync::mpsc;

use rusterm_core::config::TelnetConfig;
use rusterm_core::event::SessionEvent;
use rusterm_core::session::{Session, SessionType};

pub struct TelnetConnection;

impl TelnetConnection {
    pub async fn connect(
        config: &TelnetConfig,
        _event_tx: mpsc::UnboundedSender<SessionEvent>,
    ) -> anyhow::Result<(Session, TcpStream)> {
        let stream = TcpStream::connect((config.host.as_str(), config.port)).await?;

        let (input_tx, _) = mpsc::unbounded_channel::<Vec<u8>>();
        let (resize_tx, _) = mpsc::unbounded_channel::<(u16, u16, u32, u32)>();
        let (close_tx, _) = mpsc::unbounded_channel::<()>();

        let session = Session::new(
            format!("Telnet {}:{}", config.host, config.port),
            SessionType::Telnet,
            input_tx,
            resize_tx,
            close_tx,
        );

        Ok((session, stream))
    }
}
