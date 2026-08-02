//! Quick-parse for the "fast entry" input in the connection dialog.
//!
//! Users often paste an SSH-style address bar string like
//! `xuchao@jump.zs.shaipower.online -p 22` or `root@1.2.3.4:2222` instead of
//! filling in the host/port/username fields one by one. This module turns
//! that single string into a structured [`HostSpec`] that the dialog can
//! auto-fill from.
//!
//! ## Supported formats
//!
//! | Input                                   | user    | host                         | port | protocol |
//! | --------------------------------------- | ------- | ---------------------------- | ---- | -------- |
//! | `xuchao@jump.zs.shaipower.online -p 22` | xuchao  | jump.zs.shaipower.online     | 22   | ssh      |
//! | `root@1.2.3.4:2222`                     | root    | 1.2.3.4                      | 2222 | ssh      |
//! | `alice@host`                            | alice   | host                         | —    | ssh      |
//! | `host -p 2222`                          | —       | host                         | 2222 | ssh      |
//! | `host:23`                               | —       | host                         | 23   | ssh      |
//! | `host`                                  | —       | host                         | —    | ssh      |
//! | `telnet://user@host:23`                 | user    | host                         | 23   | telnet   |
//! | `telnet host 23`                        | —       | host                         | 23   | telnet   |
//! | `ssh host -p 2222`                      | —       | host                         | 2222 | ssh      |
//!
//! ## Default port
//!
//! When the user does not specify a port, [`HostSpec::port`] stays `None` and
//! the caller is expected to call [`HostSpec::resolved_port`] (or
//! [`default_port`]) to fill in the conventional port for the protocol
//! (ssh=22, telnet=23). This keeps the parser side-effect-free: it only
//! reports what the user typed, never invents values.

use std::fmt;

use thiserror::Error;

/// Connection protocol inferred from the quick-entry input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Ssh,
    Telnet,
}

impl Protocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Protocol::Ssh => "ssh",
            Protocol::Telnet => "telnet",
        }
    }
}

impl fmt::Display for Protocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Conventional default port for a protocol. Used when the user does not
/// specify a port in the quick-entry input.
pub fn default_port(protocol: Protocol) -> u16 {
    match protocol {
        Protocol::Ssh => 22,
        Protocol::Telnet => 23,
    }
}

/// Parsed quick-entry input.
///
/// `user` and `port` are `None` when the user did not specify them; the
/// caller decides whether to leave the corresponding form fields alone or
/// fill them with defaults via [`HostSpec::resolved_port`] /
/// `user.unwrap_or_default()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostSpec {
    pub user: Option<String>,
    pub host: String,
    pub port: Option<u16>,
    pub protocol: Protocol,
}

impl HostSpec {
    /// Port the user typed, or the conventional default for the protocol
    /// when they didn't specify one.
    pub fn resolved_port(&self) -> u16 {
        self.port.unwrap_or_else(|| default_port(self.protocol))
    }
}

/// Error returned by [`parse_host_input`]. The message is user-facing
/// (shown in the dialog as the parse-error hint), so it is kept short and
/// actionable.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum HostSpecError {
    #[error("empty input")]
    Empty,
    #[error("invalid port: {0}")]
    InvalidPort(String),
    #[error("missing host")]
    MissingHost,
    #[error("multiple -p flags")]
    DuplicatePortFlag,
}

/// Parse a quick-entry string into a [`HostSpec`].
///
/// See the module docs for the supported formats. The parser is deliberately
/// lenient about whitespace: any run of spaces is treated as a single
/// separator, so `user@host   -p   22` parses the same as
/// `user@host -p 22`.
pub fn parse_host_input(input: &str) -> Result<HostSpec, HostSpecError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(HostSpecError::Empty);
    }

    // 1) Strip an optional `protocol://` prefix. We only recognise `ssh://`
    //    and `telnet://` here; other schemes fall through and are treated as
    //    part of the host (which will then fail validation downstream).
    let (mut rest, forced_protocol) = if let Some(after) = trimmed.strip_prefix("ssh://") {
        (after, Some(Protocol::Ssh))
    } else if let Some(after) = trimmed.strip_prefix("telnet://") {
        (after, Some(Protocol::Telnet))
    } else {
        (trimmed, None)
    };

    // 2) Strip an optional leading `ssh ` / `telnet ` command-style prefix
    //    (e.g. `ssh host -p 2222`). Only do this when there's another token
    //    after the prefix, so we don't mistake a host literally named "ssh"
    //    or "telnet" for the prefix.
    let mut protocol = forced_protocol.unwrap_or(Protocol::Ssh);
    if forced_protocol.is_none() {
        let mut parts = rest.split_whitespace();
        if let Some(first) = parts.next() {
            // Only consume the prefix when there's a host token after it.
            let has_more = parts.next().is_some();
            if has_more {
                match first.to_ascii_lowercase().as_str() {
                    "ssh" => {
                        protocol = Protocol::Ssh;
                        rest = rest[first.len()..].trim_start();
                    }
                    "telnet" => {
                        protocol = Protocol::Telnet;
                        rest = rest[first.len()..].trim_start();
                    }
                    _ => {}
                }
            }
        }
    }

    // 3) Split on whitespace. The first token is the `user@host[:port]`
    //    target; remaining tokens are `-p <port>` (or `--port <port>`) and
    //    bare-port fallbacks for the `telnet host 23` form.
    let mut tokens = rest.split_whitespace();
    let target = tokens.next().ok_or(HostSpecError::MissingHost)?;

    let (user, host, mut port) = parse_target(target, protocol)?;

    // 4) Walk the remaining tokens for `-p <port>` / `--port <port>` /
    //    `port=N` / bare port (telnet only).
    let mut saw_port_flag = false;
    let mut leftover: Option<&str> = None;
    while let Some(tok) = tokens.next() {
        let lower_tok = tok.to_ascii_lowercase();
        if lower_tok == "-p" || lower_tok == "--port" {
            if saw_port_flag {
                return Err(HostSpecError::DuplicatePortFlag);
            }
            saw_port_flag = true;
            let value = tokens
                .next()
                .ok_or_else(|| HostSpecError::InvalidPort(String::new()))?;
            port = Some(parse_port(value)?);
        } else if let Some(value) = lower_tok.strip_prefix("-p") {
            if saw_port_flag {
                return Err(HostSpecError::DuplicatePortFlag);
            }
            saw_port_flag = true;
            // `-p22` glued form.
            let v = value.trim_start_matches('=');
            if v.is_empty() {
                return Err(HostSpecError::InvalidPort(tok.to_string()));
            }
            port = Some(parse_port(v)?);
        } else if let Some(value) = lower_tok.strip_prefix("--port=") {
            if saw_port_flag {
                return Err(HostSpecError::DuplicatePortFlag);
            }
            saw_port_flag = true;
            port = Some(parse_port(value)?);
        } else if let Some(value) = lower_tok.strip_prefix("port=") {
            if saw_port_flag {
                return Err(HostSpecError::DuplicatePortFlag);
            }
            saw_port_flag = true;
            port = Some(parse_port(value)?);
        } else if protocol == Protocol::Telnet && port.is_none() && leftover.is_none() {
            // `telnet host 23` — the trailing bare numeric token is the port.
            // Only consume it when it parses cleanly as a u16; otherwise leave
            // it alone (it might be a hostname the user typed weirdly).
            if let Ok(p) = tok.parse::<u16>() {
                port = Some(p);
                saw_port_flag = true;
            } else {
                leftover = Some(tok);
            }
        } else {
            leftover = Some(tok);
        }
    }

    // We don't error on leftover tokens: the user may paste trailing junk
    // (e.g. a copied command with extra args). We do log it for debug.
    if let Some(extra) = leftover {
        tracing::debug!(
            "host_spec: ignoring trailing token(s) after target+port: {:?}",
            extra
        );
    }

    if host.is_empty() {
        return Err(HostSpecError::MissingHost);
    }

    Ok(HostSpec {
        user,
        host,
        port,
        protocol,
    })
}

/// Parse the leading `target` token into `(user, host, port)`. `target` may
/// be in any of these forms:
/// - `user@host:port`
/// - `user@host`
/// - `host:port`
/// - `host`
///
/// A `port` embedded in the target overrides anything from `-p` flags later
/// in the input (since it's unambiguous and adjacent to the host), so we
/// return it and let the caller decide whether a later `-p` flag is an
/// error. In practice we let the later `-p` win silently to be lenient.
fn parse_target(
    target: &str,
    protocol: Protocol,
) -> Result<(Option<String>, String, Option<u16>), HostSpecError> {
    // Split `user@host[:port]` on the first `@`. The user part is optional.
    let (user_part, host_port) = match target.split_once('@') {
        Some((u, hp)) => (Some(u.to_string()), hp),
        None => (None, target),
    };

    // Strip surrounding brackets on IPv6 hosts: `[::1]:22` → host=`::1`, port=22.
    // Without this, the `:` inside the IPv6 address confuses the port split.
    let (host, port) = if let Some(stripped) = host_port.strip_prefix('[') {
        // Bracketed IPv6 form. Find the closing `]`; everything after is
        // either empty or `:port`.
        match stripped.find(']') {
            Some(end) => {
                let host = &stripped[..end];
                let after = &stripped[end + 1..];
                let port = after
                    .strip_prefix(':')
                    .filter(|s| !s.is_empty())
                    .map(parse_port)
                    .transpose()?;
                (host.to_string(), port)
            }
            // Unclosed bracket — treat the whole thing as a literal host.
            None => (host_port.to_string(), None),
        }
    } else if let Some((h, p)) = host_port.rsplit_once(':') {
        // `host:port` form. Use `rsplit_once` so IPv6 hosts without brackets
        // (which contain multiple `:`) at least take the last segment as the
        // port candidate; if that doesn't parse as a number we treat the
        // whole thing as a host.
        match p.parse::<u16>() {
            Ok(n) => (h.to_string(), Some(n)),
            Err(_) => (host_port.to_string(), None),
        }
    } else {
        (host_port.to_string(), None)
    };

    // `protocol` is passed in so we can resolve the `host` field's default
    // port if needed — but we keep the parser pure, so we don't fill it in
    // here. The parameter is used only to silence the unused-warning when
    // this function is called from a context that already knows the
    // protocol (and it documents intent for future extensions).
    let _ = protocol;

    if host.is_empty() {
        return Err(HostSpecError::MissingHost);
    }

    Ok((user_part, host, port))
}

fn parse_port(s: &str) -> Result<u16, HostSpecError> {
    s.parse::<u16>()
        .map_err(|_| HostSpecError::InvalidPort(s.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(user: Option<&str>, host: &str, port: Option<u16>, protocol: Protocol) -> HostSpec {
        HostSpec {
            user: user.map(str::to_owned),
            host: host.to_owned(),
            port,
            protocol,
        }
    }

    #[test]
    fn parses_user_at_host_dash_p_port() {
        let s = parse_host_input("xuchao@jump.zs.shaipower.online -p 22").unwrap();
        assert_eq!(
            s,
            spec(
                Some("xuchao"),
                "jump.zs.shaipower.online",
                Some(22),
                Protocol::Ssh
            )
        );
    }

    #[test]
    fn parses_user_at_host_colon_port() {
        let s = parse_host_input("root@1.2.3.4:2222").unwrap();
        assert_eq!(s, spec(Some("root"), "1.2.3.4", Some(2222), Protocol::Ssh));
    }

    #[test]
    fn parses_user_at_host_no_port() {
        let s = parse_host_input("alice@host").unwrap();
        assert_eq!(s, spec(Some("alice"), "host", None, Protocol::Ssh));
        assert_eq!(s.resolved_port(), 22);
    }

    #[test]
    fn parses_host_dash_p_port_no_user() {
        let s = parse_host_input("host -p 2222").unwrap();
        assert_eq!(s, spec(None, "host", Some(2222), Protocol::Ssh));
    }

    #[test]
    fn parses_host_colon_port_no_user() {
        let s = parse_host_input("host:23").unwrap();
        assert_eq!(s, spec(None, "host", Some(23), Protocol::Ssh));
    }

    #[test]
    fn parses_bare_host() {
        let s = parse_host_input("host").unwrap();
        assert_eq!(s, spec(None, "host", None, Protocol::Ssh));
    }

    #[test]
    fn parses_ssh_scheme_prefix() {
        let s = parse_host_input("ssh://user@host:2222").unwrap();
        assert_eq!(s, spec(Some("user"), "host", Some(2222), Protocol::Ssh));
    }

    #[test]
    fn parses_telnet_scheme_prefix() {
        let s = parse_host_input("telnet://user@host:23").unwrap();
        assert_eq!(s, spec(Some("user"), "host", Some(23), Protocol::Telnet));
        assert_eq!(s.resolved_port(), 23);
    }

    #[test]
    fn parses_telnet_command_prefix_with_bare_port() {
        let s = parse_host_input("telnet host 23").unwrap();
        assert_eq!(s, spec(None, "host", Some(23), Protocol::Telnet));
    }

    #[test]
    fn parses_ssh_command_prefix_with_dash_p() {
        let s = parse_host_input("ssh host -p 2222").unwrap();
        assert_eq!(s, spec(None, "host", Some(2222), Protocol::Ssh));
    }

    #[test]
    fn parses_glued_dash_p() {
        let s = parse_host_input("user@host -p22").unwrap();
        assert_eq!(s, spec(Some("user"), "host", Some(22), Protocol::Ssh));
    }

    #[test]
    fn parses_long_port_option() {
        let s = parse_host_input("user@host --port 2222").unwrap();
        assert_eq!(s, spec(Some("user"), "host", Some(2222), Protocol::Ssh));
    }

    #[test]
    fn parses_port_equals_form() {
        let s = parse_host_input("user@host port=2222").unwrap();
        assert_eq!(s, spec(Some("user"), "host", Some(2222), Protocol::Ssh));
    }

    #[test]
    fn parses_long_port_equals_form() {
        let s = parse_host_input("user@host --port=2222").unwrap();
        assert_eq!(s, spec(Some("user"), "host", Some(2222), Protocol::Ssh));
    }

    #[test]
    fn tolerates_extra_whitespace() {
        let s = parse_host_input("  user@host   -p   22  ").unwrap();
        assert_eq!(s, spec(Some("user"), "host", Some(22), Protocol::Ssh));
    }

    #[test]
    fn handles_ipv6_with_brackets_and_port() {
        let s = parse_host_input("user@[2001:db8::1]:2222").unwrap();
        assert_eq!(
            s,
            spec(Some("user"), "2001:db8::1", Some(2222), Protocol::Ssh)
        );
    }

    #[test]
    fn handles_ipv6_with_brackets_no_port() {
        let s = parse_host_input("[2001:db8::1]").unwrap();
        assert_eq!(s, spec(None, "2001:db8::1", None, Protocol::Ssh));
    }

    #[test]
    fn rejects_empty_input() {
        assert_eq!(parse_host_input("").unwrap_err(), HostSpecError::Empty);
        assert_eq!(parse_host_input("   ").unwrap_err(), HostSpecError::Empty);
    }

    #[test]
    fn rejects_invalid_port() {
        assert!(matches!(
            parse_host_input("host -p abc"),
            Err(HostSpecError::InvalidPort(_))
        ));
        assert!(matches!(
            parse_host_input("host -p 99999"),
            Err(HostSpecError::InvalidPort(_))
        ));
    }

    #[test]
    fn rejects_duplicate_port_flag() {
        assert_eq!(
            parse_host_input("host -p 22 -p 23").unwrap_err(),
            HostSpecError::DuplicatePortFlag
        );
    }

    #[test]
    fn rejects_just_protocol_prefix() {
        // `ssh://` with nothing after it → missing host.
        assert!(matches!(
            parse_host_input("ssh://"),
            Err(HostSpecError::MissingHost)
        ));
    }

    #[test]
    fn does_not_mistake_ssh_named_host_for_prefix() {
        // A host literally named "ssh" (no space after) should NOT be
        // treated as the protocol prefix.
        let s = parse_host_input("ssh").unwrap();
        assert_eq!(s, spec(None, "ssh", None, Protocol::Ssh));
    }

    #[test]
    fn resolved_port_uses_protocol_default() {
        let ssh_spec = parse_host_input("host").unwrap();
        assert_eq!(ssh_spec.resolved_port(), 22);

        let telnet_spec = parse_host_input("telnet://host").unwrap();
        assert_eq!(telnet_spec.resolved_port(), 23);
    }

    #[test]
    fn protocol_display() {
        assert_eq!(Protocol::Ssh.to_string(), "ssh");
        assert_eq!(Protocol::Telnet.to_string(), "telnet");
    }

    #[test]
    fn default_port_values() {
        assert_eq!(default_port(Protocol::Ssh), 22);
        assert_eq!(default_port(Protocol::Telnet), 23);
    }

    #[test]
    fn trailing_unrecognized_tokens_are_ignored() {
        // User pasted a whole ssh command including options we don't model.
        let s = parse_host_input("user@host -p 22 -o StrictHostKeyChecking=no").unwrap();
        assert_eq!(s, spec(Some("user"), "host", Some(22), Protocol::Ssh));
    }

    #[test]
    fn parses_telnet_scheme_no_user() {
        let s = parse_host_input("telnet://host:23").unwrap();
        assert_eq!(s, spec(None, "host", Some(23), Protocol::Telnet));
    }

    #[test]
    fn parses_user_at_ipv4_host() {
        let s = parse_host_input("admin@192.168.1.1 -p 2222").unwrap();
        assert_eq!(
            s,
            spec(Some("admin"), "192.168.1.1", Some(2222), Protocol::Ssh)
        );
    }
}
