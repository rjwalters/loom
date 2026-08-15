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
        let commit = String::from_utf8(output.stdout).ok()?.trim().to_string();
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
        .is_ok_and(|o| o.status.success())
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
        || lower.starts_with("test/") // relative test path
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

/// Local `git diff --numstat` between two resolved refs, returning
/// (lines_added, lines_deleted) — the two-field shape [`SweepOutcomeRecord`]
/// wants (Issue #5357). Thin wrapper over [`capture_git_changes`]: same
/// numstat parsing, just a narrower return type for a caller that only wants
/// the two aggregate counts, not the full [`GitDiffStats`].
///
/// `None` when `working_dir` is not a git repo or the diff invocation fails.
#[must_use]
fn diff_stat_between(working_dir: &Path, before: &str, after: &str) -> Option<(i64, i64)> {
    let stats = capture_git_changes(working_dir, Some(before), Some(after))?;
    Some((i64::from(stats.lines_added), i64::from(stats.lines_removed)))
}

/// Local lines-added/lines-removed for `working_dir`'s current branch against
/// `base_ref`'s common ancestor with `HEAD` (Issue #5357), via `git
/// merge-base` and `git diff --numstat` — never a forge API call. This is
/// the same mechanism [`capture_git_changes`] already provides for
/// prompt-level tracking, reused here for a whole sweep's own worktree.
///
/// `None` when `working_dir` is not a git repo, `base_ref` cannot be
/// resolved to a common ancestor with `HEAD` (unknown ref, shallow clone,
/// no shared history), or the diff itself fails — never a fabricated zero
/// for an unprobeable worktree.
#[must_use]
pub fn diff_stat_since_merge_base(working_dir: &Path, base_ref: &str) -> Option<(i64, i64)> {
    if !is_git_repo(working_dir) {
        return None;
    }
    let merge_base_output = Command::new("git")
        .args(["merge-base", "HEAD", base_ref])
        .current_dir(working_dir)
        .output()
        .ok()?;
    if !merge_base_output.status.success() {
        return None;
    }
    let merge_base = String::from_utf8(merge_base_output.stdout)
        .ok()?
        .trim()
        .to_string();
    if merge_base.is_empty() {
        return None;
    }
    diff_stat_between(working_dir, &merge_base, "HEAD")
}

/// Refs likely to name `repo`'s mainline, in priority order (Issue #5357):
/// `origin/<default_branch>` when `origin/HEAD` resolves (the actual base a
/// Loom worktree branched from — see `worktree.sh`'s `loom_default_branch`),
/// then the common hardcoded names as a fallback for a repo with no
/// configured remote (a bare local clone, or this module's own tests) so the
/// LOC probe degrades gracefully rather than requiring `git remote
/// set-head`. Order matters: the first candidate `git merge-base` can
/// resolve against `HEAD` wins.
fn mainline_candidates(repo: &Path) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(branch) = crate::worktree_ops::clean::default_branch(repo) {
        out.push(format!("origin/{branch}"));
    }
    for candidate in ["origin/main", "origin/master", "main", "master"] {
        if !out.iter().any(|c| c == candidate) {
            out.push(candidate.to_string());
        }
    }
    out
}

/// Local lines-added/lines-removed for `working_dir`'s current branch
/// against its mainline's merge base (Issue #5357) — never a forge API call.
/// Tries [`mainline_candidates`] in order and uses the first ref `git
/// merge-base` can resolve against `HEAD`.
///
/// `None` when `working_dir` is not a git repo or no candidate ref resolves
/// (e.g. a shallow clone with no shared history) — never a fabricated zero
/// for an unprobeable worktree. This is the entry point sweep-outcome
/// telemetry capture calls; [`diff_stat_since_merge_base`] is exposed
/// separately for a caller that already knows which ref to diff against.
#[must_use]
pub fn diff_stat_against_mainline(working_dir: &Path) -> Option<(i64, i64)> {
    if !is_git_repo(working_dir) {
        return None;
    }
    mainline_candidates(working_dir)
        .into_iter()
        .find_map(|base_ref| diff_stat_since_merge_base(working_dir, &base_ref))
}

/// Capture git changes and create a `PromptChanges` record
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
        capture_git_changes(working_dir, before_commit.as_deref(), after_commit.as_deref())?
    };

    // Only create a record if there were actual changes
    if stats.files_changed == 0 && stats.lines_added == 0 && stats.lines_removed == 0 {
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
    use serial_test::parallel;

    // --- diff_stat_since_merge_base / diff_stat_against_mainline (#5357) ---
    //
    // Issue #6177: these tests were observed flaking under `cargo test`'s
    // default parallel-thread execution (2 then 3 failures on the *same*
    // commit across two consecutive full-suite runs), always passing in
    // isolation. The shared resource is not anything in this module — every
    // `git` invocation below already pins its own `current_dir` per-`Command`
    // — it is the **process-global environment** (`std::env::set_var` /
    // `remove_var`), which dozens of tests elsewhere in this crate's single
    // test binary mutate concurrently (most heavily `LOOM_CONFIG_DEFAULTS_FILE`,
    // serialized crate-wide under the `loom_config_env` `#[serial]` key —
    // see `config_resolver.rs`). Concurrently forking a subprocess
    // (`Command::spawn`, which every test here does, some several times) while
    // another thread calls `std::env::set_var`/`remove_var` is a documented
    // Rust unsoundness (std's own `env::set_var` docs, rust-lang/rust#27970):
    // `environ` can be read mid-mutation by the fork, corrupting what the
    // child process inherits. `#[parallel(loom_config_env)]` below is
    // `serial_test`'s documented mechanism for exactly this: it lets these
    // tests keep running in parallel with each other and with ordinary tests,
    // while guaranteeing none of them ever overlaps a `loom_config_env`
    // `#[serial]` test (the single largest env-mutation lock domain in this
    // crate) — closing the race window instead of just hiding the symptom by
    // serializing this module against itself.
    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?} failed in {}", dir.display());
    }

    /// A repo with `main` at one commit and a `feature` branch (checked out)
    /// carrying one additional commit that adds `new.txt` (3 lines) and
    /// appends one line to `README.md` — no `origin` remote configured, the
    /// common shape for a bare local test fixture (and for a Loom worktree
    /// whose remote fetch failed).
    fn feature_branch_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q", "--initial-branch=main"]);
        git(dir.path(), &["config", "user.email", "loom@example.com"]);
        git(dir.path(), &["config", "user.name", "Loom Test"]);
        std::fs::write(dir.path().join("README.md"), "line1\n").unwrap();
        git(dir.path(), &["add", "-A"]);
        git(dir.path(), &["commit", "-q", "-m", "seed"]);
        git(dir.path(), &["checkout", "-q", "-b", "feature"]);
        std::fs::write(dir.path().join("README.md"), "line1\nline2\n").unwrap();
        std::fs::write(dir.path().join("new.txt"), "a\nb\nc\n").unwrap();
        git(dir.path(), &["add", "-A"]);
        git(dir.path(), &["commit", "-q", "-m", "feature work"]);
        dir
    }

    #[test]
    #[parallel(loom_config_env)]
    fn diff_stat_since_merge_base_counts_added_and_removed_lines() {
        let dir = feature_branch_repo();
        let stats = diff_stat_since_merge_base(dir.path(), "main")
            .expect("merge-base against a real sibling branch must resolve");
        // new.txt: +3, README.md: +1 (append-only, nothing removed).
        assert_eq!(stats, (4, 0));
    }

    #[test]
    #[parallel(loom_config_env)]
    fn diff_stat_since_merge_base_is_none_for_an_unresolvable_ref() {
        let dir = feature_branch_repo();
        assert_eq!(diff_stat_since_merge_base(dir.path(), "origin/main"), None);
        assert_eq!(diff_stat_since_merge_base(dir.path(), "not-a-real-ref"), None);
    }

    #[test]
    #[parallel(loom_config_env)]
    fn diff_stat_since_merge_base_is_none_outside_a_git_repo() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(diff_stat_since_merge_base(dir.path(), "main"), None);
    }

    #[test]
    #[parallel(loom_config_env)]
    fn diff_stat_against_mainline_falls_back_to_bare_branch_names_with_no_remote() {
        let dir = feature_branch_repo();
        // No `origin` remote at all, so `default_branch` resolves to `None`
        // and every `origin/*` candidate fails to resolve — the fallback to
        // the bare `main` candidate must still find the same diff.
        let stats =
            diff_stat_against_mainline(dir.path()).expect("bare `main` fallback must resolve");
        assert_eq!(stats, (4, 0));
    }

    #[test]
    #[parallel(loom_config_env)]
    fn diff_stat_against_mainline_is_none_when_head_equals_mainline() {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q", "--initial-branch=main"]);
        git(dir.path(), &["config", "user.email", "loom@example.com"]);
        git(dir.path(), &["config", "user.name", "Loom Test"]);
        git(dir.path(), &["commit", "-q", "--allow-empty", "-m", "seed"]);
        // HEAD *is* main, so the merge-base diff is empty — zero lines
        // changed, a legitimate `Some((0, 0))`, not an unprobeable `None`.
        assert_eq!(diff_stat_against_mainline(dir.path()), Some((0, 0)));
    }

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
        assert!(!is_test_file("latest.js")); // Contains "test" but not a test file
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
