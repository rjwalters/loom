//! Regression tests for `loom-daemon recover-orphans` run from a non-repo CWD
//! (issue #5140).
//!
//! The reported failure was run from `$HOME` on a fleet host: `~/.loom` exists
//! there (the token pool `loom-daemon tokens bootstrap` provisions), the
//! repo-root walk accepted any ancestor with a `.loom/` directory, so `$HOME`
//! was resolved as the "repo root" and `gh issue list` was run there — failing
//! with `fatal: not a git repository`. The command then printed the reassuring
//! **"No orphaned tasks found"** and exited 0: a false all-clear for exactly
//! the check (are any claims stranded?) an operator runs after a killed sweep.

use std::process::Command;

/// A directory holding machine-level daemon state (`.loom/tokens`) but no
/// `.git` must not be mistaken for a repository root. The command must fail
/// loudly, name what it required, and never emit the all-clear line.
#[test]
fn recover_orphans_from_a_bare_loom_dir_fails_loudly() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".loom").join("tokens")).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .arg("recover-orphans")
        .current_dir(dir.path())
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "must exit non-zero when it cannot assess claims; stdout: {stdout}"
    );
    assert!(
        !stdout.contains("No orphaned tasks found"),
        "false all-clear after a failed assessment: {stdout}"
    );
    assert!(
        stderr.contains("not inside a Loom repository"),
        "the error must name what it required; stderr: {stderr}"
    );
    assert!(
        stderr.contains("--workspace"),
        "the error must name the escape hatch; stderr: {stderr}"
    );
}

/// The already-working "not in a repo at all" case must keep working — no
/// `.loom/` anywhere upward.
#[test]
fn recover_orphans_from_a_plain_non_repo_dir_fails_loudly() {
    let dir = tempfile::tempdir().unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .arg("recover-orphans")
        .current_dir(dir.path())
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!output.status.success(), "stdout: {stdout}");
    assert!(
        !stdout.contains("No orphaned tasks found"),
        "false all-clear outside any repository: {stdout}"
    );
}

/// `--json` consumers get the same honesty: no success-shaped payload, and the
/// process still exits non-zero.
#[test]
fn recover_orphans_json_from_a_bare_loom_dir_fails_loudly() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".loom").join("tokens")).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .args(["recover-orphans", "--json"])
        .current_dir(dir.path())
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!output.status.success(), "stdout: {stdout}");
    assert!(
        !stdout.contains("\"total_orphaned\": 0"),
        "a zero-orphan payload after a failed resolution is a false all-clear: {stdout}"
    );
}
