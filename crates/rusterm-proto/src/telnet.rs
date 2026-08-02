//! Telnet (RFC 854) connection bridge.
//!
//! Opens a TCP stream to `host:port` and wires it into the session event
//! loop. The implementation is **transport-only**: it shuttles bytes between
//! the terminal and the remote end, and does not implement the telnet option
//! negotiation protocol (IAC DO/WILL/etc.).
//!
//! Most modern "telnet" targets (routers, switches, IPMI BMCs, PDU consoles,
//! debug serial-over-network servers) either:
//!
//! 1. send no negotiation at all and just stream bytes (the common case for
//!    embedded devices), or
//! 2. send a minimal IAC sequence at connect time that the remote shell
//!    ignores if unanswered.
//!
//! For full telnet option negotiation, a future revision can add a thin IAC
//! filter on the read path that responds WONT/DONT to everything. For now,
//! transparent byte streaming matches what users expect when they type
//! `telnet host 23` from a terminal and get a login prompt.
//!
//! ## Threading model
//!
//! Telnet runs on the Tokio runtime (unlike serial, which uses blocking
//! threads) because `tokio::net::TcpStream` is async-native. We split the
//! stream into read/write halves and run each half as a `tokio::spawn`'d
//! task. The write task selects on `input_rx` vs `close_rx` so a close
//! request interrupts a pending write immediately.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use rusterm_core::config::TelnetConfig;
use rusterm_core::event::SessionEvent;
use rusterm_core::session::{Session, SessionId, SessionType};

pub struct TelnetConnection;

impl TelnetConnection {
    /// Open a TCP connection to `host:port` and return a live [`Session`]
    /// wired to it.
    pub async fn connect(
        config: &TelnetConfig,
        session_id: SessionId,
        event_tx: mpsc::UnboundedSender<SessionEvent>,
    ) -> anyhow::Result<Session> {
        let stream = TcpStream::connect((config.host.as_str(), config.port)).await?;
        // Disable Nagle so keystrokes reach the remote end without the 40ms
        // coalescing delay — critical for interactive telnet sessions.
        let _ = stream.set_nodelay(true);

        let (mut read_half, mut write_half) = stream.into_split();

        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (resize_tx, mut resize_rx) = mpsc::unbounded_channel::<(u16, u16, u32, u32)>();
        let (close_tx, mut close_rx) = mpsc::unbounded_channel::<()>();

        let session = Session::with_id(
            session_id.clone(),
            format!("Telnet {}:{}", config.host, config.port),
            SessionType::Telnet,
            input_tx,
            resize_tx,
            close_tx,
        );

        let sid_read = session_id.clone();
        let evt_read = event_tx.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            loop {
                match read_half.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        let bytes = buf[..n].to_vec();
                        if evt_read
                            .send(SessionEvent::Output(sid_read.clone(), bytes))
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            let _ = evt_read.send(SessionEvent::Disconnected(
                sid_read,
                "Telnet connection closed".to_string(),
            ));
        });

        let sid_write = session_id.clone();
        let evt_write = event_tx.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some(data) = input_rx.recv() => {
                        if write_half.write_all(&data).await.is_err() {
                            break;
                        }
                        let _ = write_half.flush().await;
                    }
                    Some(_) = close_rx.recv() => break,
                    else => break,
                }
            }
            let _ = evt_write.send(SessionEvent::Disconnected(
                sid_write,
                "Telnet closed".to_string(),
            ));
        });

        // Telnet has no PTY geometry, but drain the channel so callers that
        // always send a resize event don't fill the channel.
        tokio::spawn(async move {
            while let Some((_cols, _rows, _pw, _ph)) = resize_rx.recv().await {
                // Intentionally ignored.
            }
        });

        let _ = event_tx.send(SessionEvent::Connected(session_id));
        Ok(session)
    }
}
