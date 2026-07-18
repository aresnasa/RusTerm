//! Integration tests for `ShellConnection`.
//!
//! These spawn a real PTY via `portable-pty`, so they touch the OS process
//! table and terminal layer. They are kept fast and deterministic by using
//! `printf` (a builtin / small binary available on macOS and Linux) instead
//! of a full interactive shell — that isolates the test from the user's
//! shell config, startup files, and prompt rendering.

use std::time::Duration;

use rusterm_core::config::ShellConfig;
use rusterm_core::event::SessionEvent;
use rusterm_core::terminal::TerminalSize;
use rusterm_proto::ShellConnection;
use tokio::sync::mpsc;

/// Helper: open a shell connection running `cmd` with `args`, collect output
/// bytes received via the session event channel until `deadline` elapses or
/// the session disconnects. Returns the concatenated output.
fn run_and_collect(cmd: &str, args: Vec<String>) -> Vec<u8> {
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<SessionEvent>();
    let config = ShellConfig {
        command: Some(cmd.to_string()),
        args,
        env: Vec::new(),
        working_dir: None,
    };
    let session_id = "test-session-1".to_string();
    let session = ShellConnection::open(
        &config,
        TerminalSize::default(),
        session_id.clone(),
        event_tx,
    )
    .expect("open shell connection");

    let mut out = Vec::new();
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(3) {
        match event_rx.try_recv() {
            Ok(SessionEvent::Output(_, data)) => out.extend_from_slice(&data),
            Ok(SessionEvent::Disconnected(_, _)) => break,
            Ok(_) => {}
            Err(mpsc::error::TryRecvError::Empty) => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(mpsc::error::TryRecvError::Disconnected) => break,
        }
    }
    // Best-effort close.
    let _ = session.close();
    out
}

/// Verify that `ShellConfig.args` is actually forwarded to the spawned
/// command. Without the args-forwarding fix in `shell.rs`, this test fails
/// because `printf` would be invoked with no format string and produce no
/// output (or a usage error on stderr).
#[test]
fn shell_args_are_forwarded_to_command() {
    // `printf` writes its first arg (format string) verbatim when it contains
    // no format specifiers. We pass a unique sentinel so we can detect it
    // even if the PTY layer adds CRs.
    let sentinel = "rusterm-args-test-sentinel";
    let out = run_and_collect("printf", vec![sentinel.to_string()]);
    let out_str = String::from_utf8_lossy(&out);
    assert!(
        out_str.contains(sentinel),
        "expected sentinel {:?} in output, got: {:?}",
        sentinel,
        out_str
    );
}

/// Verify that env vars from `ShellConfig.env` reach the spawned process.
/// Uses `printenv` (available on macOS and Linux) which prints the value of
/// the named env var to stdout.
#[test]
fn shell_env_is_forwarded_to_command() {
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<SessionEvent>();
    let config = ShellConfig {
        command: Some("printenv".to_string()),
        args: vec!["RUSTERM_TEST_ENV".to_string()],
        env: vec![("RUSTERM_TEST_ENV".to_string(), "env-value-123".to_string())],
        working_dir: None,
    };
    let session = ShellConnection::open(
        &config,
        TerminalSize::default(),
        "test-env-session".to_string(),
        event_tx,
    )
    .expect("open shell connection");

    let mut out = Vec::new();
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(3) {
        match event_rx.try_recv() {
            Ok(SessionEvent::Output(_, data)) => out.extend_from_slice(&data),
            Ok(SessionEvent::Disconnected(_, _)) => break,
            Ok(_) => {}
            Err(mpsc::error::TryRecvError::Empty) => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(mpsc::error::TryRecvError::Disconnected) => break,
        }
    }
    let _ = session.close();
    let out_str = String::from_utf8_lossy(&out);
    assert!(
        out_str.contains("env-value-123"),
        "expected env var value in output, got: {:?}",
        out_str
    );
}

/// Verify that `working_dir` is honored. Runs `pwd` in a uniquely-named
/// subdirectory of the temp dir and checks the unique name appears in the
/// output. Using a fresh subdir avoids the macOS `/var` -> `/private/var`
/// symlink-resolution mismatch that would otherwise make the assertion
/// fragile.
#[test]
fn shell_working_dir_is_honored() {
    let unique = format!("rusterm-cwd-test-{}", std::process::id());
    let tmp = std::env::temp_dir().join(&unique);
    std::fs::create_dir_all(&tmp).expect("create temp subdir");
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<SessionEvent>();
    let config = ShellConfig {
        command: Some("pwd".to_string()),
        args: Vec::new(),
        env: Vec::new(),
        working_dir: Some(tmp.to_string_lossy().to_string()),
    };
    let session = ShellConnection::open(
        &config,
        TerminalSize::default(),
        "test-cwd-session".to_string(),
        event_tx,
    )
    .expect("open shell connection");

    let mut out = Vec::new();
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(3) {
        match event_rx.try_recv() {
            Ok(SessionEvent::Output(_, data)) => out.extend_from_slice(&data),
            Ok(SessionEvent::Disconnected(_, _)) => break,
            Ok(_) => {}
            Err(mpsc::error::TryRecvError::Empty) => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(mpsc::error::TryRecvError::Disconnected) => break,
        }
    }
    let _ = session.close();
    let out_str = String::from_utf8_lossy(&out);
    assert!(
        out_str.contains(&unique),
        "expected unique subdir {:?} in pwd output, got: {:?}",
        unique,
        out_str
    );
    let _ = std::fs::remove_dir_all(&tmp);
}
