use std::io::{Read, Write};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use tokio::sync::mpsc;

use rusterm_core::config::ShellConfig;
use rusterm_core::event::SessionEvent;
use rusterm_core::session::{Session, SessionId, SessionType};
use rusterm_core::terminal::TerminalSize;

pub struct ShellConnection;

impl ShellConnection {
    pub fn open(
        config: &ShellConfig,
        size: TerminalSize,
        session_id: SessionId,
        event_tx: mpsc::UnboundedSender<SessionEvent>,
    ) -> anyhow::Result<Session> {
        let pty_system = native_pty_system();

        let pty_size = PtySize {
            rows: size.rows,
            cols: size.cols,
            pixel_width: size.pixel_width as u16,
            pixel_height: size.pixel_height as u16,
        };

        let pair = pty_system.openpty(pty_size)?;

        let mut cmd = if let Some(command) = &config.command {
            CommandBuilder::new(command)
        } else {
            CommandBuilder::new_default_prog()
        };

        if let Some(dir) = &config.working_dir {
            cmd.cwd(dir);
        }

        for (key, value) in &config.env {
            cmd.env(key, value);
        }

        let _child = pair.slave.spawn_command(cmd)?;

        let reader = pair.master.try_clone_reader()?;
        let mut writer = pair.master.take_writer()?;

        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (resize_tx, mut resize_rx) = mpsc::unbounded_channel::<(u16, u16, u32, u32)>();
        let (close_tx, mut close_rx) = mpsc::unbounded_channel::<()>();

        let session = Session::with_id(
            session_id.clone(),
            "Shell".to_string(),
            SessionType::Shell,
            input_tx,
            resize_tx,
            close_tx,
        );

        let sid_read = session_id.clone();
        let evt_read = event_tx.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            let mut reader = reader;
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let _ = evt_read.send(SessionEvent::Output(sid_read.clone(), buf[..n].to_vec()));
                    }
                    Err(_) => break,
                }
            }
            let _ = evt_read.send(SessionEvent::Disconnected(
                sid_read,
                "Shell exited".to_string(),
            ));
        });

        let sid_write = session_id.clone();
        let evt_write = event_tx.clone();
        std::thread::spawn(move || {
            loop {
                let cont = tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(async {
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
                    })
                });
                if !cont {
                    break;
                }
            }
            let _ = evt_write.send(SessionEvent::Disconnected(
                sid_write,
                "Shell closed".to_string(),
            ));
        });

        let _sid_resize = session_id.clone();
        let master = pair.master;
        std::thread::spawn(move || {
            while let Some((cols, rows, pw, ph)) = resize_rx.blocking_recv() {
                let size = PtySize {
                    rows,
                    cols,
                    pixel_width: pw as u16,
                    pixel_height: ph as u16,
                };
                if master.resize(size).is_err() {
                    break;
                }
            }
            let _ = resize_rx.close();
        });

        let _ = event_tx.send(SessionEvent::Connected(session_id));

        Ok(session)
    }
}
