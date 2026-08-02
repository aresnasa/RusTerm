//! Privacy sanitizer for analytics storage.
//!
//! Hard requirement: RusTerm must NEVER collect/store/upload passwords,
//! private keys, or authentication tokens in analytics. Every command line
//! and every correction pair passes through [`sanitize_command`] before it is
//! persisted; lines dominated by secret material are dropped entirely.

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::{Captures, Regex};

/// PEM private key material. The whole line is dropped — there is no
/// legitimate reason to keep any part of it in analytics.
static PEM_PRIVATE_KEY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"-----BEGIN[A-Z0-9 ]*PRIVATE KEY-----").unwrap());

/// AWS access key ids (`AKIA` + 16 uppercase alphanumeric chars).
static AWS_ACCESS_KEY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bAKIA[0-9A-Z]{16}\b").unwrap());

/// `Authorization:` headers — the value runs to the closing quote or EOL.
static AUTH_HEADER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)(Authorization:)[^'\"\r\n]*"#).unwrap());

/// Credential-ish name fragment used by the env-assignment and long-flag
/// patterns below (case-insensitive, plural-tolerant).
const CRED_NAME: &str = r"(?:passwords?|passwds?|secrets?|tokens?|api[-_]?keys?|private[-_]?keys?)";

/// Env-var style assignments: `NAME=value` where NAME contains a
/// credential-ish word, e.g. `AWS_SECRET_ACCESS_KEY=...`, `MY_API_KEY=...`.
static ENV_ASSIGNMENT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?i)\b([a-z0-9_]*{cred}[a-z0-9_]*)=[^\s]+",
        cred = CRED_NAME
    ))
    .unwrap()
});

/// Long-flag with `=`: `--password=hunter2`.
static LONG_FLAG_EQ_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?i)(--[a-z0-9_-]*{cred}\b)=[^\s]*",
        cred = CRED_NAME
    ))
    .unwrap()
});

/// Long-flag with a space-separated value: `--password hunter2`.
static LONG_FLAG_SPACE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?i)(--[a-z0-9_-]*{cred}\b)(\s+)\S+",
        cred = CRED_NAME
    ))
    .unwrap()
});

/// MySQL-family glued password flag: `-psecret` (value attached to `-p`).
/// Only applied when the command's first token is a known secret-taking
/// tool, so `git -p` / `ls -p` are never touched.
static GLUED_DASH_P_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(^|\s)-p[^\s]+").unwrap());

/// Tools whose `-p` flag takes a password value.
const SECRET_TAKING_TOOLS: [&str; 4] = ["mysql", "mysqldump", "psql", "mongo"];

/// High-entropy bare tokens: 32+ hex chars or 40+ base64url chars. SSH
/// *public* key blobs (`ssh-rsa AAA...`, `ssh-ed25519 AAA...`) are captured
/// in group 1 and preserved verbatim — public keys are not secret.
static SUSPICIOUS_TOKEN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(ssh-(?:rsa|ed25519|ecdsa)[^\s]*\s+[^\s]+)|\b[0-9a-f]{32,}\b|\b[a-z0-9_-]{40,}\b",
    )
    .unwrap()
});

/// True if the text contains credential material that must never be
/// persisted or exported.
///
/// Equivalent to "sanitize_command(text) != Some(text.to_string())" —
/// either the line would be dropped entirely, or at least one value in it
/// would be redacted.
pub fn contains_sensitive_material(text: &str) -> bool {
    match sanitize_command(text) {
        None => true,
        Some(redacted) => redacted != text,
    }
}

/// Sanitize a command line for analytics storage:
/// - returns None when the line is dominated by secret material (drop it entirely)
/// - otherwise returns Some(line) with any secret VALUES redacted as `***`
pub fn sanitize_command(text: &str) -> Option<String> {
    if PEM_PRIVATE_KEY_RE.is_match(text) {
        return None;
    }
    let mut out = AUTH_HEADER_RE.replace_all(text, "${1} ***").into_owned();
    out = ENV_ASSIGNMENT_RE.replace_all(&out, "${1}=***").into_owned();
    out = LONG_FLAG_EQ_RE.replace_all(&out, "${1}=***").into_owned();
    out = LONG_FLAG_SPACE_RE
        .replace_all(&out, "${1}${2}***")
        .into_owned();
    out = AWS_ACCESS_KEY_RE.replace_all(&out, "***").into_owned();
    out = redact_secret_tool_password(&out).into_owned();
    out = SUSPICIOUS_TOKEN_RE
        .replace_all(&out, |caps: &Captures| {
            if caps.get(1).is_some() {
                caps[0].to_string() // ssh public key blob — not secret
            } else {
                "***".to_string()
            }
        })
        .into_owned();
    Some(out)
}

/// Redact `-p<value>` to `-p***`, but only for commands whose first token
/// is a known secret-taking tool (mysql, mysqldump, psql, mongo).
fn redact_secret_tool_password(line: &str) -> Cow<'_, str> {
    let first = line.split_whitespace().next().unwrap_or("");
    let binary = first.rsplit('/').next().unwrap_or(first);
    if SECRET_TAKING_TOOLS.contains(&binary) {
        GLUED_DASH_P_RE.replace_all(line, "${1}-p***")
    } else {
        Cow::Borrowed(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_command_passes_through_unchanged() {
        assert_eq!(
            sanitize_command("git status -sb"),
            Some("git status -sb".to_string())
        );
        assert!(!contains_sensitive_material("git status -sb"));
    }

    #[test]
    fn literal_non_secret_string_passes_through() {
        assert_eq!(
            sanitize_command("none of this is secret"),
            Some("none of this is secret".to_string())
        );
        assert!(!contains_sensitive_material("none of this is secret"));
    }

    #[test]
    fn git_push_is_untouched() {
        assert_eq!(
            sanitize_command("git push origin main"),
            Some("git push origin main".to_string())
        );
    }

    #[test]
    fn mysql_glued_password_is_redacted() {
        assert_eq!(
            sanitize_command("mysql -psecret db"),
            Some("mysql -p*** db".to_string())
        );
        assert!(contains_sensitive_material("mysql -psecret db"));
    }

    #[test]
    fn psql_and_mysqldump_glued_passwords_are_redacted() {
        assert_eq!(
            sanitize_command("psql -pHunter2 mydb"),
            Some("psql -p*** mydb".to_string())
        );
        assert_eq!(
            sanitize_command("mysqldump -prootpw --all-databases"),
            Some("mysqldump -p*** --all-databases".to_string())
        );
        assert_eq!(
            sanitize_command("mongo -pletmein mydb"),
            Some("mongo -p*** mydb".to_string())
        );
    }

    #[test]
    fn bare_dash_p_is_not_a_secret() {
        // `-p` alone (value comes from the next arg or a prompt) must not
        // be treated as a credential.
        assert_eq!(
            sanitize_command("mysql -p db"),
            Some("mysql -p db".to_string())
        );
        assert!(!contains_sensitive_material("mysql -p db"));
    }

    #[test]
    fn dash_p_in_other_tools_is_not_a_secret() {
        assert_eq!(
            sanitize_command("git -p status"),
            Some("git -p status".to_string())
        );
        assert_eq!(sanitize_command("ls -p"), Some("ls -p".to_string()));
        // Even for a known tool, `-pX` mid-command is NOT glued-password
        // handling unless it looks like the password position; conservatively
        // we only treat `-p` right after the tool name or after spaces.
        // `ls -pkg` must stay untouched.
        assert_eq!(sanitize_command("ls -pkg"), Some("ls -pkg".to_string()));
    }

    #[test]
    fn long_flag_password_equals_is_redacted() {
        assert_eq!(
            sanitize_command("mycli --password=hunter2 login"),
            Some("mycli --password=*** login".to_string())
        );
        assert!(contains_sensitive_material(
            "mycli --password=hunter2 login"
        ));
    }

    #[test]
    fn long_flag_password_space_is_redacted() {
        assert_eq!(
            sanitize_command("mycli --password hunter2 login"),
            Some("mycli --password *** login".to_string())
        );
    }

    #[test]
    fn long_flag_token_secret_api_key_are_redacted() {
        assert_eq!(
            sanitize_command("tool --token=abc123"),
            Some("tool --token=***".to_string())
        );
        assert_eq!(
            sanitize_command("tool --secret=abc123"),
            Some("tool --secret=***".to_string())
        );
        assert_eq!(
            sanitize_command("tool --api-key=abc123"),
            Some("tool --api-key=***".to_string())
        );
        assert_eq!(
            sanitize_command("tool --api_key abc123"),
            Some("tool --api_key ***".to_string())
        );
    }

    #[test]
    fn export_aws_secret_access_key_is_redacted() {
        assert_eq!(
            sanitize_command("export AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI"),
            Some("export AWS_SECRET_ACCESS_KEY=***".to_string())
        );
    }

    #[test]
    fn env_var_assignment_with_secretish_name_is_redacted() {
        assert_eq!(
            sanitize_command("MY_API_KEY=abc123 ./run.sh"),
            Some("MY_API_KEY=*** ./run.sh".to_string())
        );
        assert_eq!(
            sanitize_command("DB_PASSWORD=hunter2 psql"),
            Some("DB_PASSWORD=*** psql".to_string())
        );
        assert_eq!(
            sanitize_command("GITHUB_TOKEN=ghp_abc git push"),
            Some("GITHUB_TOKEN=*** git push".to_string())
        );
        // Case-insensitive name matching.
        assert_eq!(
            sanitize_command("my_passwd=x do-thing"),
            Some("my_passwd=*** do-thing".to_string())
        );
        assert_eq!(
            sanitize_command("private_key=x do-thing"),
            Some("private_key=*** do-thing".to_string())
        );
    }

    #[test]
    fn printenv_assignment_is_redacted() {
        assert_eq!(
            sanitize_command("printenv MY_TOKEN=abc"),
            Some("printenv MY_TOKEN=***".to_string())
        );
    }

    #[test]
    fn pem_private_key_drops_the_line() {
        let line = "cat -----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBg==\n-----END PRIVATE KEY-----";
        assert_eq!(sanitize_command(line), None);
        assert!(contains_sensitive_material(line));

        let rsa = "-----BEGIN RSA PRIVATE KEY-----\nMIIEow==";
        assert_eq!(sanitize_command(rsa), None);

        let openssh = "-----BEGIN OPENSSH PRIVATE KEY-----";
        assert_eq!(sanitize_command(openssh), None);
    }

    #[test]
    fn aws_access_key_id_is_redacted() {
        assert_eq!(
            sanitize_command("aws configure set aws_access_key_id AKIAIOSFODNN7EXAMPLE"),
            Some("aws configure set aws_access_key_id ***".to_string())
        );
        assert!(contains_sensitive_material(
            "aws configure set aws_access_key_id AKIAIOSFODNN7EXAMPLE"
        ));
        // Standalone AKIA key still redacted.
        assert_eq!(
            sanitize_command("echo AKIAIOSFODNN7EXAMPLE"),
            Some("echo ***".to_string())
        );
    }

    #[test]
    fn authorization_bearer_header_is_redacted() {
        assert_eq!(
            sanitize_command("curl -H 'Authorization: Bearer abc.def.ghi' https://api.example.com"),
            Some("curl -H 'Authorization: ***' https://api.example.com".to_string())
        );
        assert_eq!(
            sanitize_command("curl -H \"Authorization: token ghp_abc123\" https://api.github.com"),
            Some("curl -H \"Authorization: ***\" https://api.github.com".to_string())
        );
        assert!(contains_sensitive_material(
            "curl -H 'Authorization: Bearer abc.def.ghi' https://example.com"
        ));
    }

    #[test]
    fn ssh_public_keys_are_not_secret() {
        let rsa_pub = "echo ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABgQC7 user@host";
        assert_eq!(sanitize_command(rsa_pub), Some(rsa_pub.to_string()));
        assert!(!contains_sensitive_material(rsa_pub));

        let ed_pub = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIL user@host";
        assert_eq!(sanitize_command(ed_pub), Some(ed_pub.to_string()));
    }

    #[test]
    fn long_hex_token_arg_is_redacted() {
        assert_eq!(
            sanitize_command("curl https://x/?t=0123456789abcdef0123456789abcdef"),
            Some("curl https://x/?t=***".to_string())
        );
        assert!(contains_sensitive_material(
            "curl https://x/?t=0123456789abcdef0123456789abcdef"
        ));
    }

    #[test]
    fn short_hex_like_strings_are_not_redacted() {
        // Deadbeef is only 8 chars — must not trigger the 32+ hex rule.
        assert_eq!(
            sanitize_command("echo deadbeef"),
            Some("echo deadbeef".to_string())
        );
        assert!(!contains_sensitive_material("echo deadbeef"));
    }

    #[test]
    fn long_base64url_token_arg_is_redacted() {
        let tok = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"; // 44 chars
        let line = format!("curl -H X-Custom: {}", tok);
        assert_eq!(
            sanitize_command(&line),
            Some("curl -H X-Custom: ***".to_string())
        );
    }
}
