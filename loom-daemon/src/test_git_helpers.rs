//! Hermetic git helpers shared by the in-crate tests that drive real
//! repositories (orphan-stack replacement and the orphaned-stacked-child pass).

use std::path::Path;
use std::process::Command;

pub fn git_out(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Loom Test")
        .env("GIT_AUTHOR_EMAIL", "loom@example.com")
        .env("GIT_COMMITTER_NAME", "Loom Test")
        .env("GIT_COMMITTER_EMAIL", "loom@example.com")
        .output()
        .expect("git must spawn")
}

pub fn run(dir: &Path, args: &[&str]) -> String {
    let out = git_out(dir, args);
    assert!(
        out.status.success(),
        "git {args:?} failed in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

pub fn commit_file(dir: &Path, name: &str, body: &str, msg: &str) -> String {
    std::fs::write(dir.join(name), body).unwrap();
    run(dir, &["add", "-A"]);
    run(dir, &["commit", "-q", "-m", msg]);
    run(dir, &["rev-parse", "HEAD"])
}

pub fn configure(dir: &Path) {
    run(dir, &["config", "user.email", "loom@example.com"]);
    run(dir, &["config", "user.name", "Loom Test"]);
    run(dir, &["config", "commit.gpgsign", "false"]);
}
