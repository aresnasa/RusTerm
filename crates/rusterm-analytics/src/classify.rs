//! Command classification — bucket commands into categories by prefix.
//!
//! Used by `AnalyticsDB::classify()` to answer "what does the user actually
//! do all day?" — the analytics panel shows a pie/bar chart of category counts.
//!
//! The rules are intentionally simple prefix matches on the first whitespace-
//! delimited token of the command. This avoids pulling in a parser for every
//! shell out there, and is fast enough to run over 100k commands in <10ms.

use serde::{Deserialize, Serialize};

use crate::CategoryCount;

/// Coarse-grained command category. Ordered roughly by what a developer
/// typically does most: version control, containers, orchestration, builds,
/// file ops, networking, etc.
///
/// Variants are intentionally limited (~15) so the UI chart is legible.
/// Commands that don't match any prefix land in `Other`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CommandCategory {
    /// `git ...` — version control
    Git,
    /// `docker ...`, `docker-compose ...` — containers
    Docker,
    /// `kubectl ...`, `helm ...`, `k9s ...` — Kubernetes orchestration
    Kubernetes,
    /// `cargo ...`, `rustc ...`, `rustup ...` — Rust toolchain
    Rust,
    /// `npm ...`, `yarn ...`, `pnpm ...`, `node ...` — JS toolchain
    NodeJs,
    /// `python ...`, `pip ...`, `poetry ...`, `uv ...` — Python toolchain
    Python,
    /// `go ...` — Go toolchain
    Go,
    /// `make ...`, `cmake ...` — build systems
    Build,
    /// `ls`, `cd`, `pwd`, `cp`, `mv`, `rm`, `mkdir`, `touch`, `find`,
    /// `tree` — file operations
    FileOps,
    /// `cat`, `less`, `more`, `head`, `tail`, `grep`, `awk`, `sed`, `cut`,
    /// `sort`, `uniq`, `wc`, `jq`, `xargs` — text processing
    TextProcessing,
    /// `ssh ...`, `scp ...`, `rsync ...`, `curl ...`, `wget ...` — networking
    Networking,
    /// `ps`, `top`, `htop`, `kill`, `killall`, `systemctl`, `service` —
    /// process / service management
    Process,
    /// `vim`, `vi`, `nano`, `emacs`, `code`, `hx` — editors
    Editor,
    /// `cd` alone (frequent enough to bucket separately so it doesn't
    /// dominate FileOps)
    Navigation,
    /// Anything else
    Other,
}

impl CommandCategory {
    /// Human-readable label for the UI chart.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Git => "Git",
            Self::Docker => "Docker",
            Self::Kubernetes => "Kubernetes",
            Self::Rust => "Rust",
            Self::NodeJs => "Node.js",
            Self::Python => "Python",
            Self::Go => "Go",
            Self::Build => "Build",
            Self::FileOps => "File ops",
            Self::TextProcessing => "Text",
            Self::Networking => "Networking",
            Self::Process => "Process",
            Self::Editor => "Editor",
            Self::Navigation => "Navigation",
            Self::Other => "Other",
        }
    }
}

impl std::fmt::Display for CommandCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// Classify a single command into a category by its first whitespace-delimited
/// token. Empty / whitespace-only commands land in `Other`.
///
/// Extracted as a free function (not a method on `AnalyticsCommand`) so it's
/// unit-testable without constructing a full `AnalyticsCommand`.
pub fn classify_command(command: &str) -> CommandCategory {
    let trimmed = command.trim_start();
    if trimmed.is_empty() {
        return CommandCategory::Other;
    }
    // Take the first whitespace-delimited token. We deliberately don't try to
    // parse shell pipes / redirects — `git log | grep foo` classifies as Git,
    // which is what the user thinks of it as.
    let first_token = trimmed
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_end_matches(':');
    // Some commands are invoked via `sudo` — look one level deeper.
    let target = if first_token == "sudo" || first_token == "time" || first_token == "nohup" {
        trimmed
            .split_whitespace()
            .nth(1)
            .unwrap_or("")
            .trim_end_matches(':')
    } else {
        first_token
    };
    // Strip any leading path component (`/usr/bin/git` → `git`).
    let basename = target.rsplit('/').next().unwrap_or(target);
    classify_basename(basename)
}

/// Inner classifier: given the basename of the first token (e.g. `git`,
/// `docker`, `ls`), return its category. Split out so we can test it
/// directly without worrying about `sudo`/path stripping.
fn classify_basename(basename: &str) -> CommandCategory {
    match basename {
        "git" => CommandCategory::Git,
        "docker" | "docker-compose" | "podman" | "buildah" => CommandCategory::Docker,
        "kubectl" | "helm" | "k9s" | "kubectx" | "kubens" | "minikube" | "kind" => {
            CommandCategory::Kubernetes
        }
        "cargo" | "rustc" | "rustup" | "rustfmt" => CommandCategory::Rust,
        "npm" | "yarn" | "pnpm" | "npx" | "node" | "bun" | "deno" | "tsc" | "tsx" => {
            CommandCategory::NodeJs
        }
        "python" | "python3" | "pip" | "pip3" | "poetry" | "pipenv" | "uv" | "conda" => {
            CommandCategory::Python
        }
        "go" => CommandCategory::Go,
        "make" | "cmake" | "ninja" | "meson" | "bazel" | "buck2" => CommandCategory::Build,
        "ls" | "ll" | "cp" | "mv" | "rm" | "mkdir" | "touch" | "find" | "tree" | "stat"
        | "chmod" | "chown" | "ln" | "realpath" | "basename" | "dirname" => {
            CommandCategory::FileOps
        }
        "cat" | "less" | "more" | "head" | "tail" | "grep" | "rg" | "awk" | "sed" | "cut"
        | "sort" | "uniq" | "wc" | "jq" | "yq" | "xargs" | "tr" | "tee" => {
            CommandCategory::TextProcessing
        }
        "ssh" | "scp" | "rsync" | "curl" | "wget" | "nc" | "netcat" | "telnet" | "ftp" | "sftp"
        | "dig" | "nslookup" | "host" | "ping" | "traceroute" | "mtr" => {
            CommandCategory::Networking
        }
        "ps" | "top" | "htop" | "btop" | "kill" | "killall" | "pkill" | "systemctl" | "service"
        | "journalctl" | "dmesg" | "lsof" | "fuser" => CommandCategory::Process,
        "vim" | "vi" | "nano" | "emacs" | "code" | "hx" | "helix" | "nvim" => {
            CommandCategory::Editor
        }
        "cd" => CommandCategory::Navigation,
        _ => CommandCategory::Other,
    }
}

/// Classify a list of commands and return `(category, count)` rows sorted
/// descending by count. Commands with the same category are summed.
pub fn classify_commands(commands: &[String]) -> Vec<CategoryCount> {
    use std::collections::HashMap;
    let mut counts: HashMap<CommandCategory, u64> = HashMap::new();
    for cmd in commands {
        let cat = classify_command(cmd);
        *counts.entry(cat).or_insert(0) += 1;
    }
    let mut rows: Vec<CategoryCount> = counts
        .into_iter()
        .map(|(category, count)| CategoryCount { category, count })
        .collect();
    // Sort by count descending; ties broken by category label for determinism.
    rows.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| a.category.label().cmp(b.category.label()))
    });
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_common_commands() {
        assert_eq!(classify_command("git status"), CommandCategory::Git);
        assert_eq!(classify_command("docker ps"), CommandCategory::Docker);
        assert_eq!(
            classify_command("kubectl get pods"),
            CommandCategory::Kubernetes
        );
        assert_eq!(classify_command("cargo build"), CommandCategory::Rust);
        assert_eq!(classify_command("npm install"), CommandCategory::NodeJs);
        assert_eq!(classify_command("ls -la"), CommandCategory::FileOps);
        assert_eq!(classify_command("cd /tmp"), CommandCategory::Navigation);
        assert_eq!(
            classify_command("grep foo bar"),
            CommandCategory::TextProcessing
        );
        assert_eq!(classify_command("ssh host"), CommandCategory::Networking);
        assert_eq!(classify_command("vim file.txt"), CommandCategory::Editor);
        assert_eq!(classify_command("make"), CommandCategory::Build);
    }

    #[test]
    fn strips_sudo_and_path_prefixes() {
        // sudo
        assert_eq!(
            classify_command("sudo apt update"),
            CommandCategory::Other // `apt` isn't in any bucket
        );
        assert_eq!(classify_command("sudo docker ps"), CommandCategory::Docker);
        // time
        assert_eq!(classify_command("time cargo build"), CommandCategory::Rust);
        // nohup
        assert_eq!(classify_command("nohup git pull &"), CommandCategory::Git);
        // path prefix
        assert_eq!(
            classify_command("/usr/bin/git status"),
            CommandCategory::Git
        );
        assert_eq!(
            classify_command("/usr/local/bin/docker ps"),
            CommandCategory::Docker
        );
    }

    #[test]
    fn classifies_piped_commands_by_first_token() {
        // `git log | grep foo` is classified as Git, not TextProcessing —
        // the user thinks of it as a git command.
        assert_eq!(classify_command("git log | grep foo"), CommandCategory::Git);
    }

    #[test]
    fn empty_command_is_other() {
        assert_eq!(classify_command(""), CommandCategory::Other);
        assert_eq!(classify_command("   "), CommandCategory::Other);
    }

    #[test]
    fn unknown_command_is_other() {
        assert_eq!(classify_command("foobarbaz"), CommandCategory::Other);
        assert_eq!(
            classify_command("my-custom-script --flag"),
            CommandCategory::Other
        );
    }

    #[test]
    fn classify_commands_sorts_descending_by_count() {
        let commands = vec![
            "git status".to_string(),
            "git log".to_string(),
            "git diff".to_string(),
            "docker ps".to_string(),
            "ls".to_string(),
        ];
        let counts = classify_commands(&commands);
        // Git (3) > FileOps (1) == Docker (1) — ties broken by label alpha
        assert_eq!(counts[0].category, CommandCategory::Git);
        assert_eq!(counts[0].count, 3);
        assert_eq!(counts.len(), 3, "three distinct categories");
        // Docker comes before FileOps alphabetically
        assert_eq!(counts[1].category, CommandCategory::Docker);
        assert_eq!(counts[2].category, CommandCategory::FileOps);
    }
}
