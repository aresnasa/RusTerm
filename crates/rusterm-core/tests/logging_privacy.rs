//! Integration test: assert that secrets never make it into log output.
//!
//! This test sets up a *real* `tracing` subscriber with the same
//! `RedactingMakeWriter` used in production, emits a log record that
//! (deliberately, for the test) contains a fake secret, and asserts the
//! captured output has the secret scrubbed.
//!
//! It uses a per-test subscriber (not the global one) so it can run in
//! parallel with other tests.

use std::io::Write;
use std::sync::{Arc, Mutex};

use rusterm_core::logging::RedactingMakeWriter;

/// A `Write` impl that captures everything written to it into a shared
/// `Vec<u8>`. Used to inspect what the redacting layer actually produced.
struct CapturingWriter {
    buf: Arc<Mutex<Vec<u8>>>,
}

impl Write for CapturingWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.buf.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// `MakeWriter` impl that always returns a fresh `CapturingWriter` pointing at
/// the same shared buffer.
struct CapturingMakeWriter {
    buf: Arc<Mutex<Vec<u8>>>,
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturingMakeWriter {
    type Writer = CapturingWriter;
    fn make_writer(&'a self) -> Self::Writer {
        CapturingWriter {
            buf: self.buf.clone(),
        }
    }
}

#[test]
fn fake_secret_never_appears_in_captured_log_output() {
    let buf = Arc::new(Mutex::new(Vec::new()));
    let make_writer = CapturingMakeWriter { buf: buf.clone() };
    let redacting = RedactingMakeWriter::new(make_writer);

    // Build a subscriber scoped to this test only (not global).
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("trace"))
        .with_writer(redacting)
        .with_ansi(false)
        .json()
        .finish();

    // Fake secrets that should NEVER appear in captured output.
    let fake_password = "hunter2-super-secret";
    let fake_jwt =
        "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjMifQ.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";
    let fake_pem = "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEAfakefakefakefake\n-----END RSA PRIVATE KEY-----";

    {
        let _guard = tracing::dispatcher::with_default(
            &tracing::dispatcher::Dispatch::new(subscriber),
            || {
                tracing::info!(
                    "connecting password={} jwt={} pem={}",
                    fake_password,
                    fake_jwt,
                    fake_pem
                );
            },
        );
    }

    let captured = String::from_utf8(buf.lock().unwrap().clone()).unwrap();

    assert!(
        !captured.contains(fake_password),
        "password leaked into log output: {captured}"
    );
    assert!(
        !captured.contains(fake_jwt),
        "JWT leaked into log output: {captured}"
    );
    assert!(
        !captured.contains("MIIEpAIBAAKCAQEAfakefakefakefake"),
        "PEM body leaked into log output: {captured}"
    );
    assert!(
        captured.contains("<redacted>"),
        "expected redaction marker in output: {captured}"
    );
}
