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
//! baked into this binary at build time?" so `loom-daemon status` can print an
//! "update available" hint. It never shells out to `cargo build`, never
//! provisions, and never restarts anything — it requires no operator opt-in
//! flag because it is inherently side-effect-free (at most one `git
//! rev-parse` subprocess). The ACTUAL update flow (rebuild -> provision ->
//! restart with the exact prior autonomy flags) lives entirely in
//! `.loom/scripts/cli/loom-daemon-update.sh`, a shell script — deliberately
//! NOT wired to auto-run from here.

use chrono::{DateTime, Utc};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The commit this binary was BUILT from, baked in at compile time via
/// `build.rs` -> `LOOM_DAEMON_GIT_COMMIT` (the same value folded into
/// `loom-daemon --version`). `"unknown"` when the build host lacked `git`
/// (e.g. a release-tarball build with no `.git` present).
pub const BUILT_COMMIT: &str = env!("LOOM_DAEMON_GIT_COMMIT");

/// The raw build-time stamp baked in by `build.rs` ->
/// `LOOM_DAEMON_BUILD_TIME` (ISO-8601 UTC, e.g. `2026-08-02T03:09:51Z`), the
/// same value `loom-daemon --version` prints. `"unknown"` when the build host
/// had no usable `date`. Prefer [`built_at`] when you want an instant.
pub const BUILT_AT_RAW: &str = env!("LOOM_DAEMON_BUILD_TIME");

/// [`BUILT_AT_RAW`] parsed into an instant, or `None` when the stamp is the
/// `"unknown"` fallback (or otherwise unparseable).
///
/// Returning `None` rather than a fabricated epoch keeps the daemon's
/// "unknown != zero" contract: a consumer that cannot learn when the binary
/// was built must see *absent*, never a wrong-but-plausible timestamp.
pub fn built_at() -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(BUILT_AT_RAW)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// The full build identity of this binary — `"<version> (commit <sha>, built
/// <ts>)"` — as shown by `loom-daemon --version`.
///
/// Lives here (the lib crate) rather than inline in `main.rs` so library code
/// can stamp it into operator-facing failures too: the empty-token-pool error
/// names the deciding binary (#4643) precisely because "which binary made this
/// decision?" was unanswerable from the 2026-07-30 incident logs. `main.rs`'s
/// clap `--version` string is this same constant, so the two can never drift.
pub const BUILD_IDENTITY: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (commit ",
    env!("LOOM_DAEMON_GIT_COMMIT"),
    ", built ",
    env!("LOOM_DAEMON_BUILD_TIME"),
    ")"
);

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
    /// Number of commits the source checkout's HEAD is ahead of
    /// `built_commit` (`git rev-list --count built_commit..source_commit`,
    /// Issue #6261). `None` whenever `update_available` is not `Some(true)`,
    /// or the count could not be computed (e.g. `built_commit` is not
    /// reachable in this checkout's history — a shallow clone, or a rebase/
    /// force-push that rewrote it away).
    pub commits_behind: Option<u32>,
    /// Whole hours elapsed since the OLDEST commit in `built_commit
    /// ..source_commit` landed — i.e. how long the FIRST fix this binary is
    /// missing has been sitting unbuilt (Issue #6261, the staleness surface
    /// the 2026-08-14 incident named as missing: "one full day and 20+
    /// merges with zero signal"). Rounded down to whole hours (a warning
    /// threshold does not need sub-hour precision). `None` under the same
    /// conditions as `commits_behind`.
    pub hours_behind: Option<u32>,
}

/// Default warn threshold (commit count) for [`staleness_warning`]: once the
/// running binary is at least this many commits behind its source checkout,
/// the staleness is surfaced as a warning rather than a quiet "update
/// available" hint. Overridable via `LOOM_SELF_UPDATE_STALE_WARN_COMMITS`.
pub const DEFAULT_STALE_WARN_COMMITS: u32 = 10;

/// Default warn threshold (whole hours) for [`staleness_warning`] — mirrors
/// [`DEFAULT_STALE_WARN_COMMITS`] but for elapsed time, so a LOW-traffic
/// source checkout that is nonetheless stale for a long time (few commits,
/// but none of them ever rolled) still gets flagged. Overridable via
/// `LOOM_SELF_UPDATE_STALE_WARN_HOURS`.
pub const DEFAULT_STALE_WARN_HOURS: u32 = 12;

/// Env override for [`DEFAULT_STALE_WARN_COMMITS`].
pub const STALE_WARN_COMMITS_ENV: &str = "LOOM_SELF_UPDATE_STALE_WARN_COMMITS";

/// Env override for [`DEFAULT_STALE_WARN_HOURS`].
pub const STALE_WARN_HOURS_ENV: &str = "LOOM_SELF_UPDATE_STALE_WARN_HOURS";

/// Resolve the commit-count warn threshold: env override, else
/// [`DEFAULT_STALE_WARN_COMMITS`]. A zero/unparseable env value falls back to
/// the default rather than warning on every single stale commit.
#[must_use]
pub fn resolve_stale_warn_commits() -> u32 {
    std::env::var(STALE_WARN_COMMITS_ENV)
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_STALE_WARN_COMMITS)
}

/// Resolve the elapsed-hours warn threshold: env override, else
/// [`DEFAULT_STALE_WARN_HOURS`]. A zero/unparseable env value falls back to
/// the default.
#[must_use]
pub fn resolve_stale_warn_hours() -> u32 {
    std::env::var(STALE_WARN_HOURS_ENV)
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_STALE_WARN_HOURS)
}

/// Pure warning-formatting logic (Issue #6261's staleness surface), split out
/// from threshold resolution so it is unit-testable with plain values. `None`
/// when neither magnitude is known or neither exceeds its threshold.
#[must_use]
pub fn staleness_warning(
    commits_behind: Option<u32>,
    hours_behind: Option<u32>,
    warn_commits: u32,
    warn_hours: u32,
) -> Option<String> {
    let commits_exceeded = commits_behind.is_some_and(|c| c >= warn_commits);
    let hours_exceeded = hours_behind.is_some_and(|h| h >= warn_hours);
    if !commits_exceeded && !hours_exceeded {
        return None;
    }
    let commits_str = commits_behind.map_or_else(|| "?".to_string(), |c| c.to_string());
    let hours_str = hours_behind.map_or_else(|| "?".to_string(), |h| h.to_string());
    Some(format!(
        "running binary is {commits_str} commit(s) / {hours_str}h behind its source checkout \
         (warn thresholds: {warn_commits} commits / {warn_hours}h) — run \
         `./.loom/scripts/cli/loom-daemon-update.sh` (or check why the autonomous auto_update \
         loop has not rolled it, `autonomous.autoUpdate.enabled`)"
    ))
}

/// [`staleness_warning`] resolved against the env-overridable default
/// thresholds ([`resolve_stale_warn_commits`] / [`resolve_stale_warn_hours`]).
#[must_use]
pub fn staleness_warning_default(
    commits_behind: Option<u32>,
    hours_behind: Option<u32>,
) -> Option<String> {
    staleness_warning(
        commits_behind,
        hours_behind,
        resolve_stale_warn_commits(),
        resolve_stale_warn_hours(),
    )
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

/// Count of commits in `repo_root` strictly between `from` (exclusive) and
/// `to` (inclusive) — `git rev-list --count from..to`. `None` on any git
/// failure (including `from` not being reachable in this checkout's history,
/// e.g. a shallow clone or a history-rewriting rebase/force-push).
fn commits_between(repo_root: &Path, from: &str, to: &str) -> Option<u32> {
    let output = Command::new("git")
        .args(["rev-list", "--count", &format!("{from}..{to}")])
        .current_dir(repo_root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()?.trim().parse().ok()
}

/// Whole hours elapsed between now and the commit-time of the OLDEST commit
/// in `from..to` — i.e. how long the FIRST commit this binary is missing has
/// been sitting unbuilt. `None` on any git failure or an empty/unparseable
/// range.
fn hours_since_oldest(repo_root: &Path, from: &str, to: &str) -> Option<u32> {
    let output = Command::new("git")
        .args(["log", "--format=%ct", &format!("{from}..{to}")])
        .current_dir(repo_root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    // `git log` lists newest-first, so the OLDEST commit in the range is the
    // last line.
    let oldest_epoch: i64 = stdout.lines().next_back()?.trim().parse().ok()?;
    let elapsed_secs = Utc::now().timestamp().saturating_sub(oldest_epoch);
    u32::try_from(elapsed_secs.max(0) / 3600).ok()
}

/// Compute the current self-update status. Read-only: `git rev-parse` plus
/// (only when a rebuild is actually available) `git rev-list`/`git log`
/// subprocesses to size the staleness — no writes, no network calls. Cheap
/// enough to call on every `loom-daemon status` invocation.
#[must_use]
pub fn check() -> SelfUpdateStatus {
    let source_commit = source_head_commit();
    let update_available = compare(BUILT_COMMIT, source_commit.as_deref());
    let (commits_behind, hours_behind) = if update_available == Some(true) {
        match (source_checkout_root(), source_commit.as_deref()) {
            (Some(root), Some(source)) => (
                commits_between(&root, BUILT_COMMIT, source),
                hours_since_oldest(&root, BUILT_COMMIT, source),
            ),
            _ => (None, None),
        }
    } else {
        (None, None)
    };
    SelfUpdateStatus {
        built_commit: BUILT_COMMIT.to_string(),
        source_commit,
        update_available,
        commits_behind,
        hours_behind,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

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

    // ===================================================================
    // Staleness magnitude (Issue #6261) — commits_between / hours_since_oldest
    // ===================================================================

    /// A throwaway git repo with a sequence of commits, so `commits_between` /
    /// `hours_since_oldest` can be exercised against known history instead of
    /// whatever checkout happens to build the test binary.
    struct TestRepo {
        dir: tempfile::TempDir,
    }

    impl TestRepo {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let run = |args: &[&str]| {
                let status = Command::new("git")
                    .args(args)
                    .current_dir(dir.path())
                    .env("GIT_AUTHOR_NAME", "test")
                    .env("GIT_AUTHOR_EMAIL", "test@example.com")
                    .env("GIT_COMMITTER_NAME", "test")
                    .env("GIT_COMMITTER_EMAIL", "test@example.com")
                    .status()
                    .expect("run git");
                assert!(status.success(), "git {args:?} failed");
            };
            run(&["init", "--quiet"]);
            run(&["commit", "--allow-empty", "--quiet", "-m", "c0"]);
            Self { dir }
        }

        /// Commit an empty commit `--date`d `secs_ago` seconds before now, and
        /// return its short SHA.
        fn commit_secs_ago(&self, secs_ago: i64, msg: &str) -> String {
            let epoch = Utc::now().timestamp() - secs_ago;
            let date = format!("{epoch} +0000");
            let status = Command::new("git")
                .args(["commit", "--allow-empty", "--quiet", "-m", msg])
                .current_dir(self.dir.path())
                .env("GIT_AUTHOR_NAME", "test")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .env("GIT_AUTHOR_DATE", &date)
                .env("GIT_COMMITTER_DATE", &date)
                .status()
                .expect("run git commit");
            assert!(status.success());
            let output = Command::new("git")
                .args(["rev-parse", "--short", "HEAD"])
                .current_dir(self.dir.path())
                .output()
                .expect("rev-parse");
            String::from_utf8(output.stdout).unwrap().trim().to_string()
        }

        fn short_head(&self) -> String {
            let output = Command::new("git")
                .args(["rev-parse", "--short", "HEAD"])
                .current_dir(self.dir.path())
                .output()
                .expect("rev-parse");
            String::from_utf8(output.stdout).unwrap().trim().to_string()
        }
    }

    #[test]
    fn commits_between_counts_only_the_range() {
        let repo = TestRepo::new();
        let base = repo.short_head();
        repo.commit_secs_ago(3600, "c1");
        repo.commit_secs_ago(1800, "c2");
        let head = repo.commit_secs_ago(0, "c3");
        assert_eq!(commits_between(repo.dir.path(), &base, &head), Some(3));
        // Same commit on both sides ⇒ zero commits in the range.
        assert_eq!(commits_between(repo.dir.path(), &head, &head), Some(0));
    }

    #[test]
    fn commits_between_unreachable_from_is_none() {
        let repo = TestRepo::new();
        let head = repo.short_head();
        // A commit SHA that was never part of this repo's history.
        assert_eq!(
            commits_between(repo.dir.path(), "0000000000000000000000000000000000000000", &head),
            None
        );
    }

    #[test]
    fn hours_since_oldest_reports_the_oldest_commits_age() {
        let repo = TestRepo::new();
        let base = repo.short_head();
        // Oldest new commit landed 5 hours ago; a more recent one 1 hour ago.
        repo.commit_secs_ago(5 * 3600 + 120, "oldest");
        let head = repo.commit_secs_ago(3600, "newest");
        // 5h+ elapsed since the oldest commit in the range ⇒ rounds down to 5.
        assert_eq!(hours_since_oldest(repo.dir.path(), &base, &head), Some(5));
    }

    #[test]
    fn hours_since_oldest_empty_range_is_none() {
        let repo = TestRepo::new();
        let head = repo.short_head();
        assert_eq!(hours_since_oldest(repo.dir.path(), &head, &head), None);
    }

    // ===================================================================
    // staleness_warning — pure formatting/threshold logic
    // ===================================================================

    #[test]
    fn staleness_warning_none_below_both_thresholds() {
        assert_eq!(staleness_warning(Some(3), Some(2), 10, 12), None);
    }

    #[test]
    fn staleness_warning_fires_on_commits_alone() {
        let warning = staleness_warning(Some(10), Some(1), 10, 12);
        assert!(warning.is_some());
        assert!(warning.unwrap().contains("10 commit(s)"));
    }

    #[test]
    fn staleness_warning_fires_on_hours_alone() {
        let warning = staleness_warning(Some(1), Some(12), 10, 12);
        assert!(warning.is_some());
        assert!(warning.unwrap().contains("12h"));
    }

    #[test]
    fn staleness_warning_none_when_magnitudes_unknown() {
        assert_eq!(staleness_warning(None, None, 10, 12), None);
    }

    #[test]
    #[serial(loom_self_update_stale_warn_env)]
    fn resolve_stale_warn_thresholds_default_when_env_unset_or_invalid() {
        std::env::remove_var(STALE_WARN_COMMITS_ENV);
        std::env::remove_var(STALE_WARN_HOURS_ENV);
        assert_eq!(resolve_stale_warn_commits(), DEFAULT_STALE_WARN_COMMITS);
        assert_eq!(resolve_stale_warn_hours(), DEFAULT_STALE_WARN_HOURS);

        std::env::set_var(STALE_WARN_COMMITS_ENV, "0");
        std::env::set_var(STALE_WARN_HOURS_ENV, "not-a-number");
        assert_eq!(resolve_stale_warn_commits(), DEFAULT_STALE_WARN_COMMITS);
        assert_eq!(resolve_stale_warn_hours(), DEFAULT_STALE_WARN_HOURS);

        std::env::set_var(STALE_WARN_COMMITS_ENV, "25");
        std::env::set_var(STALE_WARN_HOURS_ENV, "6");
        assert_eq!(resolve_stale_warn_commits(), 25);
        assert_eq!(resolve_stale_warn_hours(), 6);

        std::env::remove_var(STALE_WARN_COMMITS_ENV);
        std::env::remove_var(STALE_WARN_HOURS_ENV);
    }
}
