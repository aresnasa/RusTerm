use std::time::Duration;

use serialport::{self, SerialPort};
use tokio::sync::mpsc;

use rusterm_core::config::SerialConfig;
use rusterm_core::event::SessionEvent;
use rusterm_core::session::{Session, SessionType};

pub struct SerialConnection;

impl SerialConnection {
    pub fn open(
        config: &SerialConfig,
        _event_tx: mpsc::UnboundedSender<SessionEvent>,
    ) -> anyhow::Result<(Session, Box<dyn SerialPort>)> {
        let port = serialport::new(&config.port, config.baud_rate)
            .data_bits(match config.data_bits {
                5 => serialport::DataBits::Five,
                6 => serialport::DataBits::Six,
                7 => serialport::DataBits::Seven,
                _ => serialport::DataBits::Eight,
            })
            .parity(match config.parity.as_str() {
                "odd" => serialport::Parity::Odd,
                "even" => serialport::Parity::Even,
                _ => serialport::Parity::None,
            })
            .stop_bits(match config.stop_bits {
                2 => serialport::StopBits::Two,
                _ => serialport::StopBits::One,
            })
            .flow_control(match config.flow_control.as_str() {
                "hardware" => serialport::FlowControl::Hardware,
                "software" => serialport::FlowControl::Software,
                _ => serialport::FlowControl::None,
            })
            .timeout(Duration::from_secs(1))
            .open()?;

        let (input_tx, _) = mpsc::unbounded_channel::<Vec<u8>>();
        let (resize_tx, _) = mpsc::unbounded_channel::<(u16, u16, u32, u32)>();
        let (close_tx, _) = mpsc::unbounded_channel::<()>();

        let session = Session::new(
            format!("Serial {}", config.port),
            SessionType::Serial,
            input_tx,
            resize_tx,
            close_tx,
        );

        Ok((session, port))
    }
}
