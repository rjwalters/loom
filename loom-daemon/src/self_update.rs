//! Self-update staleness detection surfaced through `loom-daemon status`
//! (Issue #3968).
//!
//! Context: the 2026-07-25/26 canary rollout proved the self-repair loop
//! works — the daemon filed and fixed 16 of its own defects — but every
//! merged daemon fix only took effect after an operator manually rebuilt the
//! Rust binary, reprovisioned it, and restarted the process. Nothing told the
//! operator a rebuild was overdue; it was discovered only by noticing
//! behavior hadn't changed.
//!
//! This module is the READ-ONLY half of the fix: it answers "does the source
//! checkout this binary was compiled from have newer commits than the commit
//! baked into this binary at build time?" so `--status` can print an
//! "update available" hint. It never shells out to `cargo build`, never
//! provisions, and never restarts anything — it requires no operator opt-in
//! flag because it is inherently side-effect-free (at most one `git
//! rev-parse` subprocess). The ACTUAL update flow (rebuild -> provision ->
//! restart with the exact prior autonomy flags) lives entirely in
//! `.loom/scripts/cli/loom-daemon-update.sh`, a shell script — deliberately
//! NOT wired to auto-run from here.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The commit this binary was BUILT from, baked in at compile time via
/// `build.rs` -> `LOOM_DAEMON_GIT_COMMIT` (the same value folded into
/// `loom-daemon --version`). `"unknown"` when the build host lacked `git`
/// (e.g. a release-tarball build with no `.git` present).
pub const BUILT_COMMIT: &str = env!("LOOM_DAEMON_GIT_COMMIT");

/// This crate's own directory in the source tree it was compiled from, baked
/// in at compile time via `CARGO_MANIFEST_DIR` — the same
/// compiled-from-source-tree technique the (retired in #4228)
/// `sweep_registry::resolve_package_path_env` used for `LOOM_PACKAGE_PATH`
/// resolution (issue #3949). This is a build-time constant, so it keeps
/// pointing at the original checkout even after the compiled binary is
/// copied elsewhere (e.g. `~/.local/bin/loom-daemon`, issue #3922) — as long
/// as that checkout is still present on the same machine.
const BUILD_MANIFEST_DIR: &str = env!("CARGO_MANIFEST_DIR");

/// The self-update status `loom-daemon status` surfaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelfUpdateStatus {
    /// The commit this running binary was built from (`"unknown"` if the
    /// build host lacked `git` at build time).
    pub built_commit: String,
    /// The source checkout's current HEAD short commit, when that checkout is
    /// still present on this machine and `git` resolves it. `None` when the
    /// source tree this binary was built from is no longer present (binary
    /// copied to another machine, or built from a release tarball with no
    /// sibling `.git`).
    pub source_commit: Option<String>,
    /// `true` when `source_commit` is known and differs from `built_commit` —
    /// i.e. rebuilding right now would produce a different binary than the
    /// one currently running. `None` when the comparison cannot be made (no
    /// source checkout found, or `built_commit` is `"unknown"`).
    pub update_available: Option<bool>,
}

/// Pure comparison, split out from any filesystem/subprocess access so it is
/// unit-testable without a real git checkout.
fn compare(built: &str, source: Option<&str>) -> Option<bool> {
    let source = source?;
    if built == "unknown" || built.is_empty() || source.is_empty() || source == "unknown" {
        return None;
    }
    Some(built != source)
}

/// Resolve the source checkout's current HEAD short commit, if that checkout
/// is still present on this machine. `BUILD_MANIFEST_DIR` is
/// `<repo>/loom-daemon`; its parent is the repo root.
fn source_head_commit() -> Option<String> {
    let repo_root = Path::new(BUILD_MANIFEST_DIR).join("..");
    if !repo_root.join(".git").exists() {
        return None;
    }
    let output = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(&repo_root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let commit = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if commit.is_empty() {
        None
    } else {
        Some(commit)
    }
}

/// The source checkout this binary was built from — `BUILD_MANIFEST_DIR`'s
/// parent (`<repo>/loom-daemon` → `<repo>`) — when that checkout is still
/// present on this machine (a sibling `.git` exists). `None` for a tarball
/// build / a binary copied to another machine, exactly the case
/// [`SelfUpdateStatus::update_available`] reports as `None`. The autonomous
/// self-update loop (#4055) uses this to resolve the rebuild cwd and the
/// `loom-daemon-update.sh` script path.
#[must_use]
pub fn source_checkout_root() -> Option<PathBuf> {
    let repo_root = Path::new(BUILD_MANIFEST_DIR).join("..");
    if repo_root.join(".git").exists() {
        Some(repo_root)
    } else {
        None
    }
}

/// Whether the source checkout's working tree is clean (no staged, unstaged,
/// or untracked changes) via `git status --porcelain`. Empty output ⇒ clean.
///
/// `None` when the source checkout is not present on this machine (a tarball
/// build) or `git status` could not be run — the caller must treat `None` as
/// "cannot prove clean", never as clean. The autonomous self-update loop
/// (#4055) gates every unattended `cargo build --release` on this being
/// `Some(true)`: `CARGO_MANIFEST_DIR` points at the operator's live working
/// checkout, so building a dirty tree would compile whatever is uncommitted
/// into the running daemon.
#[must_use]
pub fn source_tree_clean() -> Option<bool> {
    let repo_root = source_checkout_root()?;
    let output = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&repo_root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(output.stdout.iter().all(u8::is_ascii_whitespace))
}

/// Compute the current self-update status. Read-only: at most one `git
/// rev-parse` subprocess, no writes, no network calls. Cheap enough to call
/// on every `--status` invocation.
#[must_use]
pub fn check() -> SelfUpdateStatus {
    let source_commit = source_head_commit();
    let update_available = compare(BUILT_COMMIT, source_commit.as_deref());
    SelfUpdateStatus {
        built_commit: BUILT_COMMIT.to_string(),
        source_commit,
        update_available,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compare_same_commit_is_up_to_date() {
        assert_eq!(compare("abc1234", Some("abc1234")), Some(false));
    }

    #[test]
    fn compare_different_commit_reports_update_available() {
        assert_eq!(compare("abc1234", Some("def5678")), Some(true));
    }

    #[test]
    fn compare_unknown_built_commit_is_undecidable() {
        assert_eq!(compare("unknown", Some("def5678")), None);
    }

    #[test]
    fn compare_no_source_checkout_is_undecidable() {
        assert_eq!(compare("abc1234", None), None);
    }

    #[test]
    fn compare_empty_source_is_undecidable() {
        assert_eq!(compare("abc1234", Some("")), None);
    }

    #[test]
    fn compare_empty_built_is_undecidable() {
        assert_eq!(compare("", Some("def5678")), None);
    }

    #[test]
    fn check_never_panics_and_reports_a_built_commit() {
        // `check()` shells out to `git` against whatever machine runs the test
        // suite, so we don't assert on the exact source_commit/update_available
        // values (environment-dependent) — only that it completes and reports
        // some built_commit string (possibly "unknown").
        let status = check();
        assert!(!status.built_commit.is_empty());
    }

    #[test]
    fn source_tree_clean_never_panics() {
        // Environment-dependent (runs `git status` against whatever checkout
        // built the test binary), so we only assert it completes and returns a
        // well-formed tri-state — never that the tree is actually clean/dirty.
        let clean = source_tree_clean();
        // `source_checkout_root()` and `source_tree_clean()` must agree on
        // "no checkout present": if there is no source root, clean is `None`.
        if source_checkout_root().is_none() {
            assert_eq!(clean, None);
        }
    }
}
