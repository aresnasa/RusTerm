//! Per-connection login initialization scripts.
//!
//! A login script is a small expect/send DSL stored as raw text on a
//! connection. After a session logs in, the script runs step by step:
//! waiting for terminal output (`expect`), sending text (`send`), sending
//! a credential from the OneKey library (`send_onekey`), or pausing
//! (`delay`).
//!
//! Example:
//!
//! ```text
//! # become root and set up the environment
//! expect [sudo] password for alice: $
//! send_onekey prod-sudo
//! expect [root@web-01]#
//! send source /etc/profile.d/prod.sh
//! delay 250
//! ```

/// One step of a per-connection login initialization script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginStep {
    /// Wait until terminal output matches this regex before continuing.
    Expect { pattern: String },
    /// Send this literal text (plus carriage return) to the session.
    Send { text: String },
    /// Send the credential stored in the named OneKey entry — the actual
    /// secret is resolved at runtime from the unlocked OneKey library and is
    /// NEVER stored in the script itself.
    SendOneKey { name: String },
    /// Pause N milliseconds before the next step.
    Delay { ms: u64 },
}

/// An error encountered while parsing a login script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginScriptError {
    /// 1-based line number where the error occurred.
    pub line: usize,
    /// The offending line.
    pub text: String,
    /// Why the line could not be parsed.
    pub reason: String,
}

impl std::fmt::Display for LoginScriptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}: {} (in {:?})", self.line, self.reason, self.text)
    }
}

impl std::error::Error for LoginScriptError {}

/// Parse a line-based login script into its steps.
///
/// Blank lines and lines starting with `#` are ignored. Every other line
/// must be one of `expect <regex>`, `send <text>`, `send_onekey <name>`,
/// or `delay <ms>`; unknown keywords or missing/invalid arguments produce
/// a [`LoginScriptError`] with the 1-based line number.
pub fn parse_login_script(script: &str) -> Result<Vec<LoginStep>, LoginScriptError> {
    let mut steps = Vec::new();
    for (index, raw_line) in script.lines().enumerate() {
        let line_no = index + 1;
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        steps.push(
            parse_step(line).map_err(|reason| LoginScriptError {
                line: line_no,
                text: line.to_string(),
                reason,
            })?,
        );
    }
    Ok(steps)
}

fn parse_step(line: &str) -> Result<LoginStep, String> {
    let (keyword, rest) = match line.split_once(char::is_whitespace) {
        Some((keyword, rest)) => (keyword, rest.trim()),
        None => (line, ""),
    };

    match keyword {
        "expect" => {
            if rest.is_empty() {
                Err("missing argument for 'expect' (<regex>)".to_string())
            } else {
                Ok(LoginStep::Expect {
                    pattern: rest.to_string(),
                })
            }
        }
        "send" => {
            if rest.is_empty() {
                Err("missing argument for 'send' (<text>)".to_string())
            } else {
                Ok(LoginStep::Send {
                    text: rest.to_string(),
                })
            }
        }
        "send_onekey" => {
            if rest.is_empty() {
                Err("missing argument for 'send_onekey' (<name>)".to_string())
            } else {
                Ok(LoginStep::SendOneKey {
                    name: rest.to_string(),
                })
            }
        }
        "delay" => {
            if rest.is_empty() {
                Err("missing argument for 'delay' (<ms>)".to_string())
            } else {
                rest.parse::<u64>()
                    .map(|ms| LoginStep::Delay { ms })
                    .map_err(|_| {
                        format!("invalid delay {rest:?}: expected integer milliseconds")
                    })
            }
        }
        other => Err(format!("unknown keyword {other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_script_parses_to_no_steps() {
        assert_eq!(parse_login_script(""), Ok(vec![]));
        assert_eq!(parse_login_script("\n  \n\t\n"), Ok(vec![]));
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        assert_eq!(
            parse_login_script("# become root\n\n   \n# and set things up\n"),
            Ok(vec![])
        );
    }

    #[test]
    fn expect_parses_regex_as_the_rest_of_the_line()
        let steps = parse_login_script("expect Password:").unwrap();
        assert_eq!(
            steps,
            vec![LoginStep::Expect {
                pattern: "Password:".to_string()
            }]
        );
    }

    #[test]
    fn expect_preserves_regex_with_spaces_and_colons() {
        let steps = parse_login_script("expect [sudo] password for alice: $").unwrap();
        assert_eq!(
            steps,
            vec![LoginStep::Expect {
                pattern: "[sudo] password for alice: $".to_string()
            }]
        );
    }

    #[test]
    fn send_parses_verbatim_text() {
        let steps = parse_login_script("send echo hello world").unwrap();
        assert_eq!(
            steps,
            vec![LoginStep::Send {
                text: "echo hello world".to_string()
            }]
        );
    }

    #[test]
    fn send_onekey_parses_the_entry_name() {
        let steps = parse_login_script("send_onekey prod-sudo").unwrap();
        assert_eq!(
            steps,
            vec![LoginStep::SendOneKey {
                name: "prod-sudo".to_string()
            }]
        );
    }

    #[test]
    fn delay_parses_integer_milliseconds() {
        let steps = parse_login_script("delay 250").unwrap();
        assert_eq!(steps, vec![LoginStep::Delay { ms: 250 }]);
    }

    #[test]
    fn delay_with_non_integer_errors_with_line_number() {
        let err = parse_login_script("delay abc").unwrap_err();
        assert_eq!(err.line, 1);
        assert_eq!(err.text, "delay abc");
        assert!(err.reason.contains("delay"));
    }

    #[test]
    fn unknown_keyword_errors_with_1_based_line_number() {
        let err = parse_login_script("expect Password:\nfoo bar").unwrap_err();
        assert_eq!(err.line, 2);
        assert_eq!(err.text, "foo bar");
    }

    #[test]
    fn missing_argument_errors_with_line_number() {
        for (script, line) in [
            ("expect", 1),
            ("send", 1),
            ("send_onekey", 1),
            ("delay", 1),
            ("send echo hi\nexpect", 2),
        ] {
            let err = parse_login_script(script).unwrap_err();
            assert_eq!(err.line, line, "script: {script:?}");
        }
    }

    #[test]
    fn multiline_mixed_script_parses_in_order() {
        let script = "\
# become root
expect [sudo] password for alice: $
send_onekey prod-sudo

expect [root@web-01]#
send source /etc/profile.d/prod.sh
delay 250
";
        let steps = parse_login_script(script).unwrap();
        assert_eq!(
            steps,
            vec![
                LoginStep::Expect {
                    pattern: "[sudo] password for alice: $".to_string()
                },
                LoginStep::SendOneKey {
                    name: "prod-sudo".to_string()
                },
                LoginStep::Expect {
                    pattern: "[root@web-01]#".to_string()
                },
                LoginStep::Send {
                    text: "source /etc/profile.d/prod.sh".to_string()
                },
                LoginStep::Delay { ms: 250 },
            ]
        );
    }

    #[test]
    fn error_display_includes_line_reason_and_text() {
        let err = LoginScriptError {
            line: 3,
            text: "foo bar".to_string(),
            reason: "unknown keyword".to_string(),
        };
        let rendered = err.to_string();
        assert!(rendered.contains("3"));
        assert!(rendered.contains("unknown keyword"));
        assert!(rendered.contains("foo bar"));
    }
}
