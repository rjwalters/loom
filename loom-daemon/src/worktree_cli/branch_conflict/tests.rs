//! Unit tests for the "feature branch checked out in the main worktree"
//! recovery guard (#8195 slice 7).
//!
//! [`super::run`] takes its main-workspace root as an explicit argument (see
//! the module docs for why), so — unlike [`super::super::cleanup`]'s
//! tests — every case here can run fully in-process against throwaway repos
//! with no shared-cwd hazard.
//!
//! `tests/worktree_branch_conflict_differential.rs` compares the same
//! function against a frozen copy of the retired shell function on a
//! generated corpus; that is the equivalence evidence. This file pins the
//! rung ORDER and the path-handling edge cases directly.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use super::*;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn tmpdir(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!(
        "loom-branch-conflict-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(&base).expect("create tmpdir");
    fs::canonicalize(&base).expect("canonicalize tmpdir")
}

fn git(repo: &std::path::Path, args: &[&str]) -> std::process::Output {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

/// A one-commit repo at `<dir>/<name>`, on branch `on_branch`. `name` may
/// contain spaces (#7858's class).
fn repo(dir: &std::path::Path, name: &str, on_branch: &str) -> PathBuf {
    let repo = dir.join(name);
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", on_branch]);
    git(&repo, &["config", "user.email", "t@t"]);
    git(&repo, &["config", "user.name", "t"]);
    fs::write(repo.join("tracked.txt"), "base content\n").unwrap();
    git(&repo, &["add", "tracked.txt"]);
    git(&repo, &["commit", "-q", "-m", "base"]);
    repo
}

fn current_branch(repo: &std::path::Path) -> String {
    String::from_utf8_lossy(&git(repo, &["rev-parse", "--abbrev-ref", "HEAD"]).stdout)
        .trim()
        .to_string()
}

fn conflict_error(branch: &str, path: &std::path::Path) -> String {
    format!("fatal: '{branch}' is already used by worktree at '{}'\n", path.display())
}

fn opts(error_output: String, repo_root: PathBuf) -> Options {
    Options {
        error_output,
        branch: "feature/issue-42".to_string(),
        default_branch: "main".to_string(),
        issue: "42".to_string(),
        repo_root,
        quiet: true,
    }
}

// ---------------------------------------------------------------------------
// 1. Rung 1: is this even the error?
// ---------------------------------------------------------------------------

#[test]
fn an_unrelated_git_error_is_not_handled() {
    let root = tmpdir("unrelated-root");
    let o = opts(
        "fatal: not a git repository (or any of the parent directories): .git\n".to_string(),
        root,
    );
    assert_eq!(run(&o), 1, "an unrelated error must fall through, not be handled");
}

// ---------------------------------------------------------------------------
// 2. Rung 2: the path could not be parsed
// ---------------------------------------------------------------------------

#[test]
fn a_match_with_no_closing_quote_is_treated_as_unparseable() {
    let root = tmpdir("no-quote-root");
    let o = opts(
        "fatal: 'x' is already used by worktree at somewhere with no quotes\n".to_string(),
        root,
    );
    assert_eq!(
        run(&o),
        0,
        "an unparseable conflict path is handled generically, not passed through"
    );
}

// ---------------------------------------------------------------------------
// 3. Rung 3: conflicting worktree is NOT the main workspace
// ---------------------------------------------------------------------------

#[test]
fn a_conflict_in_a_different_worktree_is_reported_without_any_checkout() {
    let dir = tmpdir("different-worktree");
    let other = repo(&dir, "other-worktree", "feature/issue-42");
    let main_root = repo(&dir, "main-workspace", "main");

    let o = opts(conflict_error("feature/issue-42", &other), main_root.clone());
    assert_eq!(run(&o), 0);

    // Neither repo's branch changed — this rung must never touch git state.
    assert_eq!(current_branch(&other), "feature/issue-42");
    assert_eq!(current_branch(&main_root), "main");
}

// ---------------------------------------------------------------------------
// 4. Rung 4: the main workspace has uncommitted changes
// ---------------------------------------------------------------------------

#[test]
fn uncommitted_changes_in_the_main_workspace_refuse_without_switching() {
    let dir = tmpdir("dirty-main");
    let main_root = repo(&dir, "main workspace", "feature/issue-42");
    fs::write(main_root.join("tracked.txt"), "dirty\n").unwrap();

    let o = opts(conflict_error("feature/issue-42", &main_root), main_root.clone());
    assert_eq!(run(&o), 0, "a dirty main workspace must refuse, not auto-switch");
    assert_eq!(
        current_branch(&main_root),
        "feature/issue-42",
        "refusing must not have attempted a checkout"
    );
}

#[test]
fn an_untracked_file_alone_still_counts_as_uncommitted() {
    // `git status --porcelain` reports untracked files too, and the shell's
    // `[[ -n "$uncommitted" ]]` does not distinguish tracked from untracked —
    // preserved: this function is not the `.loom-managed`-marker-aware
    // dirty-check used elsewhere in this crate, it is git's own porcelain,
    // verbatim.
    let dir = tmpdir("untracked-main");
    let main_root = repo(&dir, "main-workspace", "feature/issue-42");
    fs::write(main_root.join("new-file.txt"), "surprise\n").unwrap();

    let o = opts(conflict_error("feature/issue-42", &main_root), main_root.clone());
    assert_eq!(run(&o), 0);
    assert_eq!(current_branch(&main_root), "feature/issue-42");
}

// ---------------------------------------------------------------------------
// 5. Rung 5: clean main workspace — auto-switch and signal retry
// ---------------------------------------------------------------------------

#[test]
fn a_clean_main_workspace_is_switched_to_the_default_branch_and_signals_retry() {
    let dir = tmpdir("clean-main");
    let main_root = repo(&dir, "main-workspace", "feature/issue-42");
    // The recovery target must exist as a real branch for `git checkout` to
    // land on.
    git(&main_root, &["branch", "main"]);

    let o = opts(conflict_error("feature/issue-42", &main_root), main_root.clone());
    assert_eq!(run(&o), 2, "a clean main workspace must auto-recover and signal retry");
    assert_eq!(current_branch(&main_root), "main");
}

#[test]
fn a_nonexistent_default_branch_makes_the_checkout_fail_and_reports_handled() {
    let dir = tmpdir("bad-default-branch");
    let main_root = repo(&dir, "main-workspace", "feature/issue-42");
    // No `main` branch created — `git checkout main` must fail.

    let o = opts(conflict_error("feature/issue-42", &main_root), main_root.clone());
    assert_eq!(run(&o), 0, "a failed checkout is handled (with a message), not a retry signal");
    assert_eq!(
        current_branch(&main_root),
        "feature/issue-42",
        "a failed checkout must not have moved HEAD"
    );
}

// ---------------------------------------------------------------------------
// 6. Path handling — the #7858 class, directly
// ---------------------------------------------------------------------------

#[test]
fn a_conflict_path_containing_spaces_is_matched_correctly() {
    let dir = tmpdir("space in path");
    let main_root = repo(&dir, "main workspace with spaces", "feature/issue-42");
    git(&main_root, &["branch", "main"]);

    let o = opts(conflict_error("feature/issue-42", &main_root), main_root.clone());
    assert_eq!(run(&o), 2);
    assert_eq!(current_branch(&main_root), "main");
}

#[test]
fn a_main_root_reached_only_through_a_symlink_is_treated_as_a_different_worktree() {
    // The shell's `cd "$dir" && pwd` runs in bash's default LOGICAL mode,
    // which does NOT resolve symbolic links — `pwd` reports `$PWD` exactly as
    // `cd` last set it. So a `repo_root` handed in via a symlink and a
    // `conflict_path` naming the same directory by its physical spelling
    // compare as DIFFERENT strings, same as the retired shell: this rung
    // takes the "different worktree" branch (0, no switch attempted), it
    // does not fold the two spellings together. Preserved deliberately, even
    // though it reads as "wrong" in the abstract — see
    // `resolve_dir_or_literal`'s doc comment for why physically resolving
    // would be a real behaviour change, not a fix.
    let dir = tmpdir("symlinked-root");
    let real = repo(&dir, "real-workspace", "feature/issue-42");
    git(&real, &["branch", "main"]);
    let link = dir.join("link-to-workspace");
    std::os::unix::fs::symlink(&real, &link).unwrap();

    // Error text names the physical path; repo_root is handed in via the
    // symlink, as `$WORKTREE_REPO_ROOT` could be if the checkout itself sits
    // behind one.
    let o = opts(conflict_error("feature/issue-42", &real), link);
    assert_eq!(
        run(&o),
        0,
        "a symlink-vs-physical spelling mismatch must not be folded together"
    );
    assert_eq!(
        current_branch(&real),
        "feature/issue-42",
        "the different-worktree branch must never attempt a checkout"
    );
}

// ---------------------------------------------------------------------------
// 7. Extraction — pure function
// ---------------------------------------------------------------------------

#[test]
fn extract_conflict_path_finds_a_single_quoted_match() {
    assert_eq!(
        extract_conflict_path("fatal: 'x' is already used by worktree at '/a/b c'\n"),
        Some("/a/b c".to_string())
    );
}

#[test]
fn extract_conflict_path_returns_none_without_a_closing_quote() {
    assert_eq!(extract_conflict_path("is already used by worktree at 'unterminated"), None);
}

#[test]
fn extract_conflict_path_returns_none_without_the_prefix_at_all() {
    assert_eq!(extract_conflict_path("fatal: something unrelated"), None);
}

#[test]
fn extract_conflict_path_joins_multiple_matches_with_newlines() {
    let text = "is already used by worktree at '/a' and also is already used by worktree at '/b'";
    assert_eq!(extract_conflict_path(text), Some("/a\n/b".to_string()));
}

#[test]
fn extract_conflict_path_handles_an_empty_quoted_segment() {
    assert_eq!(extract_conflict_path("is already used by worktree at ''"), Some(String::new()));
}

// ---------------------------------------------------------------------------
// 8. resolve_dir_or_literal
// ---------------------------------------------------------------------------

#[test]
fn resolve_dir_or_literal_does_not_resolve_a_symlinked_directory() {
    // The load-bearing property: this is NOT `fs::canonicalize`. `cd
    // "$link" && pwd` (logical mode) reports `$link` itself, not the target
    // it points at.
    let dir = tmpdir("resolve-existing");
    let sub = dir.join("actual");
    fs::create_dir_all(&sub).unwrap();
    let link = dir.join("via-link");
    std::os::unix::fs::symlink(&sub, &link).unwrap();

    assert_eq!(resolve_dir_or_literal(&link), link);
}

#[test]
fn resolve_dir_or_literal_falls_back_to_the_literal_path_when_not_a_directory() {
    let dir = tmpdir("resolve-missing");
    let missing = dir.join("never-existed");
    assert_eq!(resolve_dir_or_literal(&missing), missing);
}

#[test]
fn resolve_dir_or_literal_falls_back_when_the_path_is_a_plain_file() {
    let dir = tmpdir("resolve-file");
    let file = dir.join("plain.txt");
    fs::write(&file, b"not a directory").unwrap();
    assert_eq!(resolve_dir_or_literal(&file), file);
}

#[test]
fn resolve_dir_or_literal_falls_back_to_the_literal_relative_spelling_when_unresolvable() {
    // A relative argument that does not name a directory under the process's
    // actual cwd (regardless of what that cwd is) falls back to its literal,
    // relative spelling — matching `cd`'s own failure mode — rather than
    // panicking or silently absolutizing something nonexistent. Every real
    // caller passes an already-absolute string; this only pins the fallback.
    let relative = PathBuf::from("loom-branch-conflict-test-never-a-real-directory");
    assert_eq!(resolve_dir_or_literal(&relative), relative);
}

// ---------------------------------------------------------------------------
// 9. The quiet contract
// ---------------------------------------------------------------------------

#[test]
fn quiet_suppresses_every_message_level_including_error() {
    // Regression shape rather than an output capture (matching the sibling
    // `worktree-cleanup`/`worktree-link` tests): `Reporter`'s quiet gate must
    // cover `error()` too, unlike `Out::error`'s unconditional routing —
    // because the retired shell wrapped its `print_error` calls in the same
    // `if [[ "$JSON_OUTPUT" != "true" ]]` as everything else in this
    // function. Exercising every level must not panic; the byte-level
    // absence is asserted end-to-end by the differential harness.
    let r = Reporter { quiet: true };
    assert!(r.quiet);
    r.error("must not appear anywhere, including stderr");
    r.plain("must not appear");
    r.info("must not appear");
    r.success("must not appear");
    r.warning("must not appear");
}
