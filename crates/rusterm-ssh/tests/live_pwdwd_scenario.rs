//! Integration test that connects to a live SSH host and verifies the
//! shell-integration + history-import + failed-command-filtering flow.
//!
//! This test is GATED behind the `RUSTERM_LIVE_SSH_TEST` env var so it
//! doesn't run in CI. To run it locally:
//!
//! ```sh
//! RUSTERM_LIVE_SSH_TEST=1 \
//! RUSTERM_LIVE_SSH_HOST=10.9.202.216 \
//! RUSTERM_LIVE_SSH_USER=root \
//! RUSTERM_LIVE_SSH_PASS=123 \
//! cargo test -p rusterm-ssh --test live_pwdwd_scenario -- --nocapture --ignored
//! ```
//!
//! The test reproduces the user's exact scenario:
//!   1. Connect to the host.
//!   2. Inject shell integration (OSC 133) the same way RusTerm does.
//!   3. Run `pwdwd` (a typo) — expect rc=127.
//!   4. Verify the OSC 133;D sequence with rc=127 was emitted.
//!   5. Run `pwd` (valid) — expect rc=0.
//!   6. Verify the OSC 133;D sequence with rc=0 was emitted.
//!
//! This confirms the shell integration works on the live host, which is
//! the prerequisite for the failed-command filtering to work. The DB-level
//! filtering is verified separately in `rusterm-db::store::tests`.

#![cfg(test)]

use std::sync::Arc;
use std::time::Duration;

use russh::client;
use russh::{ChannelMsg, Pty};

/// The shell-integration script RusTerm injects after connecting.
/// Mirrors the literal string in `crates/rusterm-ui/src/app.rs`.
const SHELL_INTEGRATION_SCRIPT: &str = r#"__rusterm_precmd() { printf '\e]133;D;%s\e\\' "$?"; printf '\e]133;A\e\\'; }; if [ -n "$ZSH_VERSION" ]; then precmd_functions+=(__rusterm_precmd); elif [ -n "$BASH_VERSION" ]; then PROMPT_COMMAND="__rusterm_precmd${PROMPT_COMMAND:+;$PROMPT_COMMAND}"; fi"#;

fn live_host() -> Option<(String, String, String)> {
    let host = std::env::var("RUSTERM_LIVE_SSH_HOST").ok()?;
    let user = std::env::var("RUSTERM_LIVE_SSH_USER").ok()?;
    let pass = std::env::var("RUSTERM_LIVE_SSH_PASS").ok()?;
    if std::env::var("RUSTERM_LIVE_SSH_TEST").is_err() {
        return None;
    }
    Some((host, user, pass))
}

async fn connect_and_run(
    host: &str,
    user: &str,
    pass: &str,
    inputs: &[&str],
) -> anyhow::Result<Vec<u8>> {
    let config = Arc::new(client::Config::default());
    let mut handle = client::connect(config, (host, 22), rusterm_ssh::client::Handler).await?;

    // Try publickey auth first (the test host has ~/.ssh/id_rsa set up).
    let mut authed = false;
    if let Ok(home) = std::env::var("HOME") {
        for key_name in ["id_rsa", "id_ed25519"] {
            let key_path = format!("{}/.ssh/{}", home, key_name);
            if let Ok(key_str) = std::fs::read_to_string(&key_path) {
                if let Ok(key) = russh::keys::ssh_key::PrivateKey::from_openssh(&key_str) {
                    let hash_alg = match handle.best_supported_rsa_hash().await {
                        Ok(Some(Some(h))) => h,
                        _ => russh::keys::ssh_key::HashAlg::Sha256,
                    };
                    let key_with_alg =
                        russh::keys::PrivateKeyWithHashAlg::new(Arc::new(key), Some(hash_alg));
                    let result = handle.authenticate_publickey(user, key_with_alg).await?;
                    if matches!(result, client::AuthResult::Success) {
                        authed = true;
                        break;
                    }
                }
            }
        }
    }

    if !authed {
        // Fall back to password.
        let result = handle.authenticate_password(user, pass).await?;
        if !matches!(result, client::AuthResult::Success) {
            // Try keyboard-interactive fallback.
            use client::KeyboardInteractiveAuthResponse;
            let mut response = handle
                .authenticate_keyboard_interactive_start(user, None::<String>)
                .await?;
            for _ in 0..3 {
                match response {
                    KeyboardInteractiveAuthResponse::Success => {
                        authed = true;
                        break;
                    }
                    KeyboardInteractiveAuthResponse::Failure { .. } => break,
                    KeyboardInteractiveAuthResponse::InfoRequest { prompts, .. } => {
                        let answers: Vec<String> =
                            prompts.iter().map(|_| pass.to_string()).collect();
                        response = handle
                            .authenticate_keyboard_interactive_respond(answers)
                            .await?;
                    }
                }
            }
            if !authed {
                anyhow::bail!("auth failed (tried publickey + password + keyboard-interactive)");
            }
        }
    }

    let channel = handle.channel_open_session().await?;
    channel
        .request_pty(
            false,
            "xterm-256color",
            80,
            24,
            0,
            0,
            &[
                (Pty::ECHO, 1),
                (Pty::ICANON, 1),
                (Pty::ISIG, 1),
                (Pty::IEXTEN, 1),
                (Pty::ICRNL, 1),
                (Pty::OPOST, 1),
                (Pty::ONLCR, 1),
                (Pty::ECHOE, 1),
                (Pty::ECHOK, 1),
                (Pty::ECHOCTL, 1),
                (Pty::ECHOKE, 1),
            ],
        )
        .await?;
    channel.request_shell(true).await?;

    let (read_half, write_half) = channel.split();

    // Collect inputs into owned Strings so the spawned task is 'static.
    let mut all_inputs: Vec<String> = Vec::with_capacity(inputs.len() + 1);
    all_inputs.push(format!("{}\n", SHELL_INTEGRATION_SCRIPT));
    for input in inputs {
        all_inputs.push(format!("{}\n", input));
    }

    // Input pump: send each input directly to the writer, with delays so
    // the shell has time to process each one. This avoids the queue/drain
    // pattern which would send all inputs at once.
    let input_task = tokio::spawn(async move {
        let writer = write_half;
        // Small initial delay to let the shell finish starting.
        tokio::time::sleep(Duration::from_millis(500)).await;
        for (i, input) in all_inputs.iter().enumerate() {
            if writer.data_bytes(input.clone().into_bytes()).await.is_err() {
                break;
            }
            // Wait for the shell to process the previous input before
            // sending the next one. This avoids interleaving.
            if i == 0 {
                // Shell-integration script needs more time to define
                // the function and set PROMPT_COMMAND.
                tokio::time::sleep(Duration::from_millis(800)).await;
            } else {
                tokio::time::sleep(Duration::from_millis(800)).await;
            }
        }
        let _ = writer;
    });

    // Output reader: collect all output until the channel closes or we've
    // seen enough prompts.
    let mut output = Vec::new();
    let mut reader = read_half;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let msg = tokio::time::timeout_at(deadline, reader.wait()).await;
        match msg {
            Ok(Some(ChannelMsg::Data { data })) => {
                output.extend_from_slice(&data);
                // If we see "exit" echoed back and then a close, we're done.
            }
            Ok(Some(ChannelMsg::Eof)) | Ok(Some(ChannelMsg::Close)) | Ok(None) => break,
            Ok(Some(_)) => continue,
            Err(_) => break, // timeout
        }
    }
    // Send a final Ctrl+D / exit to clean up.
    input_task.abort();
    let _ = handle
        .disconnect(russh::Disconnect::ByApplication, "", "en")
        .await;
    Ok(output)
}

/// Parse the OSC 133;D exit-code sequences out of raw terminal output.
fn parse_exit_codes(output: &[u8]) -> Vec<Option<i32>> {
    let mut codes = Vec::new();
    let mut i = 0;
    while i < output.len() {
        if output[i] == 0x1b
            && i + 1 < output.len()
            && output[i + 1] == b']'
            && output[i + 2..].starts_with(b"133;D")
        {
            let mut j = i + 2 + 6; // skip "133;D"
            if j < output.len() && output[j] == b';' {
                j += 1;
            }
            let digit_start = j;
            while j < output.len() && output[j].is_ascii_digit() {
                j += 1;
            }
            let code = if j > digit_start {
                std::str::from_utf8(&output[digit_start..j])
                    .ok()
                    .and_then(|s| s.parse::<i32>().ok())
            } else {
                Some(0) // 133;D with no code → treat as 0
            };
            codes.push(code);
            i = j;
        } else {
            i += 1;
        }
    }
    codes
}

#[tokio::test]
#[ignore = "live SSH test — set RUSTERM_LIVE_SSH_TEST=1 to enable"]
async fn live_pwdwd_emits_nonzero_exit_code() {
    let (host, user, pass) = match live_host() {
        Some(v) => v,
        None => {
            eprintln!(
                "skipping live test — set RUSTERM_LIVE_SSH_TEST=1 + \
                 RUSTERM_LIVE_SSH_HOST/USER/PASS to enable"
            );
            return;
        }
    };

    let output = connect_and_run(&host, &user, &pass, &["pwdwd", "pwd", "exit"])
        .await
        .expect("SSH connection failed");

    let codes = parse_exit_codes(&output);
    eprintln!("parsed exit codes: {:?}", codes);
    eprintln!(
        "output tail: {:?}",
        String::from_utf8_lossy(&output[output.len().saturating_sub(200)..])
    );

    // The shell emits OSC 133;D on every prompt. After injecting the
    // integration script, we should see at least 3 prompts:
    //   1. After the integration script itself runs (rc=0).
    //   2. After `pwdwd` (rc=127 — command not found).
    //   3. After `pwd` (rc=0).
    //
    // The first prompt might not have the integration loaded yet (the
    // script runs after the first prompt). So we look for a non-zero
    // code (127) somewhere in the sequence — that's the `pwdwd` failure.
    assert!(
        codes.iter().any(|c| *c == Some(127)),
        "expected to see exit code 127 (pwdwd not found) in the OSC 133;D \
         stream, but got: {:?}. This means shell integration is NOT \
         capturing the failed command's exit code, which is the root cause \
         of the 'popup still shows pwdwd' bug.",
        codes
    );

    // And we should also see a 0 (pwd succeeded) AFTER the 127.
    let pos_127 = codes.iter().position(|c| *c == Some(127));
    if let Some(idx) = pos_127 {
        assert!(
            codes[idx..].iter().any(|c| *c == Some(0)),
            "expected a 0 (pwd succeeded) after the 127 (pwdwd failed), \
             but the tail was: {:?}",
            &codes[idx..]
        );
    }
}

/// End-to-end test: connect to the live host, run `pwdwd`, verify the
/// shell integration reports rc=127, then simulate the full app flow
/// (mark_command_failed + search_history) to confirm `pwdwd` is filtered
/// out of suggestions.
///
/// This test combines the SSH verification with the DB verification to
/// pin the entire "failed command must not appear in suggestions" contract.
#[tokio::test]
#[ignore = "live SSH test — set RUSTERM_LIVE_SSH_TEST=1 to enable"]
async fn live_pwdwd_full_flow_filters_from_suggestions() {
    let (host, user, pass) = match live_host() {
        Some(v) => v,
        None => {
            eprintln!(
                "skipping live test — set RUSTERM_LIVE_SSH_TEST=1 + \
                 RUSTERM_LIVE_SSH_HOST/USER/PASS to enable"
            );
            return;
        }
    };

    // Step 1: connect and run pwdwd + pwd. Capture the OSC 133;D exit codes.
    let output = connect_and_run(&host, &user, &pass, &["pwdwd", "pwd", "exit"])
        .await
        .expect("SSH connection failed");
    let codes = parse_exit_codes(&output);
    eprintln!("parsed exit codes: {:?}", codes);

    // Verify shell integration captured the failure.
    assert!(
        codes.iter().any(|c| *c == Some(127)),
        "expected rc=127 for pwdwd on the live host, got: {:?}",
        codes
    );

    // Step 2: simulate the app's runtime handling of the failure.
    // The app would call `mark_command_failed("pwdwd", 127)` when it sees
    // rc=127 from OSC 133;D. We replicate that here against a temp DB.
    let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
    let db_path = temp_dir.path().join("test.db");
    let db = rusterm_db::Database::open(Some(db_path))
        .await
        .expect("failed to open DB");

    // Simulate a prior history import (as if `pwdwd` was in
    // `~/.bash_history` from a previous session).
    db.save_history_batch(vec![
        rusterm_db::history::HistoryEntry {
            id: "imp-pwdwd".to_string(),
            command: "pwdwd".to_string(),
            session_id: "s".to_string(),
            cwd: None,
            hostname: Some(host.clone()),
            exit_code: None,
            duration_ms: None,
            created_at: chrono::Utc::now().to_rfc3339(),
        },
        rusterm_db::history::HistoryEntry {
            id: "imp-pwd".to_string(),
            command: "pwd".to_string(),
            session_id: "s".to_string(),
            cwd: None,
            hostname: Some(host.clone()),
            exit_code: None,
            duration_ms: None,
            created_at: chrono::Utc::now().to_rfc3339(),
        },
    ])
    .await
    .unwrap();

    // Before the failure marker, both pwdwd and pwd are suggested.
    let before = db.search_history("pw", 10).await.unwrap();
    let cmds_before: Vec<String> = before.iter().map(|e| e.command.clone()).collect();
    assert!(
        cmds_before.contains(&"pwdwd".to_string()),
        "pwdwd should be suggested before the failure marker: {:?}",
        cmds_before
    );

    // Now mark pwdwd as failed (mirrors what the app does on rc=127).
    db.mark_command_failed("pwdwd", 127).await.unwrap();

    // After the marker, pwdwd must NOT be suggested.
    let after = db.search_history("pw", 10).await.unwrap();
    let cmds_after: Vec<String> = after.iter().map(|e| e.command.clone()).collect();
    assert!(
        !cmds_after.contains(&"pwdwd".to_string()),
        "pwdwd must NOT be suggested after mark_command_failed: {:?}",
        cmds_after
    );
    assert!(
        cmds_after.contains(&"pwd".to_string()),
        "pwd should still be suggested: {:?}",
        cmds_after
    );

    // Step 3: simulate a reconnect (history import runs again).
    // The import fetches known_failed_commands and skips them.
    let failed_set = db.known_failed_commands().await.unwrap();
    assert!(
        failed_set.contains("pwdwd"),
        "known_failed_commands must report pwdwd so the import skips it: {:?}",
        failed_set
    );

    // Simulate the import: delete by hostname, then re-insert filtered.
    db.delete_history_by_hostname(&host).await.unwrap();
    let reimport = vec!["pwdwd".to_string(), "pwd".to_string()];
    let filtered: Vec<_> = reimport
        .into_iter()
        .filter(|c| !failed_set.contains(c))
        .collect();
    let entries: Vec<_> = filtered
        .iter()
        .map(|c| rusterm_db::history::HistoryEntry {
            id: uuid::Uuid::new_v4().to_string(),
            command: c.clone(),
            session_id: "s".to_string(),
            cwd: None,
            hostname: Some(host.clone()),
            exit_code: None,
            duration_ms: None,
            created_at: chrono::Utc::now().to_rfc3339(),
        })
        .collect();
    db.save_history_batch(entries).await.unwrap();

    // After reconnect, pwdwd must STILL not be suggested.
    let after_reconnect = db.search_history("pw", 10).await.unwrap();
    let cmds_reconnect: Vec<String> = after_reconnect.iter().map(|e| e.command.clone()).collect();
    assert!(
        !cmds_reconnect.contains(&"pwdwd".to_string()),
        "pwdwd must NOT reappear after reconnect: {:?}",
        cmds_reconnect
    );
    assert!(
        cmds_reconnect.contains(&"pwd".to_string()),
        "pwd should still be suggested after reconnect: {:?}",
        cmds_reconnect
    );

    eprintln!(
        "Full flow verified: pwdwd is filtered from suggestions across \
              failure + reconnect cycles."
    );
}

/// Sanity test for the parser — doesn't need a live host.
#[test]
fn parse_exit_codes_handles_basics() {
    // ESC ] 1 3 3 ; D ; 1 2 7 ESC \
    let input = b"\x1b]133;D;127\x1b\\\x1b]133;D;0\x1b\\";
    let codes = parse_exit_codes(input);
    assert_eq!(codes, vec![Some(127), Some(0)]);

    // No code → defaults to 0.
    let input = b"\x1b]133;D\x1b\\";
    let codes = parse_exit_codes(input);
    assert_eq!(codes, vec![Some(0)]);
}
