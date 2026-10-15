//! Regression test for issue #5736: `loom-daemon clean --aggressive` used to
//! short-circuit *before* the shared confirmation gate every other
//! destructive `clean` mode goes through (`worktrees_only`, `branches_only`,
//! `tmux_only`, and the general pass all route through
//! `worktree_ops::clean::run_clean`, which calls
//! `confirm_destructive_action`). That made `--aggressive` — the most
//! destructive combination, since it can remove worktrees *and* branches and
//! overrides several of the safety checks the non-aggressive modes respect —
//! the one mode that a non-interactive/piped invocation (closed stdin, no
//! TTY, no `--force`/`--dry-run`) could run to completion with zero prompt.
//!
//! This test asserts a `--aggressive --worktrees-only` invocation with stdin
//! closed (`< /dev/null`) aborts before touching anything on disk — matching
//! today's already-correct `--branches-only` behavior — rather than silently
//! destroying worktrees.

use std::process::{Command, Stdio};

/// Initialize a minimal git repo (with an initial commit so `HEAD` resolves)
/// plus the `.loom/` marker directory `resolve_repo_root` requires to treat
/// the directory as a Loom repository root.
fn init_repo(dir: &std::path::Path) {
    let run = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .expect("git command must spawn");
        assert!(status.success(), "git {args:?} failed");
    };
    run(&["init", "--quiet"]);
    run(&["config", "user.email", "test@example.com"]);
    run(&["config", "user.name", "Test"]);
    run(&["commit", "--allow-empty", "--quiet", "-m", "initial"]);
    std::fs::create_dir_all(dir.join(".loom")).unwrap();
}

/// AC5 (issue #5736): with no TTY and no explicit affirmative, `--aggressive
/// --worktrees-only` must abort — removing nothing — rather than proceeding
/// like today's unguarded aggressive path did.
#[test]
fn aggressive_worktrees_only_with_closed_stdin_removes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());

    // A second worktree that would be a live candidate for aggressive
    // removal (untracked branch, no open PR, no active shepherd) if the run
    // were ever allowed to proceed past the confirmation gate.
    let extra_worktree = dir
        .path()
        .parent()
        .unwrap()
        .join(format!("{}-extra-worktree", dir.path().file_name().unwrap().to_string_lossy()));
    let status = Command::new("git")
        .args([
            "worktree",
            "add",
            "-b",
            "feature/issue-99999",
            extra_worktree.to_str().unwrap(),
        ])
        .current_dir(dir.path())
        .status()
        .expect("git worktree add must spawn");
    assert!(status.success(), "failed to create scratch worktree");
    assert!(
        extra_worktree.exists(),
        "precondition: scratch worktree must exist before cleanup"
    );

    let output = Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .args([
            "clean",
            "--aggressive",
            "--worktrees-only",
            "--workspace",
            dir.path().to_str().unwrap(),
        ])
        .stdin(Stdio::null())
        .output()
        .expect("loom-daemon clean must spawn");

    let stdout = String::from_utf8_lossy(&output.stdout);

    let success = output.status.success();
    let cancelled = stdout.contains("Cleanup cancelled");
    let never_ran = !stdout.contains("Aggressive cleanup complete")
        && !stdout.contains("No worktrees enumerated");
    let untouched = extra_worktree.exists();

    // Cleanup after the assertions have captured every outcome, regardless
    // of pass/fail.
    let _ = std::fs::remove_dir_all(&extra_worktree);
    let _ = Command::new("git")
        .args(["worktree", "prune"])
        .current_dir(dir.path())
        .status();

    assert!(
        success,
        "an aborted (cancelled) run is not an error condition; stdout: {stdout}"
    );
    assert!(
        cancelled,
        "closed stdin with no --force/--dry-run must abort via the shared confirmation gate; \
         stdout: {stdout}"
    );
    assert!(
        never_ran,
        "the aggressive removal pass must never run once the confirmation gate rejects the \
         invocation; stdout: {stdout}"
    );
    assert!(
        untouched,
        "the aggressive cleanup pass must not have touched the scratch worktree: {}",
        extra_worktree.display()
    );
}

/// `--dry-run` must keep bypassing the confirmation gate with no prompt in
/// every mode, `--aggressive` included (AC4) — this is the counterpart to the
/// abort case above: a read-only invocation must never need a TTY.
#[test]
fn aggressive_dry_run_with_closed_stdin_does_not_abort() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());

    let output = Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .args([
            "clean",
            "--aggressive",
            "--worktrees-only",
            "--dry-run",
            "--workspace",
            dir.path().to_str().unwrap(),
        ])
        .stdin(Stdio::null())
        .output()
        .expect("loom-daemon clean must spawn");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "--dry-run must never require a TTY/prompt; stdout: {stdout}"
    );
    assert!(
        !stdout.contains("Cleanup cancelled"),
        "--dry-run bypasses the confirmation gate rather than being rejected by it; \
         stdout: {stdout}"
    );
}
