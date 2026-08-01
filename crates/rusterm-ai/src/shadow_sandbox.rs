//! User-approved bridge between LLM command suggestions and a live terminal.
//!
//! This module intentionally has no shell, process, SSH, or PTY dependency. It
//! can observe output supplied by the terminal layer, but the only way to obtain
//! an executable command from it is the explicit `approve_execution` transition.
//! Captured output is likewise excluded from model context until
//! `approve_result_sharing` is called.

use std::mem;

const MAX_CAPTURE_BYTES: usize = 64 * 1024;
const MAX_SHARED_RESULTS: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowExecutionRequest {
    pub command: String,
    pub session_id: String,
    pub target_label: String,
    pub working_directory: Option<String>,
    pub risk_reason: Option<String>,
}

impl ShadowExecutionRequest {
    pub fn new(
        command: impl Into<String>,
        session_id: impl Into<String>,
        target_label: impl Into<String>,
        working_directory: Option<String>,
        risk_reason: Option<String>,
    ) -> Self {
        Self {
            command: command.into(),
            session_id: session_id.into(),
            target_label: target_label.into(),
            working_directory,
            risk_reason,
        }
    }
}

/// One-shot capability returned only after a user approves execution.
///
/// The UI consumes this value to write to its existing PTY. The sandbox itself
/// cannot execute anything.
#[derive(Debug, PartialEq, Eq)]
pub struct ApprovedExecution {
    command: String,
    session_id: String,
}

impl ApprovedExecution {
    pub fn command(&self) -> &str {
        &self.command
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowExecutionResult {
    pub command: String,
    pub session_id: String,
    pub target_label: String,
    pub working_directory: Option<String>,
    pub output: String,
    pub exit_code: Option<i32>,
    pub truncated: bool,
}

impl ShadowExecutionResult {
    fn model_context(&self) -> String {
        let exit = self
            .exit_code
            .map(|code| code.to_string())
            .unwrap_or_else(|| "unavailable".to_string());
        let cwd = self.working_directory.as_deref().unwrap_or("unknown");
        format!(
            "Target: {}\nWorking directory: {}\nCommand: {}\nExit code: {}\nOutput:\n{}",
            self.target_label, cwd, self.command, exit, self.output
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShadowSandboxPhase {
    Idle,
    AwaitingExecution(ShadowExecutionRequest),
    Capturing {
        request: ShadowExecutionRequest,
        output: Vec<u8>,
        truncated: bool,
    },
    AwaitingResultSharing(ShadowExecutionResult),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ShadowSandboxError {
    #[error("another shadow-sandbox approval is already in progress")]
    Busy,
    #[error("the shadow sandbox is not waiting for execution approval")]
    NotAwaitingExecution,
    #[error("the shadow sandbox is not capturing this session")]
    NotCapturingSession,
    #[error("the shadow sandbox is not waiting for result-sharing approval")]
    NotAwaitingResultSharing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowSandbox {
    phase: ShadowSandboxPhase,
    shared_results: Vec<ShadowExecutionResult>,
}

impl Default for ShadowSandbox {
    fn default() -> Self {
        Self {
            phase: ShadowSandboxPhase::Idle,
            shared_results: Vec::new(),
        }
    }
}

impl ShadowSandbox {
    pub fn phase(&self) -> &ShadowSandboxPhase {
        &self.phase
    }

    pub fn pending_execution(&self) -> Option<&ShadowExecutionRequest> {
        match &self.phase {
            ShadowSandboxPhase::AwaitingExecution(request) => Some(request),
            _ => None,
        }
    }

    pub fn pending_result(&self) -> Option<&ShadowExecutionResult> {
        match &self.phase {
            ShadowSandboxPhase::AwaitingResultSharing(result) => Some(result),
            _ => None,
        }
    }

    pub fn shared_result_count(&self) -> usize {
        self.shared_results.len()
    }

    /// Store a model suggestion for display. This transition never returns a
    /// command, so merely receiving or clicking a suggestion cannot execute it.
    pub fn propose(&mut self, request: ShadowExecutionRequest) -> Result<(), ShadowSandboxError> {
        if !matches!(self.phase, ShadowSandboxPhase::Idle) {
            return Err(ShadowSandboxError::Busy);
        }
        self.phase = ShadowSandboxPhase::AwaitingExecution(request);
        Ok(())
    }

    pub fn reject_execution(&mut self) -> Result<(), ShadowSandboxError> {
        if !matches!(self.phase, ShadowSandboxPhase::AwaitingExecution(_)) {
            return Err(ShadowSandboxError::NotAwaitingExecution);
        }
        self.phase = ShadowSandboxPhase::Idle;
        Ok(())
    }

    /// Explicit user approval. The returned capability is one-shot: after this
    /// transition, calling the method again fails and cannot duplicate execution.
    pub fn approve_execution(&mut self) -> Result<ApprovedExecution, ShadowSandboxError> {
        let phase = mem::replace(&mut self.phase, ShadowSandboxPhase::Idle);
        let ShadowSandboxPhase::AwaitingExecution(request) = phase else {
            self.phase = phase;
            return Err(ShadowSandboxError::NotAwaitingExecution);
        };

        let approved = ApprovedExecution {
            command: request.command.clone(),
            session_id: request.session_id.clone(),
        };
        self.phase = ShadowSandboxPhase::Capturing {
            request,
            output: Vec::new(),
            truncated: false,
        };
        Ok(approved)
    }

    /// Observe terminal output for the approved session. Output from every other
    /// session is ignored, preventing cross-session context leakage.
    pub fn record_output(&mut self, session_id: &str, data: &[u8]) {
        let ShadowSandboxPhase::Capturing {
            request,
            output,
            truncated,
        } = &mut self.phase
        else {
            return;
        };
        if request.session_id != session_id {
            return;
        }

        let remaining = MAX_CAPTURE_BYTES.saturating_sub(output.len());
        let copy_len = remaining.min(data.len());
        output.extend_from_slice(&data[..copy_len]);
        if copy_len < data.len() {
            *truncated = true;
        }
    }

    pub fn finish_execution(
        &mut self,
        session_id: &str,
        exit_code: i32,
    ) -> Result<(), ShadowSandboxError> {
        self.finish(session_id, Some(exit_code), None)
    }

    pub fn fail_execution(
        &mut self,
        session_id: &str,
        reason: impl AsRef<str>,
    ) -> Result<(), ShadowSandboxError> {
        self.finish(session_id, None, Some(reason.as_ref()))
    }

    fn finish(
        &mut self,
        session_id: &str,
        exit_code: Option<i32>,
        failure: Option<&str>,
    ) -> Result<(), ShadowSandboxError> {
        let phase = mem::replace(&mut self.phase, ShadowSandboxPhase::Idle);
        let ShadowSandboxPhase::Capturing {
            request,
            mut output,
            truncated,
        } = phase
        else {
            self.phase = phase;
            return Err(ShadowSandboxError::NotCapturingSession);
        };
        if request.session_id != session_id {
            self.phase = ShadowSandboxPhase::Capturing {
                request,
                output,
                truncated,
            };
            return Err(ShadowSandboxError::NotCapturingSession);
        }

        if let Some(reason) = failure {
            output.extend_from_slice(format!("\n[execution unavailable: {reason}]\n").as_bytes());
        }
        let output = sanitize_terminal_output(&String::from_utf8_lossy(&output));
        self.phase = ShadowSandboxPhase::AwaitingResultSharing(ShadowExecutionResult {
            command: request.command,
            session_id: request.session_id,
            target_label: request.target_label,
            working_directory: request.working_directory,
            output,
            exit_code,
            truncated,
        });
        Ok(())
    }

    /// Rejecting keeps the result local only and discards it from the gateway.
    pub fn reject_result_sharing(&mut self) -> Result<(), ShadowSandboxError> {
        if !matches!(self.phase, ShadowSandboxPhase::AwaitingResultSharing(_)) {
            return Err(ShadowSandboxError::NotAwaitingResultSharing);
        }
        self.phase = ShadowSandboxPhase::Idle;
        Ok(())
    }

    /// Add the reviewed result to the only context collection exposed for LLM
    /// requests. Unapproved captured output never reaches `llm_context`.
    pub fn approve_result_sharing(&mut self) -> Result<ShadowExecutionResult, ShadowSandboxError> {
        let phase = mem::replace(&mut self.phase, ShadowSandboxPhase::Idle);
        let ShadowSandboxPhase::AwaitingResultSharing(result) = phase else {
            self.phase = phase;
            return Err(ShadowSandboxError::NotAwaitingResultSharing);
        };

        self.shared_results.push(result.clone());
        if self.shared_results.len() > MAX_SHARED_RESULTS {
            self.shared_results.remove(0);
        }
        Ok(result)
    }

    /// Model context containing only results for which the user completed the
    /// second approval step.
    pub fn llm_context(&self) -> String {
        self.shared_results
            .iter()
            .enumerate()
            .map(|(index, result)| {
                format!(
                    "Approved terminal result {}:\n{}",
                    index + 1,
                    result.model_context()
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    pub fn cancel_session(&mut self, session_id: &str) {
        let belongs_to_session = match &self.phase {
            ShadowSandboxPhase::AwaitingExecution(request) => request.session_id == session_id,
            ShadowSandboxPhase::Capturing { request, .. } => request.session_id == session_id,
            ShadowSandboxPhase::AwaitingResultSharing(result) => result.session_id == session_id,
            ShadowSandboxPhase::Idle => false,
        };
        if belongs_to_session {
            self.phase = ShadowSandboxPhase::Idle;
        }
    }
}

/// Remove ANSI CSI/OSC sequences and non-printable terminal controls before a
/// result is displayed or placed in model context. Newlines and tabs are kept.
fn sanitize_terminal_output(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut output = String::with_capacity(input.len());
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] == 0x1b {
            index += 1;
            if index >= bytes.len() {
                break;
            }
            match bytes[index] {
                b'[' => {
                    index += 1;
                    while index < bytes.len() {
                        let byte = bytes[index];
                        index += 1;
                        if (0x40..=0x7e).contains(&byte) {
                            break;
                        }
                    }
                }
                b']' => {
                    index += 1;
                    while index < bytes.len() {
                        if bytes[index] == 0x07 {
                            index += 1;
                            break;
                        }
                        if bytes[index] == 0x1b
                            && index + 1 < bytes.len()
                            && bytes[index + 1] == b'\\'
                        {
                            index += 2;
                            break;
                        }
                        index += 1;
                    }
                }
                _ => index += 1,
            }
            continue;
        }

        let ch = input[index..]
            .chars()
            .next()
            .expect("valid UTF-8 char boundary");
        index += ch.len_utf8();
        if ch == '\n' || ch == '\t' || !ch.is_control() {
            output.push(ch);
        } else if ch == '\r' {
            output.push('\n');
        }
    }

    while output.contains("\n\n\n") {
        output = output.replace("\n\n\n", "\n\n");
    }
    output.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> ShadowExecutionRequest {
        ShadowExecutionRequest::new(
            "uname -a",
            "session-1",
            "prod@example",
            Some("/srv/app".to_string()),
            None,
        )
    }

    #[test]
    fn suggestion_never_yields_an_executable_command() {
        let mut sandbox = ShadowSandbox::default();
        sandbox.propose(request()).unwrap();

        assert!(matches!(
            sandbox.phase(),
            ShadowSandboxPhase::AwaitingExecution(_)
        ));
        assert!(sandbox.llm_context().is_empty());
    }

    #[test]
    fn rejecting_execution_returns_to_idle_without_a_capability() {
        let mut sandbox = ShadowSandbox::default();
        sandbox.propose(request()).unwrap();
        sandbox.reject_execution().unwrap();

        assert_eq!(sandbox.phase(), &ShadowSandboxPhase::Idle);
        assert_eq!(
            sandbox.approve_execution(),
            Err(ShadowSandboxError::NotAwaitingExecution)
        );
    }

    #[test]
    fn approval_is_one_shot() {
        let mut sandbox = ShadowSandbox::default();
        sandbox.propose(request()).unwrap();

        let approved = sandbox.approve_execution().unwrap();
        assert_eq!(approved.command(), "uname -a");
        assert_eq!(approved.session_id(), "session-1");
        assert_eq!(
            sandbox.approve_execution(),
            Err(ShadowSandboxError::NotAwaitingExecution)
        );
    }

    #[test]
    fn captured_result_is_not_model_context_before_share_approval() {
        let mut sandbox = ShadowSandbox::default();
        sandbox.propose(request()).unwrap();
        sandbox.approve_execution().unwrap();
        sandbox.record_output("session-1", b"Linux secret-host 6.8\r\n");
        sandbox.finish_execution("session-1", 0).unwrap();

        assert!(sandbox.pending_result().is_some());
        assert!(sandbox.llm_context().is_empty());
    }

    #[test]
    fn rejecting_sharing_discards_result_without_polluting_context() {
        let mut sandbox = ShadowSandbox::default();
        sandbox.propose(request()).unwrap();
        sandbox.approve_execution().unwrap();
        sandbox.record_output("session-1", b"private output\n");
        sandbox.finish_execution("session-1", 0).unwrap();
        sandbox.reject_result_sharing().unwrap();

        assert_eq!(sandbox.phase(), &ShadowSandboxPhase::Idle);
        assert!(sandbox.llm_context().is_empty());
    }

    #[test]
    fn approved_result_is_the_only_output_added_to_model_context() {
        let mut sandbox = ShadowSandbox::default();
        sandbox.propose(request()).unwrap();
        sandbox.approve_execution().unwrap();
        sandbox.record_output("other-session", b"must not leak\n");
        sandbox.record_output("session-1", b"\x1b[32mapproved output\x1b[0m\r\n");
        sandbox.finish_execution("session-1", 0).unwrap();
        sandbox.approve_result_sharing().unwrap();

        let context = sandbox.llm_context();
        assert!(context.contains("approved output"));
        assert!(context.contains("Exit code: 0"));
        assert!(!context.contains("must not leak"));
        assert!(!context.contains("\x1b[32m"));
    }

    #[test]
    fn closing_a_session_cancels_its_pending_workflow() {
        let mut sandbox = ShadowSandbox::default();
        sandbox.propose(request()).unwrap();
        sandbox.cancel_session("session-1");
        assert_eq!(sandbox.phase(), &ShadowSandboxPhase::Idle);
    }
}
