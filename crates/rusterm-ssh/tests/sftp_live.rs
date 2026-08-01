use rusterm_core::config::{SshAuth, SshConfig};
use rusterm_core::terminal::TerminalSize;
use rusterm_ssh::{RemoteFileType, SftpError, SshClient};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Live validation for browsing, mutation, progress, cancellation cleanup,
/// symlink deletion, and non-recursive directory removal.
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
    let remote_file = format!("{remote_dir}/payload.bin");
    let remote_renamed = format!("{remote_dir}/renamed.bin");
    let remote_nonempty_dir = format!("{remote_dir}/nonempty");
    let remote_child = format!("{remote_nonempty_dir}/child.txt");
    let remote_cancelled_upload = format!("{remote_dir}/cancelled-upload.bin");

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
    let upload_path = local_dir.path().join("upload.bin");
    let download_path = local_dir.path().join("download.bin");
    let cancelled_download_path = local_dir.path().join("cancelled-download.bin");
    let payload = (0..(3 * 64 * 1024 + 17))
        .map(|index| (index % 251) as u8)
        .collect::<Vec<_>>();
    tokio::fs::write(&upload_path, &payload)
        .await
        .expect("write upload fixture");

    sftp.mkdir(remote_dir.clone()).await.expect("mkdir");
    let mut upload_progress = Vec::new();
    let uploaded = sftp
        .upload_with_progress(
            &upload_path,
            remote_file.clone(),
            CancellationToken::new(),
            |bytes| upload_progress.push(bytes),
        )
        .await
        .expect("upload");
    assert_eq!(uploaded.bytes_transferred, payload.len() as u64);
    assert_eq!(upload_progress.last().copied(), Some(payload.len() as u64));
    assert!(upload_progress.windows(2).all(|pair| pair[0] <= pair[1]));
    assert_eq!(
        sftp.stat(remote_file.clone(), true)
            .await
            .expect("stat")
            .size,
        Some(payload.len() as u64)
    );

    let entries = sftp.list(remote_dir.clone()).await.expect("list");
    let uploaded_entry = entries
        .iter()
        .find(|entry| entry.name == "payload.bin")
        .expect("uploaded file appears in listing");
    assert_eq!(uploaded_entry.path, remote_file);
    assert_eq!(uploaded_entry.metadata.file_type, RemoteFileType::File);

    sftp.rename(remote_file, remote_renamed.clone())
        .await
        .expect("rename");
    assert!(matches!(
        sftp.stat(format!("{remote_dir}/payload.bin"), false).await,
        Err(SftpError::NotFound(_))
    ));

    sftp.mkdir(remote_nonempty_dir.clone())
        .await
        .expect("mkdir non-empty fixture");
    sftp.upload(&upload_path, remote_child.clone())
        .await
        .expect("upload child fixture");
    assert!(
        sftp.remove_empty_dir(remote_nonempty_dir.clone())
            .await
            .is_err()
    );
    assert_eq!(
        sftp.stat(remote_child.clone(), false)
            .await
            .expect("non-empty directory child remains")
            .file_type,
        RemoteFileType::File
    );

    if let (Ok(link_path), Ok(target_path)) = (
        std::env::var("RUSTERM_SFTP_SYMLINK_PATH"),
        std::env::var("RUSTERM_SFTP_SYMLINK_TARGET"),
    ) {
        assert_eq!(
            sftp.stat(link_path.clone(), false)
                .await
                .expect("lstat symlink")
                .file_type,
            RemoteFileType::Symlink
        );
        assert_eq!(
            sftp.stat(link_path.clone(), true)
                .await
                .expect("stat symlink target")
                .file_type,
            RemoteFileType::File
        );
        sftp.remove_file(link_path.clone())
            .await
            .expect("delete symlink without following it");
        assert!(matches!(
            sftp.stat(link_path, false).await,
            Err(SftpError::NotFound(_))
        ));
        assert_eq!(
            sftp.stat(target_path, true)
                .await
                .expect("symlink target survives deletion")
                .file_type,
            RemoteFileType::File
        );
    }

    let upload_cancellation = CancellationToken::new();
    let cancel_after_first_chunk = upload_cancellation.clone();
    let cancelled_upload = sftp
        .upload_with_progress(
            &upload_path,
            remote_cancelled_upload.clone(),
            upload_cancellation,
            move |_| cancel_after_first_chunk.cancel(),
        )
        .await;
    assert_eq!(cancelled_upload, Err(SftpError::Cancelled));
    assert!(matches!(
        sftp.stat(remote_cancelled_upload, false).await,
        Err(SftpError::NotFound(_))
    ));

    tokio::fs::write(&cancelled_download_path, b"existing destination")
        .await
        .expect("write existing destination sentinel");
    let download_cancellation = CancellationToken::new();
    let cancel_download_after_first_chunk = download_cancellation.clone();
    let cancelled_download = sftp
        .download_with_progress(
            remote_renamed.clone(),
            &cancelled_download_path,
            download_cancellation,
            move |_| cancel_download_after_first_chunk.cancel(),
        )
        .await;
    assert_eq!(cancelled_download, Err(SftpError::Cancelled));
    assert_eq!(
        tokio::fs::read(&cancelled_download_path)
            .await
            .expect("existing destination survives cancellation"),
        b"existing destination"
    );
    let mut local_entries = tokio::fs::read_dir(local_dir.path())
        .await
        .expect("list local temporary directory");
    while let Some(entry) = local_entries.next_entry().await.expect("read local entry") {
        let name = entry.file_name().to_string_lossy().into_owned();
        assert!(
            !name.ends_with(".part"),
            "cancelled download left temporary file: {name}"
        );
    }

    let mut download_progress = Vec::new();
    let downloaded = sftp
        .download_with_progress(
            remote_renamed.clone(),
            &download_path,
            CancellationToken::new(),
            |bytes| download_progress.push(bytes),
        )
        .await
        .expect("download");
    assert_eq!(downloaded.bytes_transferred, payload.len() as u64);
    assert_eq!(
        download_progress.last().copied(),
        Some(payload.len() as u64)
    );
    assert!(download_progress.windows(2).all(|pair| pair[0] <= pair[1]));
    assert_eq!(
        tokio::fs::read(&download_path)
            .await
            .expect("read downloaded file"),
        payload
    );

    sftp.remove_file(remote_child)
        .await
        .expect("remove child file");
    sftp.remove_empty_dir(remote_nonempty_dir)
        .await
        .expect("remove now-empty directory");
    sftp.remove_file(remote_renamed).await.expect("remove file");
    sftp.remove_empty_dir(remote_dir)
        .await
        .expect("remove directory");
    sftp.close().await.expect("close SFTP");
    session.disconnect().await.expect("disconnect SSH");
}
