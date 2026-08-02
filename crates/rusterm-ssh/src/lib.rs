pub mod channel;
pub mod client;
pub mod direct;
pub mod known_hosts;
pub mod sftp;
pub mod ssh_config;
pub mod transport;

pub use client::{SshClient, SshSession, connect_authenticated, parse_remote_history};
pub use direct::{DirectConnectOptions, DirectHandle, ExecResult, connect_direct};
pub use sftp::{
    RemoteDirEntry, RemoteFileMetadata, RemoteFileType, SftpClient, SftpError, TransferResult,
};
pub use ssh_config::{
    ResolvedHost, SshHostSuggestion, default_ssh_config_path, default_ssh_dir, expand_tilde,
    is_identity_file, is_wildcard_pattern, list_identity_files, list_identity_files_at,
    list_ssh_config_hosts, list_ssh_config_hosts_at, lookup_host, parse_ssh_config_text,
    resolved_host_to_auth,
};
