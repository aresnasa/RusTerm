pub mod serial;
pub mod shell;
pub mod tcp;
pub mod telnet;

pub use serial::{SerialConnection, list_available_ports};
pub use shell::ShellConnection;
pub use tcp::TcpConnection;
pub use telnet::TelnetConnection;
