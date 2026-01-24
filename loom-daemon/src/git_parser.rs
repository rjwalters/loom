//! Git commit output parser for detecting commits from terminal output.
//!
//! This module parses terminal output to detect when agents make git commits,
//! enabling automatic correlation of prompts with code changes.
//!
//! # Example
//!
//! ```ignore
//! use git_parser::parse_git_commits;
//!
//! let output = "[main abc1234] Add new feature\n 3 files changed, 42 insertions(+), 5 deletions(-)";
//! let commits = parse_git_commits(output);
//! // Returns: [GitCommitEvent { commit_hash: "abc1234", message: "Add new feature", ... }]
//! ```

use regex::Regex;

/// A parsed git commit event from terminal output
#[derive(Debug, Clone)]
pub struct GitCommitEvent {
    /// The commit hash (short form, typically 7 characters)
    pub commit_hash: String,
    /// The commit message (first line)
    pub commit_message: Option<String>,
    /// The branch name where the commit was made
    #[allow(dead_code)]
    pub branch: Option<String>,
    /// Files changed (if available from git output)
    pub files_changed: Option<i32>,
    /// Lines added (insertions)
    pub lines_added: Option<i32>,
    /// Lines removed (deletions)
    pub lines_removed: Option<i32>,
}

/// Parse terminal output for git commit events
///
/// Detects patterns from `git commit` output such as:
/// - `[main abc1234] Commit message here`
/// - `[feature/issue-42 def5678] Another commit`
/// - Summary line: `3 files changed, 42 insertions(+), 5 deletions(-)`
///
/// Also detects:
/// - `create mode` lines (new files)
/// - Commit hash from various git outputs
pub fn parse_git_commits(output: &str) -> Vec<GitCommitEvent> {
    let mut events = Vec::new();

    // Pattern: [branch hash] message
    // Matches: [main abc1234] Commit message
    // Matches: [feature/issue-42 1234567890abcdef] Multi word message
    if let Ok(commit_re) = Regex::new(r"\[([^\]\s]+)\s+([a-f0-9]{7,40})\]\s+(.+)") {
        for cap in commit_re.captures_iter(output) {
            if let (Some(branch_match), Some(hash_match), Some(msg_match)) =
                (cap.get(1), cap.get(2), cap.get(3))
            {
                let mut event = GitCommitEvent {
                    commit_hash: hash_match.as_str().to_string(),
                    commit_message: Some(msg_match.as_str().trim().to_string()),
                    branch: Some(branch_match.as_str().to_string()),
                    files_changed: None,
                    lines_added: None,
                    lines_removed: None,
                };

                // Try to extract stats from nearby lines
                if let Some(stats) = extract_commit_stats(output) {
                    event.files_changed = stats.0;
                    event.lines_added = stats.1;
                    event.lines_removed = stats.2;
                }

                events.push(event);
            }
        }
    }

    // Pattern: commit hash on its own line (from git log or git show)
    // This helps detect commits even when the main pattern doesn't match
    // But only if we haven't already captured it
    if events.is_empty() {
        if let Ok(hash_re) = Regex::new(r"(?m)^commit\s+([a-f0-9]{40})") {
            for cap in hash_re.captures_iter(output) {
                if let Some(hash_match) = cap.get(1) {
                    let hash = hash_match.as_str();
                    // Use short hash for consistency
                    let short_hash = &hash[..7.min(hash.len())];

                    events.push(GitCommitEvent {
                        commit_hash: short_hash.to_string(),
                        commit_message: None,
                        branch: None,
                        files_changed: None,
                        lines_added: None,
                        lines_removed: None,
                    });
                }
            }
        }
    }

    events
}

/// Extract commit statistics from git output
/// Returns (`files_changed`, `insertions`, `deletions`)
fn extract_commit_stats(output: &str) -> Option<(Option<i32>, Option<i32>, Option<i32>)> {
    // Pattern: N file(s) changed, N insertion(s)(+), N deletion(s)(-)
    // Also handles: N file(s) changed, N insertion(s)(+)
    // And: N file(s) changed, N deletion(s)(-)
    let stats_re = Regex::new(
        r"(\d+)\s+file[s]?\s+changed(?:,\s+(\d+)\s+insertion[s]?\(\+\))?(?:,\s+(\d+)\s+deletion[s]?\(-\))?",
    ).ok()?;

    if let Some(cap) = stats_re.captures(output) {
        let files = cap.get(1).and_then(|m| m.as_str().parse().ok());
        let insertions = cap.get(2).and_then(|m| m.as_str().parse().ok());
        let deletions = cap.get(3).and_then(|m| m.as_str().parse().ok());

        return Some((files, insertions, deletions));
    }

    None
}

/// Check if output contains evidence of a git commit operation
///
/// This is a lighter-weight check than full parsing, useful for
/// determining if we should trigger git change capture.
pub fn contains_git_commit(output: &str) -> bool {
    // Check for common git commit output patterns
    let patterns = [
        // Standard commit output: [branch hash] message
        r"\[[^\]\s]+\s+[a-f0-9]{7,40}\]",
        // Files changed summary
        r"\d+\s+file[s]?\s+changed",
        // Create mode (new file added in commit)
        r"create mode \d+",
        // git commit -m output
        r"Author:.*<.*@.*>",
    ];

    for pattern in &patterns {
        if let Ok(re) = Regex::new(pattern) {
            if re.is_match(output) {
                return true;
            }
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_standard_commit() {
        let output = r#"[main abc1234] Add new feature
 3 files changed, 42 insertions(+), 5 deletions(-)"#;

        let events = parse_git_commits(output);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].commit_hash, "abc1234");
        assert_eq!(events[0].commit_message, Some("Add new feature".to_string()));
        assert_eq!(events[0].branch, Some("main".to_string()));
        assert_eq!(events[0].files_changed, Some(3));
        assert_eq!(events[0].lines_added, Some(42));
        assert_eq!(events[0].lines_removed, Some(5));
    }

    #[test]
    fn test_parse_feature_branch_commit() {
        let output = "[feature/issue-42 def5678] Implement user authentication";

        let events = parse_git_commits(output);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].commit_hash, "def5678");
        assert_eq!(
            events[0].commit_message,
            Some("Implement user authentication".to_string())
        );
        assert_eq!(events[0].branch, Some("feature/issue-42".to_string()));
    }

    #[test]
    fn test_parse_long_hash() {
        let output = "[main 1234567890abcdef1234567890abcdef12345678] Full hash commit";

        let events = parse_git_commits(output);
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].commit_hash,
            "1234567890abcdef1234567890abcdef12345678"
        );
    }

    #[test]
    fn test_parse_commit_with_only_insertions() {
        let output = r#"[main abc1234] New file only
 1 file changed, 100 insertions(+)"#;

        let events = parse_git_commits(output);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].files_changed, Some(1));
        assert_eq!(events[0].lines_added, Some(100));
        assert_eq!(events[0].lines_removed, None);
    }

    #[test]
    fn test_parse_commit_with_only_deletions() {
        let output = r#"[main abc1234] Remove old code
 2 files changed, 50 deletions(-)"#;

        let events = parse_git_commits(output);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].files_changed, Some(2));
        assert_eq!(events[0].lines_added, None);
        assert_eq!(events[0].lines_removed, Some(50));
    }

    #[test]
    fn test_parse_git_log_output() {
        let output = r#"commit 1234567890abcdef1234567890abcdef12345678
Author: Test User <test@example.com>
Date:   Thu Jan 23 10:00:00 2025 -0800

    Commit message here"#;

        let events = parse_git_commits(output);
        assert_eq!(events.len(), 1);
        // Should extract short hash
        assert_eq!(events[0].commit_hash, "1234567");
    }

    #[test]
    fn test_contains_git_commit_true() {
        let outputs = [
            "[main abc1234] Some commit",
            "3 files changed, 10 insertions(+)",
            "create mode 100644 new_file.rs",
            "Author: Test <test@example.com>",
        ];

        for output in &outputs {
            assert!(
                contains_git_commit(output),
                "Should detect commit in: {output}"
            );
        }
    }

    #[test]
    fn test_contains_git_commit_false() {
        let outputs = [
            "Compiling loom-daemon v0.1.0",
            "cargo test passed",
            "ls -la output here",
            "No commits found",
        ];

        for output in &outputs {
            assert!(
                !contains_git_commit(output),
                "Should not detect commit in: {output}"
            );
        }
    }

    #[test]
    fn test_no_false_positives() {
        let output = "Running tests...\nAll 42 tests passed";
        let events = parse_git_commits(output);
        assert!(events.is_empty());
    }

    #[test]
    fn test_multiple_commits_in_output() {
        let output = r#"[main abc1234] First commit
 1 file changed, 10 insertions(+)
[main def5678] Second commit
 2 files changed, 20 insertions(+), 5 deletions(-)"#;

        let events = parse_git_commits(output);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].commit_hash, "abc1234");
        assert_eq!(events[1].commit_hash, "def5678");
    }
}
