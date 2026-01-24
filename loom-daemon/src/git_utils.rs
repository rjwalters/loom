//! Git utilities for capturing code changes associated with prompts.
//!
//! This module provides functions for tracking git state before and after
//! agent prompts, enabling correlation of prompts with code changes.

use std::path::Path;
use std::process::Command;

use crate::activity::PromptChanges;

/// Capture the current HEAD commit hash
pub fn get_current_commit(working_dir: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(working_dir)
        .output()
        .ok()?;

    if output.status.success() {
        let commit = String::from_utf8(output.stdout)
            .ok()?
            .trim()
            .to_string();
        if commit.is_empty() {
            None
        } else {
            Some(commit)
        }
    } else {
        None
    }
}

/// Check if a directory is a git repository
pub fn is_git_repo(working_dir: &Path) -> bool {
    Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .current_dir(working_dir)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Git diff statistics parsed from `git diff --stat`
#[derive(Debug, Default)]
pub struct GitDiffStats {
    pub files_changed: i32,
    pub lines_added: i32,
    pub lines_removed: i32,
    pub tests_added: i32,
    pub tests_modified: i32,
}

/// Parse git diff --numstat output to extract change metrics
///
/// Each line of numstat output is: `additions\tdeletions\tfilename`
fn parse_numstat_output(output: &str) -> GitDiffStats {
    let mut stats = GitDiffStats::default();

    for line in output.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() >= 3 {
            // Parse additions and deletions (can be "-" for binary files)
            let additions = parts[0].parse::<i32>().unwrap_or(0);
            let deletions = parts[1].parse::<i32>().unwrap_or(0);
            let filename = parts[2];

            stats.files_changed += 1;
            stats.lines_added += additions;
            stats.lines_removed += deletions;

            // Check if this is a test file
            if is_test_file(filename) {
                if additions > deletions {
                    // More lines added than removed - likely new tests
                    stats.tests_added += 1;
                } else {
                    // Modified existing tests
                    stats.tests_modified += 1;
                }
            }
        }
    }

    stats
}

/// Check if a filename appears to be a test file
fn is_test_file(filename: &str) -> bool {
    let lower = filename.to_lowercase();

    // Extract just the filename without path
    let basename = lower.rsplit('/').next().unwrap_or(&lower);

    // Common test file patterns - be specific to avoid false positives like "contest.rs"
    basename.starts_with("test_")
        || basename.starts_with("tests_")
        || basename.ends_with("_test.rs")
        || basename.ends_with("_test.go")
        || basename.ends_with("_test.py")
        || basename.ends_with("_test.js")
        || basename.ends_with("_test.ts")
        || basename.ends_with(".test.ts")
        || basename.ends_with(".test.js")
        || basename.ends_with(".test.tsx")
        || basename.ends_with(".test.jsx")
        || basename.ends_with("_spec.rb")
        || basename.ends_with("_spec.js")
        || basename.ends_with("_spec.ts")
        || basename.ends_with(".spec.ts")
        || basename.ends_with(".spec.js")
        || basename.ends_with(".spec.tsx")
        || basename.ends_with(".spec.jsx")
        || basename.contains("_test_")  // e.g., my_test_helper.rs
        || lower.contains("/tests/")    // tests directory
        || lower.contains("/test/")     // test directory
        || lower.starts_with("tests/")  // relative tests path
        || lower.starts_with("test/")   // relative test path
}

/// Capture git changes between two commits
///
/// If `before_commit` is None, captures uncommitted changes (staged + unstaged).
/// If `after_commit` is None, uses HEAD.
pub fn capture_git_changes(
    working_dir: &Path,
    before_commit: Option<&str>,
    after_commit: Option<&str>,
) -> Option<GitDiffStats> {
    if !is_git_repo(working_dir) {
        return None;
    }

    let mut args = vec!["diff", "--numstat"];

    match (before_commit, after_commit) {
        (Some(before), Some(after)) => {
            // Diff between two specific commits
            args.push(before);
            args.push(after);
        }
        (Some(before), None) => {
            // Diff from before commit to HEAD
            args.push(before);
            args.push("HEAD");
        }
        (None, Some(after)) => {
            // Diff from empty tree to after commit (all changes in that commit)
            args.push("4b825dc642cb6eb9a060e54bf8d69288fbee4904"); // empty tree hash
            args.push(after);
        }
        (None, None) => {
            // Diff of uncommitted changes (staged + unstaged)
            args.push("HEAD");
        }
    }

    let output = Command::new("git")
        .args(&args)
        .current_dir(working_dir)
        .output()
        .ok()?;

    if output.status.success() {
        let stdout = String::from_utf8(output.stdout).ok()?;
        Some(parse_numstat_output(&stdout))
    } else {
        None
    }
}

/// Capture git changes and create a PromptChanges record
///
/// This is the main entry point for capturing git state after a prompt.
pub fn capture_prompt_changes(
    working_dir: &Path,
    input_id: i64,
    before_commit: Option<String>,
) -> Option<PromptChanges> {
    if !is_git_repo(working_dir) {
        return None;
    }

    let after_commit = get_current_commit(working_dir);

    // If commits are the same or both None, check for uncommitted changes
    let stats = if before_commit == after_commit {
        // Check for uncommitted changes
        capture_git_changes(working_dir, None, None)?
    } else {
        // Get diff between commits
        capture_git_changes(
            working_dir,
            before_commit.as_deref(),
            after_commit.as_deref(),
        )?
    };

    // Only create a record if there were actual changes
    if stats.files_changed == 0
        && stats.lines_added == 0
        && stats.lines_removed == 0
    {
        return None;
    }

    Some(PromptChanges {
        id: None,
        input_id,
        before_commit,
        after_commit,
        files_changed: stats.files_changed,
        lines_added: stats.lines_added,
        lines_removed: stats.lines_removed,
        tests_added: stats.tests_added,
        tests_modified: stats.tests_modified,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_numstat_output() {
        let output = "10\t5\tsrc/main.rs\n3\t1\tsrc/lib.rs\n";
        let stats = parse_numstat_output(output);

        assert_eq!(stats.files_changed, 2);
        assert_eq!(stats.lines_added, 13);
        assert_eq!(stats.lines_removed, 6);
    }

    #[test]
    fn test_parse_numstat_with_tests() {
        let output = "10\t5\tsrc/main.rs\n20\t2\tsrc/main_test.rs\n5\t10\ttests/integration.rs\n";
        let stats = parse_numstat_output(output);

        assert_eq!(stats.files_changed, 3);
        assert_eq!(stats.lines_added, 35);
        assert_eq!(stats.lines_removed, 17);
        assert_eq!(stats.tests_added, 1); // main_test.rs has more additions
        assert_eq!(stats.tests_modified, 1); // integration.rs has more deletions
    }

    #[test]
    fn test_parse_numstat_binary_files() {
        // Binary files show as "-" for additions/deletions
        let output = "-\t-\timage.png\n10\t5\tsrc/main.rs\n";
        let stats = parse_numstat_output(output);

        assert_eq!(stats.files_changed, 2);
        assert_eq!(stats.lines_added, 10);
        assert_eq!(stats.lines_removed, 5);
    }

    #[test]
    fn test_is_test_file() {
        // Test file patterns that should match
        assert!(is_test_file("src/main_test.rs"));
        assert!(is_test_file("tests/integration.rs"));
        assert!(is_test_file("foo.test.ts"));
        assert!(is_test_file("bar.spec.js"));
        assert!(is_test_file("test_utils.py"));
        assert!(is_test_file("src/component.test.tsx"));
        assert!(is_test_file("lib/helper_spec.rb"));

        // Non-test files that should NOT match
        assert!(!is_test_file("src/main.rs"));
        assert!(!is_test_file("lib.rs"));
        assert!(!is_test_file("contest.rs")); // Contains "test" but not a test file pattern
        assert!(!is_test_file("latest.js"));  // Contains "test" but not a test file
        assert!(!is_test_file("attestation.rs")); // Contains "test" but not a test file
    }

    #[test]
    fn test_empty_numstat() {
        let output = "";
        let stats = parse_numstat_output(output);

        assert_eq!(stats.files_changed, 0);
        assert_eq!(stats.lines_added, 0);
        assert_eq!(stats.lines_removed, 0);
    }
}
