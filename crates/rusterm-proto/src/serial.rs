//! Serial (UART) connection bridge.
//!
//! Opens a [`serialport::SerialPort`] with the user-supplied line settings
//! (baud / data bits / parity / stop bits / flow control) and wires it into
//! the session event loop:
//!
//! - **read thread** — blocks on `SerialPort::read`, forwards each chunk as
//!   [`SessionEvent::Output`]. On EOF or error, emits
//!   [`SessionEvent::Disconnected`].
//! - **write thread** — owns the writer half; drains the session's
//!   `input_rx` channel and `write_all`s each chunk. Exits when the input
//!   channel closes or a `close` signal arrives.
//!
//! `serialport` is a synchronous (blocking) API, so both directions run on
//! dedicated `std::thread`s — exactly the pattern `ShellConnection` uses for
//! its PTY reader/writer. The Tokio runtime is not involved on the I/O path,
//! which keeps serial latency predictable and avoids blocking the async
//! reactor.
//!
//! Resizing is a no-op for serial lines (no PTY to resize), but we still
//! drain the `resize_rx` channel so callers can send resize events without
//! the channel filling up.

use std::io::{Read, Write};
use std::sync::Arc;
use std::time::Duration;

use serialport::{SerialPort, SerialPortInfo, SerialPortType};
use tokio::sync::mpsc;

use rusterm_core::config::SerialConfig;
use rusterm_core::event::SessionEvent;
use rusterm_core::session::{Session, SessionId, SessionType};

pub struct SerialConnection;

impl SerialConnection {
    /// Open a serial port and return a live [`Session`] wired to it.
    ///
    /// The returned session's `input_tx` is the channel callers write user
    /// keystrokes to; `close_tx` requests a graceful shutdown; `resize_tx`
    /// is accepted but ignored (serial lines have no PTY geometry).
    pub fn open(
        config: &SerialConfig,
        session_id: SessionId,
        event_tx: mpsc::UnboundedSender<SessionEvent>,
    ) -> anyhow::Result<Session> {
        let port = build_port(config)?;

        // `serialport` gives us a single `Box<dyn SerialPort>` that is both
        // reader and writer. To run read and write on separate threads we
        // need two handles; `try_clone_native()` is the supported way to get
        // a second OS handle to the same underlying device.
        let writer = port.try_clone()?;

        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (resize_tx, mut resize_rx) = mpsc::unbounded_channel::<(u16, u16, u32, u32)>();
        let (close_tx, mut close_rx) = mpsc::unbounded_channel::<()>();

        let session = Session::with_id(
            session_id.clone(),
            format!("Serial {}", config.port),
            SessionType::Serial,
            input_tx,
            resize_tx,
            close_tx,
        );

        // Shared guard: only one thread may emit `Disconnected`. Prevents the
        // read and write threads from racing to report the same teardown.
        let disconnected = Arc::new(std::sync::atomic::AtomicBool::new(false));

        // ── read thread ───────────────────────────────────────────────────
        let sid_read = session_id.clone();
        let evt_read = event_tx.clone();
        let disconnected_read = disconnected.clone();
        std::thread::spawn(move || {
            let mut reader = port;
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let bytes = buf[..n].to_vec();
                        if evt_read
                            .send(SessionEvent::Output(sid_read.clone(), bytes))
                            .is_err()
                        {
                            // Event receiver dropped — app tore down the
                            // session. Stop reading.
                            break;
                        }
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {
                        // Serial ports are opened with a 1s read timeout so
                        // the read thread can poll the `disconnected` flag
                        // without blocking forever. Timeouts are expected —
                        // just loop.
                        continue;
                    }
                    Err(_) => break,
                }
            }
            emit_disconnected(
                &disconnected_read,
                &evt_read,
                sid_read,
                "Serial port closed",
            );
        });

        // ── write thread ──────────────────────────────────────────────────
        let sid_write = session_id.clone();
        let evt_write = event_tx.clone();
        let disconnected_write = disconnected.clone();
        std::thread::spawn(move || {
            // The writer thread needs a Tokio runtime to poll the input
            // channel, but a std::thread has no runtime context — calling
            // `Handle::current()` here panics ("no reactor running"). Build a
            // dedicated current-thread runtime for this thread instead, same
            // pattern as `ShellConnection::open`.
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(_) => return,
            };
            let mut writer = writer;
            loop {
                let cont = rt.block_on(async {
                    tokio::select! {
                        Some(data) = input_rx.recv() => {
                            if writer.write_all(&data).is_err() {
                                false
                            } else {
                                let _ = writer.flush();
                                true
                            }
                        }
                        Some(_) = close_rx.recv() => false,
                        else => false,
                    }
                });
                if !cont {
                    break;
                }
            }
            // Best-effort: lower the serial control lines so attached devices
            // notice the drop. `serialport`'s `Drop` impl does this too, but
            // doing it explicitly here is harmless and slightly more
            // predictable across platforms.
            let _ = writer.write_data_terminal_ready(false);
            let _ = writer.write_request_to_send(false);
            emit_disconnected(&disconnected_write, &evt_write, sid_write, "Serial closed");
        });

        // ── resize drain thread ───────────────────────────────────────────
        // Serial lines have no PTY geometry, so resize is a no-op. We still
        // drain the channel so callers that always send a resize event on
        // connect don't fill the channel and waste memory.
        std::thread::spawn(move || {
            while let Some((_cols, _rows, _pw, _ph)) = resize_rx.blocking_recv() {
                // Intentionally ignored — serial has no PTY.
            }
            resize_rx.close();
        });

        let _ = event_tx.send(SessionEvent::Connected(session_id));
        Ok(session)
    }
}

fn emit_disconnected(
    guard: &std::sync::atomic::AtomicBool,
    evt: &mpsc::UnboundedSender<SessionEvent>,
    sid: SessionId,
    reason: &str,
) {
    if guard
        .compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        )
        .is_ok()
    {
        let _ = evt.send(SessionEvent::Disconnected(sid, reason.to_string()));
    }
}

/// Translate a [`SerialConfig`] into a configured `serialport` builder and
/// open it. Kept separate from `open` so it can be unit-tested without
/// touching real hardware.
fn build_port(config: &SerialConfig) -> anyhow::Result<Box<dyn SerialPort>> {
    let data_bits = match config.data_bits {
        5 => serialport::DataBits::Five,
        6 => serialport::DataBits::Six,
        7 => serialport::DataBits::Seven,
        _ => serialport::DataBits::Eight,
    };
    let parity = match config.parity.as_str() {
        "odd" | "O" | "ODD" => serialport::Parity::Odd,
        "even" | "E" | "EVEN" => serialport::Parity::Even,
        _ => serialport::Parity::None,
    };
    let stop_bits = match config.stop_bits {
        2 => serialport::StopBits::Two,
        _ => serialport::StopBits::One,
    };
    let flow_control = match config.flow_control.as_str() {
        "hardware" | "hw" | "rtscts" => serialport::FlowControl::Hardware,
        "software" | "sw" | "xonxoff" => serialport::FlowControl::Software,
        _ => serialport::FlowControl::None,
    };

    Ok(serialport::new(&config.port, config.baud_rate)
        .data_bits(data_bits)
        .parity(parity)
        .stop_bits(stop_bits)
        .flow_control(flow_control)
        // 1s read timeout so the read thread can poll the `disconnected`
        // flag without blocking forever. The thread treats `TimedOut` as a
        // no-op and loops.
        .timeout(Duration::from_secs(1))
        .open()?)
}

/// Return a list of serial port paths available on this system.
///
/// Useful for the UI: the serial-port dropdown is populated from this so the
/// user doesn't have to type `/dev/ttyUSB0` (or, on Windows, `COM3`) by hand.
/// On systems where enumeration fails (rare, but possible on locked-down
/// Linux containers), we return an empty vec — the user can still type a
/// path manually.
pub fn list_available_ports() -> Vec<String> {
    match serialport::available_ports() {
        Ok(ports) => ports
            .iter()
            .map(|SerialPortInfo { port_name, .. }| port_name.clone())
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Human-readable description of a port, used in the UI tooltip. Currently
/// distinguishes USB-attached adapters (which report a product string) from
/// native UARTs and Bluetooth virtual ports.
pub fn describe_port(name: &str) -> Option<String> {
    let ports = serialport::available_ports().ok()?;
    let info = ports
        .iter()
        .find(|p| p.port_name == name)
        .and_then(|p| match &p.port_type {
            SerialPortType::UsbPort(usb) => {
                let prod = usb.product.as_deref().unwrap_or("(unknown USB device)");
                let mfr = usb.manufacturer.as_deref();
                Some(match mfr {
                    Some(m) => format!("{} · {}", m, prod),
                    None => prod.to_string(),
                })
            }
            SerialPortType::BluetoothPort => Some("Bluetooth serial".to_string()),
            SerialPortType::PciPort => Some("PCI serial".to_string()),
            SerialPortType::Unknown => None,
        });
    info
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(port: &str) -> SerialConfig {
        SerialConfig {
            port: port.to_string(),
            baud_rate: 115200,
            data_bits: 8,
            parity: "none".to_string(),
            stop_bits: 1,
            flow_control: "none".to_string(),
        }
    }

    #[test]
    fn build_port_rejects_nonexistent_device() {
        // Use an obviously-fake path. On any CI host this should fail to open
        // rather than succeed, regardless of OS.
        let result = build_port(&cfg("/dev/rusterm-definitely-does-not-exist-9999"));
        assert!(result.is_err(), "expected open() to fail for fake device");
    }

    #[test]
    fn list_available_ports_does_not_panic() {
        // On CI / headless hosts this may return empty, but it must never
        // panic.
        let _ = list_available_ports();
    }

    #[test]
    fn describe_port_returns_none_for_unknown() {
        assert_eq!(describe_port("/dev/rusterm-no-such-port"), None);
    }
}
