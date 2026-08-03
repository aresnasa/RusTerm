//! Prompt engineering + response parsing for local template generation.
//!
//! The local Qwen2.5-Coder model is instructed to produce a single shell
//! command, a shell script, or a Python script based on the user's
//! natural-language description. The prompt is carefully scoped so a 1.5B
//! model can reliably produce useful, safe output:
//!
//! - **System role**: "You are a DevOps assistant that writes short
//!   deployment/diagnostic scripts."
//! - **Explicit constraints**: no explanation prose, just the script in a
//!   markdown code fence; use `set -e`; target POSIX `sh`.
//! - **Few-shot**: one concise example anchors the expected format.

#![cfg(feature = "qwen-local")]

/// Which kind of API template the model should generate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemplateKind {
    /// Exactly one POSIX shell command (the API panel's `Command` mode).
    Command,
    /// A POSIX `sh` script (the API panel's `Script` mode).
    ShellScript,
    /// A Python 3 script wrapped in a `python3 - <<'PYEOF'` heredoc so it
    /// runs through the existing relay `sh` executor without changes.
    PythonScript,
}

impl TemplateKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Command => "Command",
            Self::ShellScript => "Shell Script",
            Self::PythonScript => "Python Script",
        }
    }
}

/// Build the user-facing prompt for the Qwen2 instruct chat template.
///
/// The prompt tells the model exactly what to produce and in what format.
/// Keeping it short and explicit is critical for a 1.5B model — verbose
/// instructions degrade output quality.
pub fn build_prompt(description: &str, kind: TemplateKind) -> String {
    if kind == TemplateKind::Command {
        return format!(
            "You are a DevOps assistant. Write exactly one POSIX shell command \
             for the following task. The command must be a single line. Output \
             ONLY the command inside a single ```sh code block — no shebang, \
             `set -e`, comments, or explanation.\n\n\
             Example output:\n\
             ```sh\n\
             uname -a\n\
             ```\n\n\
             Task: {description}"
        );
    }

    let lang = match kind {
        TemplateKind::Command => unreachable!("command prompt returned above"),
        TemplateKind::ShellScript => "POSIX sh",
        TemplateKind::PythonScript => "Python 3 (wrapped in a python3 heredoc)",
    };

    let example = match kind {
        TemplateKind::Command => unreachable!("command prompt returned above"),
        TemplateKind::ShellScript => {
            "Example output:\n\
             ```sh\n\
             #!/bin/sh\n\
             set -e\n\
             echo \"Hostname: $(hostname)\"\n\
             uptime\n\
             df -h /\n\
             ```"
        }
        TemplateKind::PythonScript => {
            "Example output:\n\
             ```sh\n\
             #!/bin/sh\n\
             python3 - <<'PYEOF'\n\
             import subprocess, json\n\
             r = subprocess.run([\"uname\", \"-a\"], capture_output=True, text=True)\n\
             print(json.dumps({\"stdout\": r.stdout.strip()}))\n\
             PYEOF\n\
             ```"
        }
    };

    format!(
        "You are a DevOps assistant. Write a short {lang} script for the \
         following task. Output ONLY the script inside a single ```sh code \
         block — no explanation before or after. Always start with \
         \"#!/bin/sh\" and use `set -e` for shell scripts.\n\n\
         {example}\n\n\
         Task: {description}"
    )
}

/// Extract the script body from the model's response.
///
/// The model is instructed to wrap output in a ```sh fence. This function
/// handles the common cases:
/// 1. Text inside a ```sh ... ``` fence.
/// 2. Text inside a ``` ... ``` fence (unnamed).
/// 3. Raw text with no fence (returned as-is after trimming).
///
/// Any explanation prose outside the fence is discarded — we only want
/// the runnable script.
pub fn parse_response(response: &str) -> String {
    // Look for the first ```fence ... ``` block.
    let trimmed = response.trim();

    // Try ```sh or ```bash or ```python first.
    for fence in &["```sh\n", "```bash\n", "```sh\n", "```python\n", "```\n"] {
        if let Some(start) = trimmed.find(fence) {
            let after_fence = &trimmed[start + fence.len()..];
            if let Some(end) = after_fence.find("```") {
                return after_fence[..end].trim().to_string();
            }
            // Opening fence but no closing — return everything after it.
            return after_fence.trim().to_string();
        }
    }

    // No fence found — check for ``` without newline (e.g. ```sh at line start).
    if let Some(start) = trimmed.find("```") {
        let after = &trimmed[start + 3..];
        // Skip the language tag on the same line (e.g. "sh\n").
        let after = if let Some(nl) = after.find('\n') {
            &after[nl + 1..]
        } else {
            after
        };
        if let Some(end) = after.find("```") {
            return after[..end].trim().to_string();
        }
        return after.trim().to_string();
    }

    // No fence at all — return the raw text. It might still be usable.
    trimmed.to_string()
}

/// Parse a model response according to the requested template contract.
/// Script responses preserve their multi-line body. Command responses are
/// reduced to the first executable line so a small model cannot accidentally
/// put a shebang or a second command into the relay's single-command field.
pub fn parse_generated_response(response: &str, kind: TemplateKind) -> String {
    let parsed = parse_response(response);
    if kind != TemplateKind::Command {
        return parsed;
    }

    parsed
        .lines()
        .map(str::trim)
        .find(|line| {
            !line.is_empty()
                && *line != "set -e"
                && !line.starts_with("#!")
                && !line.starts_with('#')
        })
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sh_fence() {
        let resp = "Here's the script:\n```sh\n#!/bin/sh\necho hello\n```\nDone!";
        assert_eq!(parse_response(resp), "#!/bin/sh\necho hello");
    }

    #[test]
    fn parse_bare_fence() {
        let resp = "```\n#!/bin/sh\necho hi\n```";
        assert_eq!(parse_response(resp), "#!/bin/sh\necho hi");
    }

    #[test]
    fn parse_no_fence() {
        let resp = "#!/bin/sh\necho raw";
        assert_eq!(parse_response(resp), "#!/bin/sh\necho raw");
    }

    #[test]
    fn parse_python_fence() {
        let resp = "```python\nprint('hi')\n```";
        assert_eq!(parse_response(resp), "print('hi')");
    }

    #[test]
    fn build_prompt_contains_task() {
        let p = build_prompt("check disk space", TemplateKind::ShellScript);
        assert!(p.contains("check disk space"));
        assert!(p.contains("POSIX sh"));
    }

    #[test]
    fn command_prompt_requires_exactly_one_shell_command() {
        let prompt = build_prompt("show the kernel", TemplateKind::Command);

        assert!(prompt.contains("show the kernel"));
        assert!(prompt.contains("exactly one"));
        assert!(prompt.contains("single line"));
    }

    #[test]
    fn command_response_returns_one_command_without_script_prelude() {
        let response = "```sh\n#!/bin/sh\nset -e\nuname -a\nuptime\n```";

        assert_eq!(
            parse_generated_response(response, TemplateKind::Command),
            "uname -a"
        );
    }
}
