//! Minimal SOCKS5 server: no-auth, CONNECT only. This is the local side of
//! a `ssh -D` dynamic forward — each inbound SOCKS5 request maps to one
//! `direct-tcpip` channel on the SSH connection.
//!
//! Only what `curl --socks5`, browsers and brew-style tools need:
//!
//! ```text
//! greeting: 05 nmethods methods...  → reply 05 00 (no auth)
//! request:  05 cmd=01 rsv atyp dst port → open direct-tcpip → reply 05 00 ...
//! then plain bidirectional copy.
//! ```

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use anyhow::{Context, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use rusterm_ssh::DirectHandle;

/// SOCKS5 reply codes we emit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Reply {
    Succeeded = 0x00,
    GeneralFailure = 0x01,
    RuleDenied = 0x02,
    NetworkUnreachable = 0x03,
    HostUnreachable = 0x04,
    ConnectionRefused = 0x05,
    CommandNotSupported = 0x07,
    AddressTypeNotSupported = 0x08,
}

/// A parsed CONNECT target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Ip(IpAddr, u16),
    Domain(String, u16),
}

impl Target {
    pub fn host(&self) -> String {
        match self {
            Target::Ip(ip, _) => ip.to_string(),
            Target::Domain(d, _) => d.clone(),
        }
    }
    pub fn port(&self) -> u16 {
        match self {
            Target::Ip(_, p) | Target::Domain(_, p) => *p,
        }
    }
}

/// Read the client greeting and select "no authentication" (0x00).
/// Returns an error if the client doesn't support no-auth.
async fn negotiate(stream: &mut TcpStream) -> anyhow::Result<()> {
    let version = stream.read_u8().await?;
    if version != 0x05 {
        bail!("unsupported socks version {version}");
    }
    let nmethods = stream.read_u8().await?;
    let mut methods = vec![0u8; nmethods as usize];
    stream.read_exact(&mut methods).await?;
    if !methods.contains(&0x00) {
        stream.write_all(&[0x05, 0xff]).await.ok();
        bail!("client does not support no-auth");
    }
    stream.write_all(&[0x05, 0x00]).await?;
    Ok(())
}

/// Parse the request header (we support CONNECT only; anything else gets
/// `CommandNotSupported`).
async fn read_request(stream: &mut TcpStream) -> Result<Target, Reply> {
    let mut header = [0u8; 4];
    stream
        .read_exact(&mut header)
        .await
        .map_err(|_| Reply::GeneralFailure)?;
    let [version, cmd, _rsv, atyp] = header;
    if version != 0x05 {
        return Err(Reply::GeneralFailure);
    }
    if cmd != 0x01 {
        return Err(Reply::CommandNotSupported);
    }
    let target = match atyp {
        0x01 => {
            let mut octets = [0u8; 4];
            stream
                .read_exact(&mut octets)
                .await
                .map_err(|_| Reply::GeneralFailure)?;
            let port_hi = stream.read_u8().await.map_err(|_| Reply::GeneralFailure)?;
            let port_lo = stream.read_u8().await.map_err(|_| Reply::GeneralFailure)?;
            Target::Ip(
                IpAddr::V4(Ipv4Addr::from(octets)),
                u16::from_be_bytes([port_hi, port_lo]),
            )
        }
        0x04 => {
            let mut octets = [0u8; 16];
            stream
                .read_exact(&mut octets)
                .await
                .map_err(|_| Reply::GeneralFailure)?;
            let port_hi = stream.read_u8().await.map_err(|_| Reply::GeneralFailure)?;
            let port_lo = stream.read_u8().await.map_err(|_| Reply::GeneralFailure)?;
            Target::Ip(
                IpAddr::V6(Ipv6Addr::from(octets)),
                u16::from_be_bytes([port_hi, port_lo]),
            )
        }
        0x03 => {
            let len = stream.read_u8().await.map_err(|_| Reply::GeneralFailure)? as usize;
            if len == 0 {
                return Err(Reply::AddressTypeNotSupported);
            }
            let mut bytes = vec![0u8; len];
            stream
                .read_exact(&mut bytes)
                .await
                .map_err(|_| Reply::GeneralFailure)?;
            let port_hi = stream.read_u8().await.map_err(|_| Reply::GeneralFailure)?;
            let port_lo = stream.read_u8().await.map_err(|_| Reply::GeneralFailure)?;
            let domain = String::from_utf8(bytes).map_err(|_| Reply::AddressTypeNotSupported)?;
            Target::Domain(domain, u16::from_be_bytes([port_hi, port_lo]))
        }
        _ => return Err(Reply::AddressTypeNotSupported),
    };
    Ok(target)
}

async fn send_reply(stream: &mut TcpStream, reply: Reply) -> anyhow::Result<()> {
    // BND.ADDR/BND.PORT are zeroes; RFC 1928 allows "0" values when not
    // meaningful and every client tolerates them for CONNECT.
    let mut buf = Vec::with_capacity(10);
    buf.extend_from_slice(&[0x05, reply as u8, 0x00, 0x01]);
    buf.extend_from_slice(&[0, 0, 0, 0]);
    buf.extend_from_slice(&[0, 0]);
    stream.write_all(&buf).await?;
    Ok(())
}

/// Serve one SOCKS5 connection to completion. Errors are logged by the
/// caller; this function is intentionally self-contained.
pub async fn serve(stream: TcpStream, handle: DirectHandle) -> anyhow::Result<()> {
    let mut stream = stream;
    negotiate(&mut stream).await?;

    let target = match read_request(&mut stream).await {
        Ok(t) => t,
        Err(reply) => {
            send_reply(&mut stream, reply).await.ok();
            bail!("socks5 request rejected: {reply:?}");
        }
    };

    let peer = stream.peer_addr().ok();
    let originator = peer
        .map(|SocketAddr { ip, port }| (ip.to_string(), port))
        .unwrap_or_else(|| ("127.0.0.1".to_string(), 0));

    let mut upstream = match handle
        .open_direct_tcpip(&target.host(), target.port(), (&originator.0, originator.1))
        .await
    {
        Ok(s) => s,
        Err(e) => {
            send_reply(&mut stream, Reply::ConnectionRefused)
                .await
                .context("sending refusal")?;
            bail!(
                "direct-tcpip to {}:{} failed: {e}",
                target.host(),
                target.port()
            );
        }
    };

    send_reply(&mut stream, Reply::Succeeded).await?;

    tokio::io::copy_bidirectional(&mut stream, &mut upstream)
        .await
        .context("proxying socks5 stream")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    /// Parse a request from raw bytes without touching a real socket: the
    /// framing logic is the interesting part, and `tokio::io::duplex` gives
    /// us a `DuplexStream`, which isn't a `TcpStream`... so instead we spin
    /// up a loopback TCP pair for these tests.
    async fn connect_pair() -> (TcpStream, TcpStream) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();
        (client, server)
    }

    #[tokio::test]
    async fn negotiate_accepts_no_auth() {
        let (mut client, mut server) = connect_pair().await;
        let server_task = tokio::spawn(async move { negotiate(&mut server).await });
        client.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
        let mut reply = [0u8; 2];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply, [0x05, 0x00]);
        server_task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn negotiate_rejects_when_no_noauth_offered() {
        let (mut client, mut server) = connect_pair().await;
        let server_task = tokio::spawn(async move { negotiate(&mut server).await });
        // Only GSSAPI offered.
        client.write_all(&[0x05, 0x01, 0x01]).await.unwrap();
        let mut reply = [0u8; 2];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply, [0x05, 0xff]);
        assert!(server_task.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn parses_ipv4_connect() {
        let (mut client, mut server) = connect_pair().await;
        let task = tokio::spawn(async move { read_request(&mut server).await });
        // CONNECT 93.184.216.34:443
        let mut req = vec![0x05, 0x01, 0x00, 0x01, 93, 184, 216, 34];
        req.extend_from_slice(&443u16.to_be_bytes());
        client.write_all(&req).await.unwrap();
        let target = task.await.unwrap().unwrap();
        assert_eq!(
            target,
            Target::Ip(IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)), 443)
        );
    }

    #[tokio::test]
    async fn parses_domain_connect() {
        let (mut client, mut server) = connect_pair().await;
        let task = tokio::spawn(async move { read_request(&mut server).await });
        let mut req = vec![0x05, 0x01, 0x00, 0x03, 11];
        req.extend_from_slice(b"example.com");
        req.extend_from_slice(&80u16.to_be_bytes());
        client.write_all(&req).await.unwrap();
        let target = task.await.unwrap().unwrap();
        assert_eq!(target, Target::Domain("example.com".into(), 80));
    }

    #[tokio::test]
    async fn non_connect_unsupported() {
        let (mut client, mut server) = connect_pair().await;
        let task = tokio::spawn(async move { read_request(&mut server).await });
        // BIND request.
        client.write_all(&[0x05, 0x02, 0x00, 0x01]).await.unwrap();
        let err = task.await.unwrap().unwrap_err();
        assert_eq!(err, Reply::CommandNotSupported);
    }
}
