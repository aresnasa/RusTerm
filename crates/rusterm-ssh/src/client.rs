use std::sync::Arc;

use russh::client;
use russh::ChannelMsg;
use tokio::sync::mpsc;

use rusterm_core::config::{SshAuth, SshConfig};
use rusterm_core::event::SessionEvent;
use rusterm_core::session::{Session, SessionId, SessionType};
use rusterm_core::terminal::TerminalSize;

#[derive(Debug)]
pub struct Handler;

impl client::Handler for Handler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &russh::keys::ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

pub struct SshClient {
    config: SshConfig,
    event_tx: mpsc::UnboundedSender<SessionEvent>,
}

impl SshClient {
    pub fn new(config: SshConfig, event_tx: mpsc::UnboundedSender<SessionEvent>) -> Self {
        Self { config, event_tx }
    }

    pub async fn connect(
        &self,
        session_id: SessionId,
        size: TerminalSize,
    ) -> anyhow::Result<(Session, SshSession)> {
        let config = Arc::new(client::Config::default());

        let mut handle =
            client::connect(config, (self.config.host.as_str(), self.config.port), Handler).await?;

        match &self.config.auth {
            SshAuth::Password { password } => {
                let result = handle
                    .authenticate_password(&self.config.username, password.as_str())
                    .await?;
                if !matches!(result, client::AuthResult::Success) {
                    anyhow::bail!("SSH password authentication failed");
                }
            }
            SshAuth::Key {
                private_key_path,
                passphrase,
            } => {
                let expanded_path = if private_key_path.starts_with("~/") {
                    if let Some(home) = dirs::home_dir() {
                        home.join(&private_key_path[2..]).to_string_lossy().to_string()
                    } else {
                        private_key_path.clone()
                    }
                } else {
                    private_key_path.clone()
                };
                let key_data = std::fs::read_to_string(&expanded_path)?;
                let key = russh::keys::ssh_key::PrivateKey::from_openssh(&key_data)?;
                let key = if let Some(pass) = passphrase {
                    key.decrypt(pass.as_bytes())?
                } else {
                    key
                };

                let best_rsa_hash = handle.best_supported_rsa_hash().await?;
                let hash_alg = best_rsa_hash.flatten();

                let key_with_alg =
                    russh::keys::PrivateKeyWithHashAlg::new(Arc::new(key), hash_alg);
                let result = handle
                    .authenticate_publickey(&self.config.username, key_with_alg)
                    .await?;
                if !matches!(result, client::AuthResult::Success) {
                    anyhow::bail!("SSH key authentication failed");
                }
            }
            SshAuth::Agent => {
                anyhow::bail!("SSH agent auth not yet supported");
            }
        }

        let handle = Arc::new(handle);

        let channel = handle.channel_open_session().await?;

        channel
            .request_pty(
                false,
                self.config.terminal_type.as_str(),
                size.cols as u32,
                size.rows as u32,
                0,
                0,
                &[],
            )
            .await?;

        channel.request_shell(true).await?;

        let (read_half, write_half) = channel.split();

        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (resize_tx, mut resize_rx) = mpsc::unbounded_channel::<(u16, u16, u32, u32)>();
        let (close_tx, mut close_rx) = mpsc::unbounded_channel::<()>();

        let session = Session::with_id(
            session_id.clone(),
            self.config.host.clone(),
            SessionType::Ssh,
            input_tx,
            resize_tx,
            close_tx,
        );

        // Output reader: forward data from SSH channel to event channel
        let sid_read = session_id.clone();
        let evt_read = self.event_tx.clone();
        tokio::spawn(async move {
            let mut reader = read_half;
            while let Some(msg) = reader.wait().await {
                match msg {
                    ChannelMsg::Data { data } => {
                        let _ = evt_read.send(SessionEvent::Output(sid_read.clone(), data.to_vec()));
                    }
                    ChannelMsg::ExtendedData { data, .. } => {
                        let _ = evt_read.send(SessionEvent::Output(sid_read.clone(), data.to_vec()));
                    }
                    ChannelMsg::Eof | ChannelMsg::Close => break,
                    _ => {}
                }
            }
            let _ = evt_read.send(SessionEvent::Disconnected(
                sid_read,
                "Session closed".to_string(),
            ));
        });

        // Input/resize/close writer
        let sid_write = session_id.clone();
        let evt_write = self.event_tx.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some(data) = input_rx.recv() => {
                        if write_half.data_bytes(data).await.is_err() {
                            break;
                        }
                    }
                    Some((cols, rows, pw, ph)) = resize_rx.recv() => {
                        if write_half.window_change(cols as u32, rows as u32, pw, ph).await.is_err() {
                            break;
                        }
                    }
                    Some(_) = close_rx.recv() => {
                        let _ = write_half.eof().await;
                        break;
                    }
                    else => break,
                }
            }
            let _ = evt_write.send(SessionEvent::Disconnected(
                sid_write,
                "Session closed".to_string(),
            ));
        });

        let _ = self
            .event_tx
            .send(SessionEvent::Connected(session_id.clone()));

        Ok((
            session,
            SshSession {
                handle,
                session_id,
                event_tx: self.event_tx.clone(),
            },
        ))
    }
}

type Handle = client::Handle<Handler>;

pub struct SshSession {
    handle: Arc<Handle>,
    session_id: String,
    event_tx: mpsc::UnboundedSender<SessionEvent>,
}

impl SshSession {
    pub async fn disconnect(&self) -> anyhow::Result<()> {
        self.handle
            .disconnect(russh::Disconnect::AuthCancelledByUser, "Bye", "")
            .await?;
        let _ = self.event_tx.send(SessionEvent::Disconnected(
            self.session_id.clone(),
            "User disconnected".to_string(),
        ));
        Ok(())
    }
}
