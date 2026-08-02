//! Privacy sanitizer for analytics storage.
//!
//! Hard requirement: RusTerm must NEVER collect/store/upload passwords,
//! private keys, or authentication tokens in analytics. Every command line
//! and every correction pair passes through [`sanitize_command`] before it is
//! persisted; lines dominated by secret material are dropped entirely.

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
        assert!(contains_sensitive_material("mycli --password=hunter2 login"));
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
