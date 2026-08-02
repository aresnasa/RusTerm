//! Local command typo correction.
//!
//! Only the executable token is compared. Suggestions replace the current
//! input line and are never executed automatically.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Minimum number of local observations before a learned-only correction is
/// shown. Static well-known commands remain available immediately.
pub const MIN_LEARNED_OBSERVATIONS: u64 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorrectionCandidate {
    pub command: String,
    pub observations: u64,
    pub learned: bool,
}

const COMMON_EXECUTABLES: &[&str] = &[
    "ansible",
    "ansible-playbook",
    "awk",
    "bun",
    "cargo",
    "cat",
    "chmod",
    "chown",
    "cmake",
    "curl",
    "dig",
    "docker",
    "find",
    "git",
    "go",
    "gradle",
    "grep",
    "gzip",
    "helm",
    "htop",
    "java",
    "javac",
    "journalctl",
    "kubectl",
    "less",
    "make",
    "mkdir",
    "mvn",
    "node",
    "npm",
    "nslookup",
    "ping",
    "pip",
    "pip3",
    "pnpm",
    "podman",
    "python",
    "python3",
    "rsync",
    "rustc",
    "scp",
    "sed",
    "ssh",
    "systemctl",
    "tail",
    "tar",
    "terraform",
    "traceroute",
    "unzip",
    "wget",
    "yarn",
];

/// Suggest low-risk corrections for `input`, combining the built-in command
/// vocabulary with locally learned `(correction, observations)` rows.
pub fn suggest_corrections(input: &str, learned: &[(String, u64)]) -> Vec<CorrectionCandidate> {
    let Some((executable, suffix)) = split_command(input) else {
        return Vec::new();
    };
    if !safe_executable_token(executable) {
        return Vec::new();
    }

    let mut candidates = std::collections::HashMap::<String, CorrectionCandidate>::new();
    for candidate in COMMON_EXECUTABLES {
        if executable != *candidate && damerau_levenshtein(executable, candidate) == 1 {
            let command = format!("{candidate}{suffix}");
            candidates.insert(
                command.clone(),
                CorrectionCandidate {
                    command,
                    observations: 0,
                    learned: false,
                },
            );
        }
    }

    for (correction, observations) in learned {
        let Some((corrected_executable, corrected_suffix)) = split_command(correction) else {
            continue;
        };
        let is_static = COMMON_EXECUTABLES.contains(&corrected_executable);
        if corrected_suffix
            .split_whitespace()
            .eq(suffix.split_whitespace())
            && executable != corrected_executable
            && damerau_levenshtein(executable, corrected_executable) == 1
            && (*observations >= MIN_LEARNED_OBSERVATIONS || is_static)
        {
            candidates
                .entry(correction.clone())
                .and_modify(|candidate| {
                    candidate.observations = candidate.observations.max(*observations);
                    candidate.learned = true;
                })
                .or_insert_with(|| CorrectionCandidate {
                    command: correction.clone(),
                    observations: *observations,
                    learned: true,
                });
        }
    }

    let mut candidates = candidates.into_values().collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        right
            .observations
            .cmp(&left.observations)
            .then_with(|| left.command.cmp(&right.command))
    });
    candidates
}

pub fn remember_failed_command(
    failures: &mut HashMap<String, (String, Instant)>,
    session_id: &str,
    command: &str,
) {
    failures.insert(
        session_id.to_string(),
        (command.to_string(), Instant::now()),
    );
}

/// Consume the pending failure for this session and return its command only
/// when the successful command is a timely, high-confidence correction.
pub fn take_correction_for_success(
    failures: &mut HashMap<String, (String, Instant)>,
    session_id: &str,
    successful: &str,
) -> Option<String> {
    failures.remove(session_id).and_then(|(failed, failed_at)| {
        (failed_at.elapsed() <= Duration::from_secs(120)
            && is_likely_correction(&failed, successful))
        .then_some(failed)
    })
}

/// Bytes sent to the PTY when the user accepts a correction. DEL removes the
/// currently typed line and the replacement is inserted without Enter.
pub fn replacement_input(current: &str, correction: &str) -> Vec<u8> {
    let mut input = vec![0x7f; current.chars().count()];
    input.extend_from_slice(correction.as_bytes());
    input
}

/// Return true when a failed command followed by a successful command is a
/// strong correction signal: same arguments and a one-edit executable typo.
pub fn is_likely_correction(failed: &str, successful: &str) -> bool {
    let (
        Some((failed_executable, failed_suffix)),
        Some((successful_executable, successful_suffix)),
    ) = (split_command(failed), split_command(successful))
    else {
        return false;
    };

    safe_executable_token(failed_executable)
        && safe_executable_token(successful_executable)
        && failed_executable != successful_executable
        && failed_suffix
            .split_whitespace()
            .eq(successful_suffix.split_whitespace())
        && damerau_levenshtein(failed_executable, successful_executable) == 1
}

fn split_command(command: &str) -> Option<(&str, &str)> {
    let command = command.trim();
    if command.is_empty() {
        return None;
    }
    let executable_end = command.find(char::is_whitespace).unwrap_or(command.len());
    Some((&command[..executable_end], &command[executable_end..]))
}

fn safe_executable_token(token: &str) -> bool {
    token.len() >= 2
        && !token.chars().any(|character| {
            matches!(
                character,
                '/' | '\\' | '~' | '$' | '|' | '&' | ';' | '<' | '>' | '"' | '\''
            )
        })
}

/// Optimal-string-alignment Damerau-Levenshtein distance. Adjacent key swaps
/// count as one edit, which is essential for `dockre` → `docker`.
fn damerau_levenshtein(left: &str, right: &str) -> usize {
    let left = left.chars().collect::<Vec<_>>();
    let right = right.chars().collect::<Vec<_>>();
    let mut matrix = vec![vec![0usize; right.len() + 1]; left.len() + 1];

    for (index, row) in matrix.iter_mut().enumerate() {
        row[0] = index;
    }
    for index in 0..=right.len() {
        matrix[0][index] = index;
    }

    for i in 1..=left.len() {
        for j in 1..=right.len() {
            let substitution = usize::from(left[i - 1] != right[j - 1]);
            matrix[i][j] = (matrix[i - 1][j] + 1)
                .min(matrix[i][j - 1] + 1)
                .min(matrix[i - 1][j - 1] + substitution);
            if i > 1 && j > 1 && left[i - 1] == right[j - 2] && left[i - 2] == right[j - 1] {
                matrix[i][j] = matrix[i][j].min(matrix[i - 2][j - 2] + 1);
            }
        }
    }

    matrix[left.len()][right.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corrects_adjacent_transpositions_and_preserves_arguments() {
        let docker = suggest_corrections("dockre ps -a", &[]);
        assert_eq!(docker[0].command, "docker ps -a");

        let git = suggest_corrections("gti status", &[]);
        assert_eq!(git[0].command, "git status");
    }

    #[test]
    fn does_not_suggest_for_correct_distant_or_path_commands() {
        assert!(suggest_corrections("docker ps", &[]).is_empty());
        assert!(suggest_corrections("dockzzz ps", &[]).is_empty());
        assert!(suggest_corrections("/usr/bin/dockre ps", &[]).is_empty());
        assert!(suggest_corrections("echo ok | dockre ps", &[]).is_empty());
        assert!(suggest_corrections("", &[]).is_empty());
    }

    #[test]
    fn learned_candidates_require_repetition_and_rank_by_observations() {
        let learned = vec![("gt status".to_string(), 4), ("git status".to_string(), 1)];
        let candidates = suggest_corrections("gti status", &learned);

        assert_eq!(candidates[0].command, "gt status");
        assert_eq!(candidates[0].observations, 4);
        assert!(candidates[0].learned);
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.command == "git status")
        );
    }

    #[test]
    fn failed_command_learning_stays_within_the_same_session() {
        let mut failures = HashMap::new();
        remember_failed_command(&mut failures, "session-a", "dockre ps");

        assert_eq!(
            take_correction_for_success(&mut failures, "session-b", "docker ps"),
            None
        );
        assert_eq!(
            take_correction_for_success(&mut failures, "session-a", "docker ps"),
            Some("dockre ps".to_string())
        );
    }

    #[test]
    fn unrelated_success_does_not_learn_and_consumes_pending_failure() {
        let mut failures = HashMap::new();
        remember_failed_command(&mut failures, "session-a", "dockre ps");
        assert_eq!(
            take_correction_for_success(&mut failures, "session-a", "git status"),
            None
        );
        assert!(!failures.contains_key("session-a"));
    }

    #[test]
    fn newer_failure_replaces_older_failure_in_the_same_session() {
        let mut failures = HashMap::new();
        remember_failed_command(&mut failures, "session-a", "gti status");
        remember_failed_command(&mut failures, "session-a", "dockre ps");
        assert_eq!(
            take_correction_for_success(&mut failures, "session-a", "docker ps"),
            Some("dockre ps".to_string())
        );
    }

    #[test]
    fn accepted_correction_replaces_without_executing() {
        let input = replacement_input("dockre ps", "docker ps");
        assert_eq!(&input[..9], &[0x7f; 9]);
        assert_eq!(&input[9..], b"docker ps");
        assert!(!input.contains(&b'\r'));
        assert!(!input.contains(&b'\n'));
    }

    #[test]
    fn learning_signal_is_session_safe_when_applied_by_the_caller() {
        assert!(is_likely_correction("dockre ps", "docker ps"));
        assert!(is_likely_correction("gti status", "git status"));
        assert!(!is_likely_correction("dockre ps", "docker images"));
        assert!(!is_likely_correction("dockre ps", "git status"));
        assert!(!is_likely_correction("docker ps", "docker ps"));
    }
}
