//! Shared fixtures for the `worktree_cli` per-module test suites (#13530).
//!
//! Every suite used to carry its own copy of `tmpdir` and `git`, and the copies
//! drifted: two of them ran git with no hermetic environment, so they read the
//! host's global git config (the leak class tracked in #11116). One definition
//! here makes every suite hermetic by construction.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A fresh, canonicalized scratch directory under the system temp dir.
///
/// `prefix` names the suite (and may contain a space where a case needs one,
/// #7858's class); `tag` names the case. The pid and thread id keep concurrent
/// cases apart.
pub fn tmpdir(prefix: &str, tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!(
        "{prefix}-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(&base).expect("create tmpdir");
    fs::canonicalize(&base).expect("canonicalize tmpdir")
}

/// Run `git -C <dir> <args>` with a hardened environment (no global/system
/// config, fixed author and committer, `LC_ALL=C`). Panics, naming the
/// directory and stderr, unless git succeeds.
pub fn git(dir: &Path, args: &[&str]) -> Output {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Loom Test")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .env("GIT_COMMITTER_NAME", "Loom Test")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .env("LC_ALL", "C")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} in {} failed: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    out
}
