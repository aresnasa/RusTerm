use rusterm_core::config::{SshAuth, SshConfig};
use rusterm_core::terminal::TerminalSize;
use rusterm_ssh::SshClient;
use tokio::sync::mpsc;
use uuid::Uuid;

/// Live smoke test for the complete SFTP lifecycle.
///
/// Required environment variables:
/// `RUSTERM_SFTP_HOST`, `RUSTERM_SFTP_USER`, and `RUSTERM_SFTP_PASSWORD`.
/// Optional: `RUSTERM_SFTP_PORT` and `RUSTERM_SFTP_TEST_DIR`.
#[tokio::test]
#[ignore = "requires a disposable live SSH/SFTP server"]
async fn live_sftp_round_trip() {
    let host = std::env::var("RUSTERM_SFTP_HOST").expect("RUSTERM_SFTP_HOST");
    let username = std::env::var("RUSTERM_SFTP_USER").expect("RUSTERM_SFTP_USER");
    let password = std::env::var("RUSTERM_SFTP_PASSWORD").expect("RUSTERM_SFTP_PASSWORD");
    let port = std::env::var("RUSTERM_SFTP_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(22);
    let remote_parent =
        std::env::var("RUSTERM_SFTP_TEST_DIR").unwrap_or_else(|_| "/tmp".to_owned());
    let test_id = Uuid::new_v4();
    let remote_dir = format!("{remote_parent}/rusterm-sftp-{test_id}");
    let remote_file = format!("{remote_dir}/payload.txt");
    let remote_renamed = format!("{remote_dir}/renamed.txt");

    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let ssh = SshClient::new(
        SshConfig {
            host,
            port,
            username,
            auth: SshAuth::Password { password },
            terminal_type: "xterm-256color".to_owned(),
            proxy_jump: None,
            keepalive_interval: None,
            host_key_policy: "disabled".to_owned(),
        },
        event_tx,
    );
    let (_terminal, session) = ssh
        .connect(
            format!("sftp-live-{test_id}"),
            TerminalSize {
                cols: 80,
                rows: 24,
                pixel_width: 0,
                pixel_height: 0,
            },
        )
        .await
        .expect("connect and authenticate");
    let sftp = session.open_sftp().await.expect("open SFTP subsystem");

    let local_dir = tempfile::tempdir().expect("local temporary directory");
    let upload_path = local_dir.path().join("upload.txt");
    let download_path = local_dir.path().join("download.txt");
    tokio::fs::write(&upload_path, b"RusTerm SFTP live test")
        .await
        .expect("write upload fixture");

    sftp.mkdir(remote_dir.clone()).await.expect("mkdir");
    let uploaded = sftp
        .upload(&upload_path, remote_file.clone())
        .await
        .expect("upload");
    assert_eq!(uploaded.bytes_transferred, 22);
    assert_eq!(
        sftp.stat(remote_file.clone(), true)
            .await
            .expect("stat")
            .size,
        Some(22)
    );
    assert_eq!(sftp.list(remote_dir.clone()).await.expect("list").len(), 1);
    sftp.rename(remote_file, remote_renamed.clone())
        .await
        .expect("rename");
    let downloaded = sftp
        .download(remote_renamed.clone(), &download_path)
        .await
        .expect("download");
    assert_eq!(downloaded.bytes_transferred, 22);
    assert_eq!(
        tokio::fs::read(&download_path)
            .await
            .expect("read downloaded file"),
        b"RusTerm SFTP live test"
    );

    sftp.remove_file(remote_renamed).await.expect("remove file");
    sftp.remove_empty_dir(remote_dir)
        .await
        .expect("remove directory");
    sftp.close().await.expect("close SFTP");
    session.disconnect().await.expect("disconnect SSH");
}
