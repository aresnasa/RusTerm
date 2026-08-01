use std::fmt;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use rusterm_core::config::{ProxyConfig, ProxyKind};
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, RootCertStore};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_rustls::TlsConnector;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_HTTP_HEADER_SIZE: usize = 16 * 1024;

pub trait AsyncTransport: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T> AsyncTransport for T where T: AsyncRead + AsyncWrite + Unpin + Send {}

pub type BoxedTransport = Box<dyn AsyncTransport>;

#[derive(Debug, Error)]
#[error("SSH transport {stage} failed: {message}")]
pub struct TransportError {
    stage: &'static str,
    message: String,
}

impl TransportError {
    fn new(stage: &'static str, message: impl fmt::Display) -> Self {
        Self {
            stage,
            message: message.to_string(),
        }
    }
}

pub async fn connect_transport(
    target_host: &str,
    target_port: u16,
    proxy: Option<&ProxyConfig>,
) -> Result<BoxedTransport, TransportError> {
    connect_transport_with_tls_config(target_host, target_port, proxy, None).await
}

async fn connect_transport_with_tls_config(
    target_host: &str,
    target_port: u16,
    proxy: Option<&ProxyConfig>,
    tls_config: Option<Arc<ClientConfig>>,
) -> Result<BoxedTransport, TransportError> {
    validate_host(target_host, "target validation")?;

    let Some(proxy) = proxy else {
        return Ok(Box::new(
            connect_tcp(target_host, target_port, "direct TCP connect").await?,
        ) as BoxedTransport);
    };

    validate_host(&proxy.host, "proxy validation")?;
    match &proxy.kind {
        ProxyKind::Http => {
            let mut stream = connect_tcp(&proxy.host, proxy.port, "HTTP proxy TCP connect").await?;
            timeout(
                HANDSHAKE_TIMEOUT,
                http_connect(&mut stream, target_host, target_port, proxy),
            )
            .await
            .map_err(|_| TransportError::new("HTTP CONNECT handshake", "timed out"))??;
            Ok(Box::new(stream))
        }
        ProxyKind::Https => {
            let stream = connect_tcp(&proxy.host, proxy.port, "HTTPS proxy TCP connect").await?;
            let server_name = ServerName::try_from(proxy.host.clone()).map_err(|_| {
                TransportError::new("HTTPS proxy TLS setup", "invalid proxy server name")
            })?;
            let tls_config = tls_config.unwrap_or_else(default_tls_config);
            let mut stream = timeout(
                HANDSHAKE_TIMEOUT,
                TlsConnector::from(tls_config).connect(server_name, stream),
            )
            .await
            .map_err(|_| TransportError::new("HTTPS proxy TLS handshake", "timed out"))?
            .map_err(|error| TransportError::new("HTTPS proxy TLS handshake", error))?;
            timeout(
                HANDSHAKE_TIMEOUT,
                http_connect(&mut stream, target_host, target_port, proxy),
            )
            .await
            .map_err(|_| TransportError::new("HTTPS CONNECT handshake", "timed out"))??;
            Ok(Box::new(stream))
        }
        ProxyKind::Socks5 => {
            let mut stream =
                connect_tcp(&proxy.host, proxy.port, "SOCKS5 proxy TCP connect").await?;
            timeout(
                HANDSHAKE_TIMEOUT,
                socks5_connect(&mut stream, target_host, target_port, proxy),
            )
            .await
            .map_err(|_| TransportError::new("SOCKS5 handshake", "timed out"))??;
            Ok(Box::new(stream))
        }
    }
}

fn default_tls_config() -> Arc<ClientConfig> {
    let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    Arc::new(
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

async fn connect_tcp(
    host: &str,
    port: u16,
    stage: &'static str,
) -> Result<TcpStream, TransportError> {
    let stream = timeout(CONNECT_TIMEOUT, TcpStream::connect((host, port)))
        .await
        .map_err(|_| TransportError::new(stage, "timed out"))?
        .map_err(|error| TransportError::new(stage, error))?;
    stream
        .set_nodelay(true)
        .map_err(|error| TransportError::new(stage, error))?;
    Ok(stream)
}

fn validate_host(host: &str, stage: &'static str) -> Result<(), TransportError> {
    if host.is_empty() || host.bytes().any(|byte| byte.is_ascii_control()) {
        return Err(TransportError::new(stage, "invalid host"));
    }
    Ok(())
}

fn credentials<'a>(
    proxy: &'a ProxyConfig,
    stage: &'static str,
) -> Result<Option<(&'a str, &'a str)>, TransportError> {
    match (&proxy.username, &proxy.password) {
        (None, None) => Ok(None),
        (Some(username), Some(password)) => Ok(Some((username, password))),
        _ => Err(TransportError::new(
            stage,
            "proxy username and password must be provided together",
        )),
    }
}

fn target_authority(host: &str, port: u16) -> String {
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V6(address)) => format!("[{address}]:{port}"),
        _ => format!("{host}:{port}"),
    }
}

async fn http_connect<S>(
    stream: &mut S,
    target_host: &str,
    target_port: u16,
    proxy: &ProxyConfig,
) -> Result<(), TransportError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let authority = target_authority(target_host, target_port);
    let mut request = format!(
        "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\nProxy-Connection: Keep-Alive\r\n"
    );
    if let Some((username, password)) = credentials(proxy, "HTTP CONNECT authentication")? {
        let token =
            base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
        request.push_str("Proxy-Authorization: Basic ");
        request.push_str(&token);
        request.push_str("\r\n");
    }
    request.push_str("\r\n");

    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|error| TransportError::new("HTTP CONNECT request write", error))?;
    stream
        .flush()
        .await
        .map_err(|error| TransportError::new("HTTP CONNECT request flush", error))?;

    let mut header = Vec::with_capacity(512);
    while header.len() < MAX_HTTP_HEADER_SIZE {
        let byte = stream
            .read_u8()
            .await
            .map_err(|error| TransportError::new("HTTP CONNECT response read", error))?;
        header.push(byte);
        if header.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    if !header.ends_with(b"\r\n\r\n") {
        return Err(TransportError::new(
            "HTTP CONNECT response read",
            "response headers exceed 16 KiB",
        ));
    }

    let status_line_end = header
        .windows(2)
        .position(|window| window == b"\r\n")
        .ok_or_else(|| TransportError::new("HTTP CONNECT response parse", "missing status line"))?;
    let status_line = std::str::from_utf8(&header[..status_line_end]).map_err(|_| {
        TransportError::new(
            "HTTP CONNECT response parse",
            "status line is not valid UTF-8",
        )
    })?;
    let mut fields = status_line.split_whitespace();
    let version = fields.next().unwrap_or_default();
    let status = fields
        .next()
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or_else(|| TransportError::new("HTTP CONNECT response parse", "invalid status code"))?;
    if !version.starts_with("HTTP/") {
        return Err(TransportError::new(
            "HTTP CONNECT response parse",
            "invalid HTTP version",
        ));
    }
    if !(200..300).contains(&status) {
        return Err(TransportError::new(
            "HTTP CONNECT response status",
            format_args!("proxy returned status {status}"),
        ));
    }
    Ok(())
}

async fn socks5_connect<S>(
    stream: &mut S,
    target_host: &str,
    target_port: u16,
    proxy: &ProxyConfig,
) -> Result<(), TransportError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let credentials = credentials(proxy, "SOCKS5 authentication setup")?;
    let method = if credentials.is_some() { 0x02 } else { 0x00 };
    stream
        .write_all(&[0x05, 0x01, method])
        .await
        .map_err(|error| TransportError::new("SOCKS5 method negotiation write", error))?;

    let mut method_reply = [0_u8; 2];
    stream
        .read_exact(&mut method_reply)
        .await
        .map_err(|error| TransportError::new("SOCKS5 method negotiation read", error))?;
    if method_reply[0] != 0x05 {
        return Err(TransportError::new(
            "SOCKS5 method negotiation",
            "invalid protocol version",
        ));
    }
    if method_reply[1] != method {
        let message = if method_reply[1] == 0xff {
            "proxy rejected all offered authentication methods"
        } else {
            "proxy selected an authentication method that was not offered"
        };
        return Err(TransportError::new("SOCKS5 method negotiation", message));
    }

    if let Some((username, password)) = credentials {
        let username = username.as_bytes();
        let password = password.as_bytes();
        if username.is_empty() || username.len() > u8::MAX as usize {
            return Err(TransportError::new(
                "SOCKS5 username/password authentication",
                "username length must be between 1 and 255 bytes",
            ));
        }
        if password.is_empty() || password.len() > u8::MAX as usize {
            return Err(TransportError::new(
                "SOCKS5 username/password authentication",
                "password length must be between 1 and 255 bytes",
            ));
        }
        let mut request = Vec::with_capacity(username.len() + password.len() + 3);
        request.extend_from_slice(&[0x01, username.len() as u8]);
        request.extend_from_slice(username);
        request.push(password.len() as u8);
        request.extend_from_slice(password);
        stream.write_all(&request).await.map_err(|error| {
            TransportError::new("SOCKS5 username/password authentication write", error)
        })?;

        let mut auth_reply = [0_u8; 2];
        stream.read_exact(&mut auth_reply).await.map_err(|error| {
            TransportError::new("SOCKS5 username/password authentication read", error)
        })?;
        if auth_reply[0] != 0x01 || auth_reply[1] != 0x00 {
            return Err(TransportError::new(
                "SOCKS5 username/password authentication",
                "proxy rejected credentials",
            ));
        }
    }

    let mut request = vec![0x05, 0x01, 0x00];
    match target_host.parse::<IpAddr>() {
        Ok(IpAddr::V4(address)) => {
            request.push(0x01);
            request.extend_from_slice(&address.octets());
        }
        Ok(IpAddr::V6(address)) => {
            request.push(0x04);
            request.extend_from_slice(&address.octets());
        }
        Err(_) => {
            let host = target_host.as_bytes();
            if host.len() > u8::MAX as usize {
                return Err(TransportError::new(
                    "SOCKS5 target encoding",
                    "target domain exceeds 255 bytes",
                ));
            }
            request.extend_from_slice(&[0x03, host.len() as u8]);
            request.extend_from_slice(host);
        }
    }
    request.extend_from_slice(&target_port.to_be_bytes());
    stream
        .write_all(&request)
        .await
        .map_err(|error| TransportError::new("SOCKS5 CONNECT request write", error))?;

    let mut reply = [0_u8; 4];
    stream
        .read_exact(&mut reply)
        .await
        .map_err(|error| TransportError::new("SOCKS5 CONNECT response read", error))?;
    if reply[0] != 0x05 || reply[2] != 0x00 {
        return Err(TransportError::new(
            "SOCKS5 CONNECT response parse",
            "invalid response header",
        ));
    }
    if reply[1] != 0x00 {
        return Err(TransportError::new(
            "SOCKS5 CONNECT response status",
            format_args!("proxy returned reply code {}", reply[1]),
        ));
    }

    match reply[3] {
        0x01 => {
            let mut address = [0_u8; 4];
            stream
                .read_exact(&mut address)
                .await
                .map_err(|error| TransportError::new("SOCKS5 CONNECT bound address read", error))?;
        }
        0x03 => {
            let length =
                stream.read_u8().await.map_err(|error| {
                    TransportError::new("SOCKS5 CONNECT bound domain read", error)
                })? as usize;
            let mut address = vec![0_u8; length];
            stream
                .read_exact(&mut address)
                .await
                .map_err(|error| TransportError::new("SOCKS5 CONNECT bound domain read", error))?;
        }
        0x04 => {
            let mut address = [0_u8; 16];
            stream
                .read_exact(&mut address)
                .await
                .map_err(|error| TransportError::new("SOCKS5 CONNECT bound address read", error))?;
        }
        _ => {
            return Err(TransportError::new(
                "SOCKS5 CONNECT response parse",
                "invalid bound address type",
            ));
        }
    }
    let mut bound_port = [0_u8; 2];
    stream
        .read_exact(&mut bound_port)
        .await
        .map_err(|error| TransportError::new("SOCKS5 CONNECT bound port read", error))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::generate_simple_self_signed;
    use rustls::pki_types::PrivateKeyDer;
    use rustls::{RootCertStore, ServerConfig};
    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;

    async fn read_http_header<S>(stream: &mut S) -> Vec<u8>
    where
        S: AsyncRead + Unpin,
    {
        let mut header = Vec::new();
        while !header.ends_with(b"\r\n\r\n") {
            header.push(stream.read_u8().await.unwrap());
        }
        header
    }

    async fn assert_bidirectional_tunnel<S>(stream: &mut S)
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let mut inbound = [0_u8; 4];
        stream.read_exact(&mut inbound).await.unwrap();
        assert_eq!(&inbound, b"ping");
        stream.write_all(b"pong").await.unwrap();
    }

    #[tokio::test]
    async fn http_connect_sends_authority_and_basic_auth_then_tunnels() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = String::from_utf8(read_http_header(&mut stream).await).unwrap();
            assert!(request.starts_with("CONNECT ssh.example:2222 HTTP/1.1\r\n"));
            assert!(request.contains("\r\nHost: ssh.example:2222\r\n"));
            assert!(request.contains("\r\nProxy-Authorization: Basic YWxpY2U6c2VjcmV0\r\n"));
            stream
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await
                .unwrap();
            assert_bidirectional_tunnel(&mut stream).await;
        });

        let proxy = ProxyConfig {
            kind: ProxyKind::Http,
            host: "127.0.0.1".to_string(),
            port,
            username: Some("alice".to_string()),
            password: Some("secret".to_string()),
        };
        let mut stream = connect_transport("ssh.example", 2222, Some(&proxy))
            .await
            .unwrap();
        stream.write_all(b"ping").await.unwrap();
        let mut response = [0_u8; 4];
        stream.read_exact(&mut response).await.unwrap();
        assert_eq!(&response, b"pong");
        server.await.unwrap();
    }

    async fn run_socks5_test(with_credentials: bool) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut greeting = [0_u8; 3];
            stream.read_exact(&mut greeting).await.unwrap();
            let method = if with_credentials { 0x02 } else { 0x00 };
            assert_eq!(greeting, [0x05, 0x01, method]);
            stream.write_all(&[0x05, method]).await.unwrap();

            if with_credentials {
                let mut auth_prefix = [0_u8; 2];
                stream.read_exact(&mut auth_prefix).await.unwrap();
                assert_eq!(auth_prefix, [0x01, 0x05]);
                let mut username = [0_u8; 5];
                stream.read_exact(&mut username).await.unwrap();
                assert_eq!(&username, b"alice");
                assert_eq!(stream.read_u8().await.unwrap(), 6);
                let mut password = [0_u8; 6];
                stream.read_exact(&mut password).await.unwrap();
                assert_eq!(&password, b"secret");
                stream.write_all(&[0x01, 0x00]).await.unwrap();
            }

            let mut request_prefix = [0_u8; 5];
            stream.read_exact(&mut request_prefix).await.unwrap();
            assert_eq!(request_prefix, [0x05, 0x01, 0x00, 0x03, 0x0b]);
            let mut domain = [0_u8; 11];
            stream.read_exact(&mut domain).await.unwrap();
            assert_eq!(&domain, b"ssh.example");
            let mut target_port = [0_u8; 2];
            stream.read_exact(&mut target_port).await.unwrap();
            assert_eq!(u16::from_be_bytes(target_port), 2222);
            stream
                .write_all(&[0x05, 0x00, 0x00, 0x01, 127, 0, 0, 1, 0, 0])
                .await
                .unwrap();
            assert_bidirectional_tunnel(&mut stream).await;
        });

        let proxy = ProxyConfig {
            kind: ProxyKind::Socks5,
            host: "127.0.0.1".to_string(),
            port,
            username: with_credentials.then(|| "alice".to_string()),
            password: with_credentials.then(|| "secret".to_string()),
        };
        let mut stream = connect_transport("ssh.example", 2222, Some(&proxy))
            .await
            .unwrap();
        stream.write_all(b"ping").await.unwrap();
        let mut response = [0_u8; 4];
        stream.read_exact(&mut response).await.unwrap();
        assert_eq!(&response, b"pong");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn socks5_no_auth_preserves_domain_target_and_tunnels() {
        run_socks5_test(false).await;
    }

    #[tokio::test]
    async fn socks5_rfc1929_auth_preserves_domain_target_and_tunnels() {
        run_socks5_test(true).await;
    }

    #[tokio::test]
    async fn https_connect_validates_injected_root_then_tunnels() {
        let certified = generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let certificate = certified.cert.der().clone();
        let private_key = PrivateKeyDer::Pkcs8(certified.signing_key.serialize_der().into());

        let server_config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![certificate.clone()], private_key)
            .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(server_config));
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = acceptor.accept(stream).await.unwrap();
            let request = String::from_utf8(read_http_header(&mut stream).await).unwrap();
            assert!(request.starts_with("CONNECT ssh.example:22 HTTP/1.1\r\n"));
            stream
                .write_all(b"HTTP/1.1 204 Tunnel Ready\r\n\r\n")
                .await
                .unwrap();
            assert_bidirectional_tunnel(&mut stream).await;
        });

        let mut roots = RootCertStore::empty();
        roots.add(certificate).unwrap();
        let client_config = Arc::new(
            ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        );
        let proxy = ProxyConfig {
            kind: ProxyKind::Https,
            host: "localhost".to_string(),
            port,
            username: None,
            password: None,
        };
        let mut stream =
            connect_transport_with_tls_config("ssh.example", 22, Some(&proxy), Some(client_config))
                .await
                .unwrap();
        stream.write_all(b"ping").await.unwrap();
        let mut response = [0_u8; 4];
        stream.read_exact(&mut response).await.unwrap();
        assert_eq!(&response, b"pong");
        server.await.unwrap();
    }
}
