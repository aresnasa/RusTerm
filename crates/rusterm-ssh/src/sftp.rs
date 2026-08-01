use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use russh_sftp::client::SftpSession;
use russh_sftp::client::error::Error as RusshSftpError;
use russh_sftp::protocol::{FileAttributes, FileType, StatusCode};
use thiserror::Error;
use tokio::fs::OpenOptions;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const TRANSFER_BUFFER_SIZE: usize = 64 * 1024;

/// High-level SFTP client whose public API is independent of the wire library.
#[derive(Clone)]
pub struct SftpClient {
    session: Arc<SftpSession>,
}

impl fmt::Debug for SftpClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("SftpClient").finish_non_exhaustive()
    }
}

/// Error returned by SFTP operations.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SftpError {
    #[error("SFTP connection failed: {0}")]
    Connection(String),
    #[error("path not found: {0}")]
    NotFound(String),
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    #[error("operation is not supported: {0}")]
    Unsupported(String),
    #[error("the SSH server rejected the SFTP subsystem request")]
    SubsystemRejected,
    #[error("unexpected SFTP subsystem reply: {0}")]
    UnexpectedSubsystemReply(String),
    #[error("the SSH channel closed before SFTP started")]
    ChannelClosed,
    #[error("SFTP connection lost: {0}")]
    ConnectionLost(String),
    #[error("SFTP request timed out")]
    Timeout,
    #[error("SFTP transfer cancelled")]
    Cancelled,
    #[error("I/O error: {0}")]
    Io(String),
    #[error("SFTP protocol error: {0}")]
    Protocol(String),
    #[error("invalid path: {0}")]
    InvalidPath(String),
}

/// Simplified type of a remote filesystem entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteFileType {
    File,
    Directory,
    Symlink,
    Other,
}

/// Metadata for a remote filesystem entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteFileMetadata {
    pub file_type: RemoteFileType,
    pub size: Option<u64>,
    pub uid: Option<u32>,
    pub user: Option<String>,
    pub gid: Option<u32>,
    pub group: Option<String>,
    pub permissions: Option<u32>,
    pub accessed: Option<u64>,
    pub modified: Option<u64>,
}

/// One entry returned by [`SftpClient::list`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteDirEntry {
    pub name: String,
    pub path: String,
    pub metadata: RemoteFileMetadata,
}

/// Result of a completed upload or download.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransferResult {
    pub bytes_transferred: u64,
}

impl SftpClient {
    pub(crate) fn new(session: SftpSession) -> Self {
        Self {
            session: Arc::new(session),
        }
    }

    /// Close the SFTP subsystem channel.
    pub async fn close(&self) -> Result<(), SftpError> {
        self.session.close().await.map_err(map_sftp_error)
    }

    /// List the direct children of a remote directory.
    pub async fn list(&self, path: impl Into<String>) -> Result<Vec<RemoteDirEntry>, SftpError> {
        let path = path.into();
        let entries = self
            .session
            .read_dir(path.clone())
            .await
            .map_err(map_sftp_error)?;

        Ok(entries
            .map(|entry| {
                let name = entry.file_name().to_owned();
                RemoteDirEntry {
                    path: join_remote_path(&path, &name),
                    name: name.to_string(),
                    metadata: metadata_from_attributes(entry.metadata()),
                }
            })
            .collect())
    }

    /// Read metadata for a remote path.
    ///
    /// When `follow_symlinks` is `false`, metadata describes the link itself.
    pub async fn stat(
        &self,
        path: impl Into<String>,
        follow_symlinks: bool,
    ) -> Result<RemoteFileMetadata, SftpError> {
        let path = path.into();
        let metadata = if follow_symlinks {
            self.session.metadata(path).await
        } else {
            self.session.symlink_metadata(path).await
        }
        .map_err(map_sftp_error)?;

        Ok(metadata_from_attributes(metadata))
    }

    /// Create one remote directory. Parent directories must already exist.
    pub async fn mkdir(&self, path: impl Into<String>) -> Result<(), SftpError> {
        self.session.create_dir(path).await.map_err(map_sftp_error)
    }

    /// Rename a remote file or directory.
    pub async fn rename(
        &self,
        old_path: impl Into<String>,
        new_path: impl Into<String>,
    ) -> Result<(), SftpError> {
        self.session
            .rename(old_path, new_path)
            .await
            .map_err(map_sftp_error)
    }

    /// Remove a remote file or symbolic link.
    pub async fn remove_file(&self, path: impl Into<String>) -> Result<(), SftpError> {
        self.session.remove_file(path).await.map_err(map_sftp_error)
    }

    /// Remove a remote directory. The server rejects non-empty directories.
    pub async fn remove_empty_dir(&self, path: impl Into<String>) -> Result<(), SftpError> {
        self.session.remove_dir(path).await.map_err(map_sftp_error)
    }

    /// Alias for [`SftpClient::remove_empty_dir`].
    pub async fn remove_dir(&self, path: impl Into<String>) -> Result<(), SftpError> {
        self.remove_empty_dir(path).await
    }

    /// Stream a local file to a remote path.
    ///
    /// The remote handle is explicitly shut down before success is returned.
    /// `russh-sftp` drains every outstanding write acknowledgement during that
    /// shutdown, so a successful result means the server acknowledged all data.
    pub async fn upload(
        &self,
        local_path: impl AsRef<Path>,
        remote_path: impl Into<String>,
    ) -> Result<TransferResult, SftpError> {
        self.upload_with_progress(local_path, remote_path, CancellationToken::new(), |_| {})
            .await
    }

    /// Stream a local file to a remote path with cumulative progress and cancellation.
    ///
    /// Progress is reported after each complete chunk is accepted by the remote
    /// handle. On cancellation or failure, the handle is shut down to drain write
    /// acknowledgements and the incomplete remote file is removed when possible.
    pub async fn upload_with_progress<F>(
        &self,
        local_path: impl AsRef<Path>,
        remote_path: impl Into<String>,
        cancellation: CancellationToken,
        mut on_progress: F,
    ) -> Result<TransferResult, SftpError>
    where
        F: FnMut(u64),
    {
        ensure_not_cancelled(cancellation.is_cancelled())?;

        let local_path = local_path.as_ref();
        let remote_path = remote_path.into();
        let mut local = tokio::fs::File::open(local_path)
            .await
            .map_err(|error| map_io_error(error, local_path))?;

        ensure_not_cancelled(cancellation.is_cancelled())?;

        let mut remote = self
            .session
            .create(remote_path.clone())
            .await
            .map_err(map_sftp_error)?;
        let transfer_result =
            copy_chunks_with_progress(&mut local, &mut remote, &cancellation, &mut on_progress)
                .await;
        let shutdown_result = remote
            .shutdown()
            .await
            .map_err(|error| SftpError::Io(error.to_string()));

        let result = match (transfer_result, shutdown_result) {
            (Err(error), _) | (Ok(_), Err(error)) => Err(error),
            (Ok(bytes_transferred), Ok(())) => ensure_not_cancelled(cancellation.is_cancelled())
                .map(|()| TransferResult { bytes_transferred }),
        };

        if result.is_err() {
            let _ = self.session.remove_file(remote_path).await;
        }

        result
    }

    /// Stream a remote file into a local destination.
    ///
    /// Data is written to a uniquely named temporary file in the destination's
    /// directory. Only a complete, flushed download is renamed into place.
    pub async fn download(
        &self,
        remote_path: impl Into<String>,
        local_path: impl AsRef<Path>,
    ) -> Result<TransferResult, SftpError> {
        self.download_with_progress(remote_path, local_path, CancellationToken::new(), |_| {})
            .await
    }

    /// Stream a remote file into a local destination with progress and cancellation.
    ///
    /// Progress is cumulative and reported after each chunk reaches the local
    /// temporary file. Cancellation and all other failures remove that `.part`
    /// file; only a complete, synced download is renamed into place.
    pub async fn download_with_progress<F>(
        &self,
        remote_path: impl Into<String>,
        local_path: impl AsRef<Path>,
        cancellation: CancellationToken,
        mut on_progress: F,
    ) -> Result<TransferResult, SftpError>
    where
        F: FnMut(u64),
    {
        ensure_not_cancelled(cancellation.is_cancelled())?;

        let remote_path = remote_path.into();
        let local_path = local_path.as_ref();
        let temporary_path = download_temp_path(local_path)?;

        let result = self
            .download_to_temporary(
                &remote_path,
                local_path,
                &temporary_path,
                &cancellation,
                &mut on_progress,
            )
            .await;

        if result.is_err() {
            let _ = tokio::fs::remove_file(&temporary_path).await;
        }

        result
    }

    async fn download_to_temporary<F>(
        &self,
        remote_path: &str,
        local_path: &Path,
        temporary_path: &Path,
        cancellation: &CancellationToken,
        on_progress: &mut F,
    ) -> Result<TransferResult, SftpError>
    where
        F: FnMut(u64),
    {
        let mut remote = self
            .session
            .open(remote_path.to_owned())
            .await
            .map_err(map_sftp_error)?;

        if let Err(error) = ensure_not_cancelled(cancellation.is_cancelled()) {
            let _ = remote.shutdown().await;
            return Err(error);
        }

        let mut local = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(temporary_path)
            .await
        {
            Ok(local) => local,
            Err(error) => {
                let error = map_io_error(error, temporary_path);
                let _ = remote.shutdown().await;
                return Err(error);
            }
        };

        let transfer_result =
            copy_chunks_with_progress(&mut remote, &mut local, cancellation, on_progress).await;
        let remote_shutdown_result = remote
            .shutdown()
            .await
            .map_err(|error| SftpError::Io(error.to_string()));

        let bytes_transferred = match transfer_result {
            Ok(bytes_transferred) => bytes_transferred,
            Err(error) => return Err(error),
        };
        remote_shutdown_result?;
        ensure_not_cancelled(cancellation.is_cancelled())?;

        local
            .flush()
            .await
            .map_err(|error| map_io_error(error, temporary_path))?;
        ensure_not_cancelled(cancellation.is_cancelled())?;
        local
            .sync_all()
            .await
            .map_err(|error| map_io_error(error, temporary_path))?;
        ensure_not_cancelled(cancellation.is_cancelled())?;
        drop(local);

        tokio::fs::rename(temporary_path, local_path)
            .await
            .map_err(|error| map_io_error(error, local_path))?;

        Ok(TransferResult { bytes_transferred })
    }
}

async fn copy_chunks_with_progress<R, W, F>(
    reader: &mut R,
    writer: &mut W,
    cancellation: &CancellationToken,
    on_progress: &mut F,
) -> Result<u64, SftpError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    F: FnMut(u64),
{
    let mut buffer = vec![0; TRANSFER_BUFFER_SIZE];
    let mut bytes_transferred = 0;

    loop {
        ensure_not_cancelled(cancellation.is_cancelled())?;
        let bytes_read = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(SftpError::Cancelled),
            result = reader.read(&mut buffer) => {
                result.map_err(|error| SftpError::Io(error.to_string()))?
            }
        };

        if bytes_read == 0 {
            ensure_not_cancelled(cancellation.is_cancelled())?;
            return Ok(bytes_transferred);
        }

        tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(SftpError::Cancelled),
            result = writer.write_all(&buffer[..bytes_read]) => {
                result.map_err(|error| SftpError::Io(error.to_string()))?;
            }
        }
        report_chunk_progress(&mut bytes_transferred, bytes_read, on_progress);
    }
}

fn report_chunk_progress<F>(bytes_transferred: &mut u64, chunk_size: usize, on_progress: &mut F)
where
    F: FnMut(u64),
{
    if chunk_size == 0 {
        return;
    }

    *bytes_transferred += chunk_size as u64;
    on_progress(*bytes_transferred);
}

fn ensure_not_cancelled(cancelled: bool) -> Result<(), SftpError> {
    if cancelled {
        Err(SftpError::Cancelled)
    } else {
        Ok(())
    }
}

fn metadata_from_attributes(attributes: FileAttributes) -> RemoteFileMetadata {
    RemoteFileMetadata {
        file_type: remote_file_type(attributes.file_type()),
        size: attributes.size,
        uid: attributes.uid,
        user: attributes.user,
        gid: attributes.gid,
        group: attributes.group,
        permissions: attributes.permissions,
        accessed: attributes.atime.map(u64::from),
        modified: attributes.mtime.map(u64::from),
    }
}

fn remote_file_type(file_type: FileType) -> RemoteFileType {
    match file_type {
        FileType::File => RemoteFileType::File,
        FileType::Dir => RemoteFileType::Directory,
        FileType::Symlink => RemoteFileType::Symlink,
        FileType::Other => RemoteFileType::Other,
    }
}

fn join_remote_path(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_owned()
    } else if parent.ends_with('/') {
        format!("{parent}{name}")
    } else {
        format!("{parent}/{name}")
    }
}

fn download_temp_path(destination: &Path) -> Result<PathBuf, SftpError> {
    let file_name = destination.file_name().ok_or_else(|| {
        SftpError::InvalidPath(format!(
            "download destination has no file name: {}",
            destination.display()
        ))
    })?;
    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    let mut temporary_name = OsString::from(".");
    temporary_name.push(file_name);
    temporary_name.push(format!(".rusterm-{}.part", Uuid::new_v4()));
    Ok(parent.join(temporary_name))
}

fn map_io_error(error: std::io::Error, path: &Path) -> SftpError {
    let path = path.display().to_string();
    match error.kind() {
        std::io::ErrorKind::NotFound => SftpError::NotFound(path),
        std::io::ErrorKind::PermissionDenied => SftpError::PermissionDenied(path),
        _ => SftpError::Io(format!("{path}: {error}")),
    }
}

pub(crate) fn map_sftp_error(error: RusshSftpError) -> SftpError {
    match error {
        RusshSftpError::Status(status) => {
            let message = if status.error_message.is_empty() {
                status.status_code.to_string()
            } else {
                status.error_message
            };
            match status.status_code {
                StatusCode::NoSuchFile => SftpError::NotFound(message),
                StatusCode::PermissionDenied => SftpError::PermissionDenied(message),
                StatusCode::OpUnsupported => SftpError::Unsupported(message),
                StatusCode::NoConnection => SftpError::Connection(message),
                StatusCode::ConnectionLost => SftpError::ConnectionLost(message),
                StatusCode::Ok | StatusCode::Eof | StatusCode::Failure | StatusCode::BadMessage => {
                    SftpError::Protocol(message)
                }
            }
        }
        RusshSftpError::Timeout => SftpError::Timeout,
        RusshSftpError::IO(message) => SftpError::Io(message),
        RusshSftpError::Limited(message) => SftpError::Protocol(message),
        RusshSftpError::UnexpectedPacket => {
            SftpError::Protocol("unexpected response packet".to_owned())
        }
        RusshSftpError::UnexpectedBehavior(message) => SftpError::Protocol(message),
    }
}

pub(crate) fn map_ssh_error(error: russh::Error) -> SftpError {
    SftpError::Connection(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use russh_sftp::protocol::Status;

    #[test]
    fn converts_remote_metadata_without_exposing_protocol_types() {
        let attributes = FileAttributes {
            size: Some(42),
            uid: Some(1000),
            user: Some("alice".to_owned()),
            gid: Some(100),
            group: Some("staff".to_owned()),
            permissions: Some(0o100640),
            atime: Some(10),
            mtime: Some(20),
        };

        let metadata = metadata_from_attributes(attributes);

        assert_eq!(metadata.file_type, RemoteFileType::File);
        assert_eq!(metadata.size, Some(42));
        assert_eq!(metadata.uid, Some(1000));
        assert_eq!(metadata.user.as_deref(), Some("alice"));
        assert_eq!(metadata.gid, Some(100));
        assert_eq!(metadata.group.as_deref(), Some("staff"));
        assert_eq!(metadata.permissions, Some(0o100640));
        assert_eq!(metadata.accessed, Some(10));
        assert_eq!(metadata.modified, Some(20));
    }

    #[test]
    fn converts_all_remote_file_types() {
        assert_eq!(remote_file_type(FileType::File), RemoteFileType::File);
        assert_eq!(remote_file_type(FileType::Dir), RemoteFileType::Directory);
        assert_eq!(remote_file_type(FileType::Symlink), RemoteFileType::Symlink);
        assert_eq!(remote_file_type(FileType::Other), RemoteFileType::Other);
    }

    #[test]
    fn joins_remote_paths_with_posix_separators() {
        assert_eq!(join_remote_path("", "file.txt"), "file.txt");
        assert_eq!(join_remote_path("/", "file.txt"), "/file.txt");
        assert_eq!(join_remote_path("/tmp", "file.txt"), "/tmp/file.txt");
        assert_eq!(join_remote_path("tmp/", "file.txt"), "tmp/file.txt");
    }

    #[test]
    fn download_temp_file_is_hidden_and_in_destination_directory() {
        let destination = Path::new("/tmp/downloads/report.txt");
        let temporary = download_temp_path(destination).expect("temporary path");

        assert_eq!(temporary.parent(), destination.parent());
        let name = temporary
            .file_name()
            .expect("temporary file name")
            .to_string_lossy();
        assert!(name.starts_with(".report.txt.rusterm-"));
        assert!(name.ends_with(".part"));
    }

    #[test]
    fn rejects_download_destination_without_file_name() {
        let error = download_temp_path(Path::new("/")).expect_err("invalid destination");
        assert!(matches!(error, SftpError::InvalidPath(_)));
    }

    #[test]
    fn reports_monotonic_cumulative_progress_after_each_nonempty_chunk() {
        let mut bytes_transferred = 0;
        let mut reports = Vec::new();
        let mut on_progress = |total| reports.push(total);

        report_chunk_progress(&mut bytes_transferred, 3, &mut on_progress);
        report_chunk_progress(&mut bytes_transferred, 5, &mut on_progress);
        report_chunk_progress(&mut bytes_transferred, 2, &mut on_progress);

        assert_eq!(bytes_transferred, 10);
        assert_eq!(reports, vec![3, 8, 10]);
        assert!(reports.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn empty_chunks_do_not_report_progress() {
        let mut bytes_transferred = 7;
        let mut reports = Vec::new();

        report_chunk_progress(&mut bytes_transferred, 0, &mut |total| reports.push(total));

        assert_eq!(bytes_transferred, 7);
        assert!(reports.is_empty());
    }

    #[test]
    fn cancellation_maps_to_explicit_domain_error() {
        assert_eq!(ensure_not_cancelled(false), Ok(()));
        assert_eq!(ensure_not_cancelled(true), Err(SftpError::Cancelled));
    }

    #[test]
    fn maps_sftp_status_errors_to_domain_errors() {
        let not_found = RusshSftpError::Status(Status {
            id: 1,
            status_code: StatusCode::NoSuchFile,
            error_message: "missing.txt".to_owned(),
            language_tag: String::new(),
        });
        let denied = RusshSftpError::Status(Status {
            id: 2,
            status_code: StatusCode::PermissionDenied,
            error_message: "private".to_owned(),
            language_tag: String::new(),
        });

        assert_eq!(
            map_sftp_error(not_found),
            SftpError::NotFound("missing.txt".to_owned())
        );
        assert_eq!(
            map_sftp_error(denied),
            SftpError::PermissionDenied("private".to_owned())
        );
        assert_eq!(map_sftp_error(RusshSftpError::Timeout), SftpError::Timeout);
    }

    #[test]
    fn maps_local_io_errors_to_domain_errors() {
        let path = Path::new("/tmp/missing");
        let error = std::io::Error::new(std::io::ErrorKind::NotFound, "gone");

        assert_eq!(
            map_io_error(error, path),
            SftpError::NotFound("/tmp/missing".to_owned())
        );
    }
}
