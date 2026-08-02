//! Tiny curl-command parser for the `/exec/parse-curl` endpoint.
//!
//! Users paste whatever curl command their tooling generated; we extract the
//! method, URL, headers and body into structured JSON so the UI can show
//! "what you're actually about to run" and the client can convert it into a
//! proper `/exec` request. We deliberately parse — never execute — here.
//!
//! This is NOT a full clone of `curl`'s option set. It handles the common
//! flags (`-X`, `-H`, `-d/--data*` family, `-u`, `-k`, `-sS`) and skips
//! unknown single-value flags conservatively. Ambiguous input is an explicit
//! error instead of a guess.

use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ParsedCurl {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
    /// `-u user:pass` when present. Password is included because the caller
    /// already has the raw command; this is informational only.
    pub basic_auth: Option<String>,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum CurlParseError {
    #[error("input does not start with `curl`")]
    NotACurlCommand,
    #[error("no URL found in the command")]
    MissingUrl,
    #[error("unbalanced quoting in the command")]
    UnbalancedQuotes,
    #[error("flag {0} requires a value")]
    MissingValue(String),
}

/// Split a shell-style command line into argv, honouring single and double
/// quotes and backslash escapes (outside single quotes). This is a
/// deliberately small subset of POSIX word splitting.
pub fn shell_split(input: &str) -> Result<Vec<String>, CurlParseError> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut chars = input.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;
    let mut started_token = false;

    while let Some(c) = chars.next() {
        match c {
            '\'' if !in_double => {
                in_single = !in_single;
                started_token = true;
            }
            '"' if !in_single => {
                in_double = !in_double;
                started_token = true;
            }
            '\\' if !in_single => {
                // Line continuation `\\\n` or escaped next char.
                match chars.next() {
                    Some('\n') => {}
                    Some(next) => {
                        current.push(next);
                        started_token = true;
                    }
                    None => current.push('\\'),
                }
            }
            c if c.is_whitespace() && !in_single && !in_double => {
                if started_token || !current.is_empty() {
                    parts.push(std::mem::take(&mut current));
                    started_token = false;
                }
            }
            _ => {
                current.push(c);
                started_token = true;
            }
        }
    }
    if in_single || in_double {
        return Err(CurlParseError::UnbalancedQuotes);
    }
    if started_token || !current.is_empty() {
        parts.push(current);
    }
    Ok(parts)
}

/// Parse a pasted curl command into structured fields.
pub fn parse_curl(input: &str) -> Result<ParsedCurl, CurlParseError> {
    let tokens = shell_split(input)?;
    let mut iter = tokens.iter().peekable();

    // Skip everything up to the `curl` binary itself so pasted strings like
    // `$ curl ...` or `curl.exe ...` still parse.
    loop {
        match iter.next() {
            Some(t) if t.trim_start_matches('$').trim() == "curl" || t == "curl.exe" => break,
            Some(t) if t.ends_with("/curl") => break,
            Some(_) => continue,
            None => return Err(CurlParseError::NotACurlCommand),
        }
    }

    let mut method: Option<String> = None;
    let mut url: Option<String> = None;
    let mut headers: Vec<(String, String)> = Vec::new();
    let mut body: Option<String> = None;
    let mut basic_auth: Option<String> = None;

    // Flags that take a value we don't use → skip both tokens.
    const SKIP_WITH_VALUE: &[&str] = &[
        "-o",
        "--output",
        "-A",
        "--user-agent",
        "-e",
        "--referer",
        "-m",
        "--max-time",
        "--connect-timeout",
        "--resolve",
        "--cacert",
        "--cert",
        "--key",
        "-x",
        "--proxy",
        "--proxy-user",
        "-b",
        "--cookie",
        "-c",
        "--cookie-jar",
        "-T",
        "--upload-file",
        "--retry",
        "--limit-rate",
    ];
    // Valueless flags we accept and ignore.
    const SKIP_FLAG: &[&str] = &[
        "-k",
        "--insecure",
        "-s",
        "--silent",
        "-S",
        "--show-error",
        "-i",
        "--include",
        "-v",
        "--verbose",
        "-L",
        "--location",
        "-f",
        "--fail",
        "-N",
        "--no-buffer",
        "--compressed",
    ];

    while let Some(token) = iter.next() {
        let t = token.as_str();
        match t {
            "-X" | "--request" => {
                let value = iter.next().ok_or(CurlParseError::MissingValue(t.into()))?;
                method = Some(value.to_uppercase());
            }
            "-H" | "--header" => {
                let value = iter.next().ok_or(CurlParseError::MissingValue(t.into()))?;
                if let Some((name, val)) = value.split_once(':') {
                    headers.push((name.trim().to_string(), val.trim().to_string()));
                }
            }
            "-d" | "--data" | "--data-raw" | "--data-binary" | "--data-ascii"
            | "--data-urlencode" => {
                let value = iter.next().ok_or(CurlParseError::MissingValue(t.into()))?;
                body = Some(match body.take() {
                    Some(existing) => format!("{existing}&{value}"),
                    None => value.to_string(),
                });
            }
            "-u" | "--user" => {
                let value = iter.next().ok_or(CurlParseError::MissingValue(t.into()))?;
                basic_auth = Some(value.to_string());
            }
            _ if SKIP_WITH_VALUE.contains(&t) => {
                iter.next();
            }
            _ if SKIP_FLAG.contains(&t) => {}
            // Long forms with `=` (e.g. --user=foo:bar, --request=POST).
            _ if t.starts_with("--request=") => {
                method = Some(t["--request=".len()..].to_uppercase());
            }
            _ if t.starts_with("--header=") => {
                let value = &t["--header=".len()..];
                if let Some((name, val)) = value.split_once(':') {
                    headers.push((name.trim().to_string(), val.trim().to_string()));
                }
            }
            _ if t.starts_with("--user=") => {
                basic_auth = Some(t["--user=".len()..].to_string());
            }
            _ if t.starts_with("--data") && t.contains('=') => {
                let value = t[t.find('=').unwrap() + 1..].to_string();
                body = Some(match body.take() {
                    Some(existing) => format!("{existing}&{value}"),
                    None => value,
                });
            }
            _ if t.starts_with('-') && t.len() > 1 => {
                // Unknown flag. `-HAccept: x` style glued short flags are
                // common; handle the data/header/auth shorts.
                if let Some(rest) = t.strip_prefix("-H") {
                    if let Some((name, val)) = rest.split_once(':') {
                        headers.push((name.trim().to_string(), val.trim().to_string()));
                    }
                } else if let Some(rest) = t.strip_prefix("-u") {
                    basic_auth = Some(rest.to_string());
                } else if let Some(rest) = t.strip_prefix("-X") {
                    method = Some(rest.to_uppercase());
                }
                // Anything else unknown is ignored silently — we're a parser,
                // not an executor, so being permissive is safe here.
            }
            _ => {
                // First bare token that's not a flag == the URL.
                url = Some(t.to_string());
            }
        }
    }

    let url = url.ok_or(CurlParseError::MissingUrl)?;
    // When -d is present without -X, curl defaults to POST.
    let method = method.unwrap_or_else(|| if body.is_some() { "POST" } else { "GET" }.to_string());

    Ok(ParsedCurl {
        method,
        url,
        headers,
        body,
        basic_auth,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_split_quotes_and_escapes() {
        assert_eq!(shell_split("a b c").unwrap(), vec!["a", "b", "c"]);
        assert_eq!(shell_split("a 'b c' d").unwrap(), vec!["a", "b c", "d"]);
        assert_eq!(shell_split("a \"b c\" d").unwrap(), vec!["a", "b c", "d"]);
        // Escapes outside single quotes.
        assert_eq!(shell_split(r"a\ b").unwrap(), vec!["a b"]);
        // Quotes can be adjacent to unquoted text.
        assert_eq!(shell_split("a\"b\"c").unwrap(), vec!["abc"]);
        // Line continuation.
        assert_eq!(
            shell_split("curl \\\n  -X GET x").unwrap(),
            vec!["curl", "-X", "GET", "x"]
        );
        assert!(matches!(
            shell_split("'unterminated"),
            Err(CurlParseError::UnbalancedQuotes)
        ));
    }

    #[test]
    fn parse_simple_get() {
        let parsed = parse_curl("curl https://example.com/api").unwrap();
        assert_eq!(parsed.method, "GET");
        assert_eq!(parsed.url, "https://example.com/api");
        assert!(parsed.body.is_none());
        assert!(parsed.headers.is_empty());
    }

    #[test]
    fn parse_post_with_headers_and_body() {
        let parsed = parse_curl(
            "curl -X POST 'https://example.com/api' -H 'Content-Type: application/json' -d '{\"a\":1}'",
        )
        .unwrap();
        assert_eq!(parsed.method, "POST");
        assert_eq!(
            parsed.headers,
            vec![("Content-Type".to_string(), "application/json".to_string())]
        );
        assert_eq!(parsed.body.as_deref(), Some(r#"{"a":1}"#));
    }

    #[test]
    fn data_implies_post() {
        let parsed = parse_curl("curl https://x -d a=1 -d b=2").unwrap();
        assert_eq!(parsed.method, "POST");
        assert_eq!(parsed.body.as_deref(), Some("a=1&b=2"));
    }

    #[test]
    fn parse_basic_auth_flag() {
        let parsed = parse_curl("curl -u alice:secret https://x").unwrap();
        assert_eq!(parsed.basic_auth.as_deref(), Some("alice:secret"));
    }

    #[test]
    fn parse_long_equals_forms() {
        let parsed =
            parse_curl("curl --request=PUT --header=X-Test: 1 --user=bob:pw --data k=v https://x")
                .unwrap();
        assert_eq!(parsed.method, "PUT");
        assert_eq!(
            parsed.headers,
            vec![("X-Test".to_string(), "1".to_string())]
        );
        assert_eq!(parsed.basic_auth.as_deref(), Some("bob:pw"));
        assert_eq!(parsed.body.as_deref(), Some("k=v"));
    }

    #[test]
    fn ignores_safe_flags_and_value_flags() {
        let parsed = parse_curl("curl -skS --compressed -m 5 -o out.txt https://x").unwrap();
        assert_eq!(parsed.method, "GET");
        assert_eq!(parsed.url, "https://x");
    }

    #[test]
    fn multiline_pasted_command() {
        let input = "curl -X POST https://x \\\n  -H 'Authorization: Basic abc' \\\n  -d '{\"k\": \"v with spaces\"}'";
        let parsed = parse_curl(input).unwrap();
        assert_eq!(parsed.method, "POST");
        assert_eq!(parsed.body.as_deref(), Some(r#"{"k": "v with spaces"}"#));
    }

    #[test]
    fn not_a_curl_command_errors() {
        assert!(matches!(
            parse_curl("wget https://x"),
            Err(CurlParseError::NotACurlCommand)
        ));
        assert!(matches!(
            parse_curl(""),
            Err(CurlParseError::NotACurlCommand)
        ));
    }

    #[test]
    fn missing_url_errors() {
        assert!(matches!(
            parse_curl("curl -X POST -d a=1"),
            Err(CurlParseError::MissingUrl)
        ));
    }
}
