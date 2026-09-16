//! Removal-failure backoff + stuck-worktree health surfacing (#7590), and the
//! orphan-record prune that lets a stuck record clear when the operator
//! resolves the condition by hand (#7709).
//!
//! Extracted from [`super`] rather than living inline: `worktree_reaper.rs` is
//! over the file-size ratchet's threshold (`scripts/file-size-baseline.txt`),
//! so the rule is "put the new code in a NEW sibling module". The public
//! surface is unchanged — [`super`] re-exports everything callers already name
//! as `crate::worktree_reaper::…`.
//!
//! # The state machine, in one paragraph
//!
//! Every failed `issue-<N>`/`pr-<N>` removal records a [`RemovalFailureRecord`]
//! keyed by the worktree's on-disk path. A permission-class cause backs off
//! immediately (it cannot self-resolve); any other cause backs off only after
//! [`REMOVAL_FAILURE_CAP`] *consecutive* failures. A backed-off path is skipped
//! entirely — no syscalls — until [`REMOVAL_BACKOFF_SECS`] have elapsed, and is
//! surfaced on `loom-daemon health` by [`stuck_worktree_removals`]. A record is
//! cleared by exactly two things: a removal that actually succeeds, and the
//! #7709 prune below.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};

use chrono::{DateTime, Utc};

use super::WorktreeKind;

/// Per-path removal-failure state, tracked across reaper ticks (#7590). Never
/// persisted across a daemon restart — an acceptable trade-off given the
/// daemon restarts far less often than the 15-minute reap tick, called out
/// explicitly here rather than silently accepted.
#[derive(Debug, Clone)]
struct RemovalFailureRecord {
    repo_root: PathBuf,
    kind: WorktreeKind,
    number: u32,
    last_cause: String,
    first_failure_at: DateTime<Utc>,
    last_attempt_at: DateTime<Utc>,
    attempt_count: u32,
    /// Set once the failure is classified as permanent (a permission-class
    /// error) — as opposed to merely having crossed [`REMOVAL_FAILURE_CAP`]
    /// consecutive failures of an unclassified (potentially transient) cause.
    /// Both conditions back off identically; this only changes the wording of
    /// the surfaced explanation.
    permanent: bool,
}

/// How many consecutive failed removal attempts (of any cause) before a
/// worktree backs off even when the cause is not classified as a permanent
/// permission error (AC1 of #7590: "at minimum, retries ... are capped in
/// frequency"). A single transient failure (a lock, a race with a concurrent
/// `git worktree` op) must never trip this — only a *run* of consecutive
/// failures for the exact same path does, and a single intervening success
/// clears the run entirely (see [`remove_with_backoff`]).
pub const REMOVAL_FAILURE_CAP: u32 = 3;

/// How long a backed-off path waits before its next retry attempt (#7590 AC1:
/// "no more than once per N hours" — exact cadence is an implementation
/// choice, documented here). Six hours: frequent enough that a *self-resolving*
/// condition (a lock released, a busy mount unmounted, a concurrent `git
/// worktree` op finished) is picked up the same day, infrequent enough to
/// eliminate the "every single tick, forever" log noise and wasted `git
/// worktree remove` calls #7590 was filed for.
///
/// This cadence deliberately claims nothing about the *other* remediation. An
/// operator's `sudo rm -rf <worktree>` is **not** picked up by this retry:
/// once the directory is gone the reaper's `enumerate_worktree_dirs` scan never
/// yields it again, so no retry — at six hours or six seconds — can ever run.
/// That case is handled by the confirmed-absence prune in
/// [`prune_orphaned_removal_records`] / [`stuck_worktree_removals`] instead
/// (#7709), which clears the record on the next status call rather than waiting
/// out this window.
pub const REMOVAL_BACKOFF_SECS: i64 = 6 * 3600;

/// Whether a `cleanup_worktree`/`cleanup_pr_worktree` failure `cause` string
/// names a permission-class error — precisely the root cause #7590 was filed
/// for: `std::fs::remove_dir_all`'s `Permission denied (os error 13)`,
/// embedded verbatim in the propagated `Err` string at `clean.rs`'s untracked-
/// orphan-directory fallback. Classified by substring match on the
/// `std::io::Error` `Display` text rather than by threading a richer error
/// type through `clean::cleanup_worktree`'s public contract (the issue's
/// "Option B") — cheaper, and that contract is deliberately preserved for
/// operator log visibility (#4877). A permission-class failure cannot
/// self-resolve without a manual `sudo` intervention, so it is always
/// permanent — unlike a merely-repeated failure of unknown cause, which only
/// backs off once it has actually repeated (see [`REMOVAL_FAILURE_CAP`]).
#[must_use]
pub fn is_permission_denied_cause(cause: &str) -> bool {
    cause.to_ascii_lowercase().contains("permission denied")
}

/// Process-global removal-failure tracker, keyed by the worktree's on-disk
/// path (already unique per call site — no need to additionally key by repo
/// root or worktree number). Mirrors the [`super::RECLAIM_IN_FLIGHT`]
/// process-global pattern.
static REMOVAL_FAILURES: OnceLock<Mutex<HashMap<PathBuf, RemovalFailureRecord>>> = OnceLock::new();

fn removal_failures() -> &'static Mutex<HashMap<PathBuf, RemovalFailureRecord>> {
    REMOVAL_FAILURES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The pure half of [`record_is_orphaned`]: given the outcome of a
/// `Path::try_exists` probe, may the record be dropped? (#7709)
///
/// Only a **confirmed** `Ok(false)` counts. This is why the caller uses
/// [`Path::try_exists`] and not [`Path::exists`]: `exists()` is
/// `fs::metadata(..).is_ok()`, which collapses *every* stat failure — including
/// the `EACCES` on an ancestor that this tracker exists to record in the first
/// place — into "absent". Inferring "the operator fixed it" from a permission
/// error would drop a still-stuck record and turn `loom-daemon health` GREEN
/// while the worktree still occupies disk. On `Err`, keep the record.
///
/// Split out from the path-taking wrapper purely so the `Err` arm is directly
/// testable: it cannot be provoked deterministically on a real filesystem in a
/// unit test, and it is the arm whose regression would be silent.
#[must_use]
fn is_confirmed_absent(probe: &std::io::Result<bool>) -> bool {
    matches!(probe, Ok(false))
}

/// Whether a tracked record's worktree directory is confirmed gone from disk —
/// the signal that the tracked condition was resolved out of band (#7709).
#[must_use]
fn record_is_orphaned(path: &Path) -> bool {
    is_confirmed_absent(&path.try_exists())
}

/// Drop every tracked record whose worktree directory is confirmed gone (#7709).
///
/// Before this, the *only* code path that cleared a record was
/// [`remove_with_backoff`]'s `Ok(())` arm — which runs only when the reaper's
/// own `enumerate_worktree_dirs` scan still finds the directory. The
/// remediation `loom-daemon health` recommends for a permission-denied stuck
/// removal is an operator's `sudo rm -rf`, after which the directory is never
/// enumerated again: the record, and the DEGRADED health status it causes,
/// outlived the condition that created it until the next daemon restart. The
/// record must never outlive its condition, so the tracker prunes on confirmed
/// absence as well.
///
/// Called at the top of each reap pass so the map cannot accumulate dead
/// entries between status calls; [`stuck_worktree_removals`] prunes again on
/// read so the health surface clears on the very next status call rather than
/// waiting up to a full reap interval.
pub(crate) fn prune_orphaned_removal_records() {
    let mut failures = removal_failures()
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    failures.retain(|path, _| !record_is_orphaned(path));
}

/// Attempt a worktree removal through the shared backoff/tracking state
/// (#7590) — the common body behind both the `issue-<N>` and `pr-<N>`
/// removers in [`super::reap_worktrees_only`].
///
/// Returns `true` iff `attempt` ran AND succeeded this call. When `path` is
/// currently backed off, `attempt` is not invoked at all — skipping the
/// syscalls entirely, not merely downgrading the log line, is the point.
///
/// A cause classified [`is_permission_denied_cause`] backs off immediately
/// (AC1/AC3: cannot self-resolve, so there is nothing to gain by retrying
/// every tick). Any other cause only backs off after [`REMOVAL_FAILURE_CAP`]
/// *consecutive* failures for the same path — a single transient failure (a
/// lock, a race) is retried again next tick exactly as before #7590, and a
/// success at any point clears the run.
pub(crate) fn remove_with_backoff(
    repo_root: &Path,
    kind: WorktreeKind,
    number: u32,
    path: &Path,
    attempt: impl FnOnce() -> Result<(), String>,
) -> bool {
    let now = Utc::now();
    let label = kind.as_str();

    // Fast path: honor an active backoff without running `attempt` at all.
    {
        let failures = removal_failures()
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(record) = failures.get(path) {
            let backed_off = record.permanent || record.attempt_count >= REMOVAL_FAILURE_CAP;
            let elapsed_secs = (now - record.last_attempt_at).num_seconds();
            if backed_off && elapsed_secs < REMOVAL_BACKOFF_SECS {
                // Known-stuck, already surfaced on `loom-daemon health` — quiet
                // by design (#7590 AC4): debug, not warn, for a path that is
                // not going to remove itself between ticks.
                log::debug!(
                    "worktree_reaper: {} skipping {label}-{number} ({}) — backed off after {} \
                     failed attempt(s) (next retry in ~{}s), last cause: {}",
                    repo_root.display(),
                    path.display(),
                    record.attempt_count,
                    REMOVAL_BACKOFF_SECS - elapsed_secs,
                    record.last_cause,
                );
                return false;
            }
        }
    }

    match attempt() {
        Ok(()) => {
            // A removal that actually succeeds — e.g. a transient cause
            // resolved itself, or a `git worktree remove` that finally took —
            // must always clear any prior backoff state; it must never
            // outlive the condition that caused it. (The *other* way a
            // condition gets resolved, an operator deleting the directory by
            // hand, never reaches this arm at all — there is no directory
            // left for the reaper to enumerate, so the prune in
            // [`prune_orphaned_removal_records`] covers it instead, #7709.)
            removal_failures()
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(path);
            true
        }
        Err(cause) => {
            let permanent_this_attempt = is_permission_denied_cause(&cause);
            let mut failures = removal_failures()
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let record =
                failures
                    .entry(path.to_path_buf())
                    .or_insert_with(|| RemovalFailureRecord {
                        repo_root: repo_root.to_path_buf(),
                        kind,
                        number,
                        last_cause: cause.clone(),
                        first_failure_at: now,
                        last_attempt_at: now,
                        attempt_count: 0,
                        permanent: false,
                    });
            record.attempt_count += 1;
            record.last_attempt_at = now;
            record.last_cause = cause.clone();
            record.permanent = record.permanent || permanent_this_attempt;
            let newly_or_still_stuck =
                record.permanent || record.attempt_count >= REMOVAL_FAILURE_CAP;
            let attempt_count = record.attempt_count;
            drop(failures);

            if newly_or_still_stuck {
                log::warn!(
                    "worktree_reaper: {} could not remove {label}-{number} ({}): {cause} — \
                     backing off further retries (next attempt in ~{REMOVAL_BACKOFF_SECS}s); \
                     surfaced on `loom-daemon health` (#7590)",
                    repo_root.display(),
                    path.display(),
                );
            } else {
                log::warn!(
                    "worktree_reaper: {} could not remove {label}-{number} ({}): {cause} \
                     (attempt {attempt_count}/{REMOVAL_FAILURE_CAP} before backing off)",
                    repo_root.display(),
                    path.display(),
                );
            }
            false
        }
    }
}

/// Snapshot every worktree removal currently backed off (#7590 AC2) — the
/// input [`crate::ipc::build_daemon_status`] threads into
/// [`crate::types::DaemonStatusReport::stuck_worktree_reclaims`], and from
/// there into `loom-daemon health`/`status` and the `/api/health` route.
///
/// Only entries that have actually crossed into backed-off state are
/// returned — a lone, still-being-retried transient failure never appears
/// here, matching the same predicate [`remove_with_backoff`] uses to decide
/// whether to skip a retry.
///
/// Prunes confirmed-absent records first (#7709), so an operator who resolves a
/// stuck removal by deleting the directory sees health go GREEN on their very
/// next `loom-daemon health` rather than at the next reap tick (up to
/// [`super::DEFAULT_WORKTREE_REAPER_INTERVAL_SECS`] later) or the next daemon
/// restart. The snapshot therefore mutates the tracker — it already took the
/// lock, so this is a shape change, not a locking change.
#[must_use]
pub fn stuck_worktree_removals() -> Vec<crate::types::StuckWorktreeReclaim> {
    let mut failures = removal_failures()
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    failures.retain(|path, _| !record_is_orphaned(path));
    failures
        .iter()
        .filter(|(_, r)| r.permanent || r.attempt_count >= REMOVAL_FAILURE_CAP)
        .map(|(path, r)| crate::types::StuckWorktreeReclaim {
            repo_root: r.repo_root.clone(),
            kind: r.kind.as_str().to_string(),
            number: r.number,
            path: path.clone(),
            cause: r.last_cause.clone(),
            first_failure_at: r.first_failure_at,
            last_attempt_at: r.last_attempt_at,
            attempt_count: r.attempt_count,
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    // ===================================================================
    // Removal-failure backoff (#7590) + orphan prune (#7709)
    //
    // `REMOVAL_FAILURES` is a process-global keyed by path, shared across the
    // whole test binary. Every test below uses a distinct path (no two tests
    // collide), and — since the map is never cleared wholesale — assertions
    // on [`stuck_worktree_removals`] filter down to the path under test
    // rather than asserting on the map's total size.
    //
    // The paths must be REAL directories: since #7709 a record whose path is
    // confirmed absent is pruned, so the synthetic `/fake/repo/...` fixtures
    // these tests originally used would now be dropped before any assertion
    // could see them. The `repo_root` fixtures stay synthetic on purpose —
    // that field is only ever stored and echoed back, never probed.
    // ===================================================================

    /// A temp root that lives for the whole test binary. Tracked records are
    /// keyed by path and asserted on well after the call that created them, so
    /// the directories must not be reclaimed when a single test returns.
    fn test_root() -> &'static Path {
        static ROOT: OnceLock<tempfile::TempDir> = OnceLock::new();
        ROOT.get_or_init(|| tempfile::tempdir().unwrap()).path()
    }

    fn unique_test_path(label: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = test_root()
            .join(".loom/worktrees")
            .join(format!("{label}-{n}"));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn is_permission_denied_cause_matches_the_real_remove_dir_all_message() {
        // The exact wording `clean::cleanup_worktree` propagates from
        // `std::fs::remove_dir_all`'s `Err` on a real permission failure
        // (`clean.rs`'s untracked-orphan-directory fallback).
        assert!(is_permission_denied_cause(
            "git worktree remove failed (fatal: ...); direct removal of the untracked worktree \
             directory also failed: Permission denied (os error 13)"
        ));
        // Case-insensitive, since not every OS/locale capitalizes identically.
        assert!(is_permission_denied_cause("permission denied"));
        // A merely-repeated, unrelated cause must NOT be misclassified.
        assert!(!is_permission_denied_cause("fatal: not a working tree"));
        assert!(!is_permission_denied_cause("resource busy or locked"));
    }

    #[test]
    fn a_single_transient_failure_is_retried_next_tick_not_backed_off() {
        // AC3 of #7590: one ordinary (non-permission) failure must not be
        // penalized — `remove_with_backoff` must invoke `attempt` again on
        // the very next call for the same path.
        let repo_root = PathBuf::from("/fake/repo");
        let path = unique_test_path("issue-1");
        let calls = std::cell::Cell::new(0);

        let ok1 = remove_with_backoff(&repo_root, WorktreeKind::Issue, 1, &path, || {
            calls.set(calls.get() + 1);
            Err("resource busy or locked".to_string())
        });
        assert!(!ok1);
        assert_eq!(calls.get(), 1);

        // A single failure is well under `REMOVAL_FAILURE_CAP` — the next
        // call must invoke `attempt` again, not skip it.
        let ok2 = remove_with_backoff(&repo_root, WorktreeKind::Issue, 1, &path, || {
            calls.set(calls.get() + 1);
            Err("resource busy or locked".to_string())
        });
        assert!(!ok2);
        assert_eq!(calls.get(), 2, "a lone transient failure must still retry next tick");

        // And it must not (yet) be surfaced as stuck.
        assert!(!stuck_worktree_removals().iter().any(|r| r.path == path));
    }

    #[test]
    fn a_permission_denied_failure_backs_off_immediately_without_reattempting() {
        // AC1/AC3 of #7590: a permission-class failure cannot self-resolve,
        // so it backs off on the very FIRST failure — unlike the transient
        // case above, which tolerates a single failure.
        let repo_root = PathBuf::from("/fake/repo");
        let path = unique_test_path("issue-2");
        let calls = std::cell::Cell::new(0);

        let ok1 = remove_with_backoff(&repo_root, WorktreeKind::Issue, 2, &path, || {
            calls.set(calls.get() + 1);
            Err("Permission denied (os error 13)".to_string())
        });
        assert!(!ok1);
        assert_eq!(calls.get(), 1);

        // The very next call must skip `attempt` entirely — the whole point
        // is to stop paying the removal syscalls, not just quiet the log.
        let ok2 = remove_with_backoff(&repo_root, WorktreeKind::Issue, 2, &path, || {
            calls.set(calls.get() + 1);
            Err("Permission denied (os error 13)".to_string())
        });
        assert!(!ok2);
        assert_eq!(calls.get(), 1, "a backed-off path must not re-invoke `attempt` at all");
    }

    #[test]
    fn repeated_unclassified_failures_back_off_once_the_cap_is_reached() {
        // AC1 of #7590: even a cause that is never classified as a
        // permission error must eventually back off once it has repeated
        // `REMOVAL_FAILURE_CAP` times in a row — the fixed retry-count cap.
        let repo_root = PathBuf::from("/fake/repo");
        let path = unique_test_path("issue-3");
        let calls = std::cell::Cell::new(0);
        let cause = || {
            calls.set(calls.get() + 1);
            Err("some unclassified, persistent failure".to_string())
        };

        for _ in 0..REMOVAL_FAILURE_CAP {
            let ok = remove_with_backoff(&repo_root, WorktreeKind::Issue, 3, &path, cause);
            assert!(!ok);
        }
        assert_eq!(calls.get(), u64::from(REMOVAL_FAILURE_CAP));

        // The cap has now been reached — the next call must skip `attempt`.
        let ok_after_cap = remove_with_backoff(&repo_root, WorktreeKind::Issue, 3, &path, cause);
        assert!(!ok_after_cap);
        assert_eq!(
            calls.get(),
            u64::from(REMOVAL_FAILURE_CAP),
            "once the cap is reached, `attempt` must not be re-invoked until the backoff window \
             elapses"
        );
    }

    #[test]
    fn stuck_worktree_removals_reflects_a_backed_off_path() {
        // AC2 of #7590: once backed off, the path must be visible on
        // `stuck_worktree_removals` — the input the health/status surfaces
        // read from — naming the repo, the kind/number, and the cause.
        let repo_root = PathBuf::from("/fake/repo/stuck");
        let path = unique_test_path("issue-4");
        let _ = remove_with_backoff(&repo_root, WorktreeKind::Issue, 4, &path, || {
            Err("Permission denied (os error 13)".to_string())
        });

        let stuck = stuck_worktree_removals();
        let entry = stuck
            .iter()
            .find(|r| r.path == path)
            .expect("the backed-off path must appear in the snapshot");
        assert_eq!(entry.repo_root, repo_root);
        assert_eq!(entry.kind, "issue");
        assert_eq!(entry.number, 4);
        assert!(entry.cause.contains("Permission denied"));
        assert_eq!(entry.attempt_count, 1);
    }

    #[test]
    fn a_successful_removal_clears_prior_failure_state() {
        // The recorded failure state must never survive a removal that
        // actually succeeds — e.g. a transient lock/race resolved itself by
        // the next tick. (A path already fully backed off is, by design, not
        // retried again until the backoff window elapses — see
        // `a_permission_denied_failure_backs_off_immediately_without_reattempting`
        // — so this exercises the pre-backoff case: a single failure below
        // both the permanent classification and `REMOVAL_FAILURE_CAP`.)
        let repo_root = PathBuf::from("/fake/repo");
        let path = unique_test_path("issue-5");

        let ok1 = remove_with_backoff(&repo_root, WorktreeKind::Issue, 5, &path, || {
            Err("resource busy or locked".to_string())
        });
        assert!(!ok1);
        assert!(
            stuck_worktree_removals().iter().all(|r| r.path != path),
            "one transient failure alone must not yet be surfaced as stuck"
        );

        let ok2 = remove_with_backoff(&repo_root, WorktreeKind::Issue, 5, &path, || Ok(()));
        assert!(ok2, "a lone prior failure must not itself block the next retry");
        assert!(
            !stuck_worktree_removals().iter().any(|r| r.path == path),
            "a successful removal must clear the prior failure state"
        );
    }

    #[test]
    fn pr_worktree_removal_failures_are_tracked_and_backed_off_too() {
        // #7590's fix applies identically to the `pr-<N>` remover — a
        // `pr-<N>` worktree is exactly as susceptible to a root-owned,
        // permission-denied nested build-cache directory as an `issue-<N>`
        // one.
        let repo_root = PathBuf::from("/fake/repo");
        let path = unique_test_path("pr-6");

        let ok1 = remove_with_backoff(&repo_root, WorktreeKind::Pr, 6, &path, || {
            Err("Permission denied (os error 13)".to_string())
        });
        assert!(!ok1);

        let stuck = stuck_worktree_removals();
        let entry = stuck
            .iter()
            .find(|r| r.path == path)
            .expect("the backed-off pr-<N> path must appear in the snapshot");
        assert_eq!(entry.kind, "pr");
        assert_eq!(entry.number, 6);
    }

    #[test]
    fn a_stuck_record_clears_once_the_worktree_directory_is_gone() {
        // #7709: the remediation `loom-daemon health` recommends for a
        // permission-denied stuck removal is an operator's `sudo rm -rf
        // <worktree>`. After it, the reaper's own directory scan never yields
        // the path again, so the `Ok(())` arm — the only *other* path that
        // clears a record — can never run. Without the prune, health stays
        // DEGRADED naming a directory that no longer exists until the daemon
        // restarts.
        let repo_root = PathBuf::from("/fake/repo");
        let path = unique_test_path("issue-7");

        let ok = remove_with_backoff(&repo_root, WorktreeKind::Issue, 7, &path, || {
            Err("Permission denied (os error 13)".to_string())
        });
        assert!(!ok);
        assert!(
            stuck_worktree_removals().iter().any(|r| r.path == path),
            "precondition: a permission-denied path must first be surfaced as stuck"
        );

        // The operator removes it by hand, out from under the reaper.
        std::fs::remove_dir_all(&path).unwrap();

        assert!(
            !stuck_worktree_removals().iter().any(|r| r.path == path),
            "a record whose worktree is confirmed gone must stop being surfaced as stuck"
        );
        assert!(
            !removal_failures()
                .lock()
                .unwrap()
                .contains_key(path.as_path()),
            "the snapshot must DROP the dead entry, not merely filter it out of one read"
        );
    }

    #[test]
    fn prune_orphaned_removal_records_keeps_records_whose_worktree_still_exists() {
        // The reap-pass-side prune (#7709): dead entries go, live ones stay.
        // A stale record is not merely cosmetic — `remove_with_backoff`'s fast
        // path is keyed purely on the path, so keeping one for a directory
        // that still exists is load-bearing, and dropping one for a directory
        // that does not is the whole point.
        let repo_root = PathBuf::from("/fake/repo");
        let live = unique_test_path("issue-8");
        let dead = unique_test_path("issue-9");
        for (n, path) in [(8u32, &live), (9u32, &dead)] {
            let ok = remove_with_backoff(&repo_root, WorktreeKind::Issue, n, path, || {
                Err("Permission denied (os error 13)".to_string())
            });
            assert!(!ok);
        }
        std::fs::remove_dir_all(&dead).unwrap();

        prune_orphaned_removal_records();

        let failures = removal_failures().lock().unwrap();
        assert!(
            failures.contains_key(live.as_path()),
            "a record whose worktree is still on disk must survive the prune"
        );
        assert!(
            !failures.contains_key(dead.as_path()),
            "a record whose worktree is confirmed gone must be pruned"
        );
    }

    #[test]
    fn absence_must_be_confirmed_not_inferred_from_a_stat_error() {
        // #7709, AC2: `Path::exists()` collapses EVERY stat failure — including
        // the `EACCES` on an ancestor that this tracker exists to record —
        // into "absent". Pruning on that would turn `loom-daemon health` GREEN
        // while the worktree still occupies disk, which is strictly worse than
        // the stale record the prune was added to fix.
        assert!(
            is_confirmed_absent(&Ok(false)),
            "a confirmed-absent path is the ONLY case that may drop a record"
        );
        assert!(!is_confirmed_absent(&Ok(true)));
        assert!(
            !is_confirmed_absent(&Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))),
            "existence that cannot be determined must KEEP the record"
        );

        // And the path-taking wrapper agrees on a directory that really exists.
        let live = unique_test_path("issue-10");
        assert!(!record_is_orphaned(&live));
        std::fs::remove_dir_all(&live).unwrap();
        assert!(record_is_orphaned(&live));
    }
}
