pub mod serial;
pub mod telnet;
pub mod tcp;
pub mod shell;

pub use serial::SerialConnection;
pub use telnet::TelnetConnection;
pub use tcp::TcpConnection;
pub use shell::ShellConnection;
