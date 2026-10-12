//! Live-binary check of the `kept:` line and summary for a worktree-held
//! branch (#10851, following #10766/#10848).
//!
//! `clean_branches` reports with `println!`, which a unit test cannot capture,
//! so this runs the real `loom-daemon clean --branches-only --force` against a
//! temp repo and asserts on what it actually printed. The repo has no `origin`
//! remote: a held branch is skipped before any remote or forge probe, so no
//! network call is made.

use std::path::Path;
use std::process::{Command, Stdio};

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(["-c", "user.email=t@example.com", "-c", "user.name=t"])
        .args(args)
        .current_dir(dir)
        .stdout(Stdio::null())
        .status()
        .expect("git must spawn");
    assert!(status.success(), "git {args:?} failed");
}

#[test]
fn clean_prints_the_kept_line_and_counts_held_branches_as_in_use() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let repo = root.join("repo");
    std::fs::create_dir_all(repo.join(".loom")).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("a.txt"), "base\n").unwrap();
    git(&repo, &["add", "a.txt"]);
    git(&repo, &["commit", "-q", "-m", "init"]);

    // A dirty worktree holding a stale (no remote) branch.
    let wt = root.join("wt-pr-1");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature/issue-1",
            wt.to_str().unwrap(),
        ],
    );
    std::fs::write(wt.join("dirty.txt"), "uncommitted").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .args([
            "clean",
            "--branches-only",
            "--force",
            "--workspace",
            repo.to_str().unwrap(),
        ])
        .env("LOOM_CONFIG_DEFAULTS_FILE", "")
        .stdin(Stdio::null())
        .output()
        .expect("loom-daemon clean must spawn");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "clean failed\nstdout:\n{stdout}\nstderr:\n{stderr}");

    let kept = format!("kept: worktree {} holds feature/issue-1", wt.display());
    assert!(stdout.contains(&kept), "missing `{kept}` in:\n{stdout}");
    assert!(
        stdout.contains("In use (held by a worktree): 1 branch(es)"),
        "missing in-use summary in:\n{stdout}"
    );
    assert!(stdout.contains("Kept: 0 branch(es)"), "held branch counted as Kept:\n{stdout}");
    assert!(wt.join("dirty.txt").exists());
}
