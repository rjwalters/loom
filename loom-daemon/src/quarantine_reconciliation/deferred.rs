//! The startup quarantine pass, retried once after a breaker trip (Issue
//! #11973).
//!
//! # The gap
//!
//! The startup pass ([`super::forge::reconcile_workspace`]) skips a workspace
//! while the shared GitHub rate-limit breaker is cooling (#4429), and nothing
//! ran it again: a host that started under a trip never released an expired
//! park nor restored a live one ([`super::restored`]) until its next restart.
//!
//! # The rule
//!
//! The roots the breaker skipped are kept. Once, and only once per process,
//! a background thread waits for the breaker to clear and then runs the pass
//! over those roots. It does no forge work while the trip lasts (it only reads
//! the breaker's in-memory window), and it does not retry a root that a fresh
//! trip skips again: the periodic sweeps' own TTL expiry covers what is left,
//! and a pass that chased the breaker would add load to the very limit that
//! tripped it.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Shortest wait between breaker checks.
const MIN_POLL: Duration = Duration::from_secs(1);

/// Longest wait between breaker checks: a cooldown can be cut short (a
/// contradicted trip, #8997), so the thread re-reads the window at least this
/// often instead of sleeping to a release time that may have moved.
const MAX_POLL: Duration = Duration::from_secs(60);

/// Set once the retry has been scheduled, so the process runs it at most once.
static SCHEDULED: AtomicBool = AtomicBool::new(false);

/// One startup pass over a set of roots.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Sweep {
    pub checked: usize,
    pub released: usize,
    /// Roots the breaker kept the pass from reading.
    pub skipped: Vec<PathBuf>,
}

/// Run `pass` over every root, collecting the totals and the roots it
/// skipped for the breaker (`None`).
pub(crate) fn sweep(
    roots: &[PathBuf],
    mut pass: impl FnMut(&Path) -> Option<(usize, usize)>,
) -> Sweep {
    let mut out = Sweep::default();
    for root in roots {
        match pass(root) {
            Some((checked, released)) => {
                out.checked += checked;
                out.released += released;
            }
            None => out.skipped.push(root.clone()),
        }
    }
    out
}

/// How long to wait before re-reading the process-global breaker, or `None`
/// when it is not suppressing forge polling now. Reads memory only.
fn global_wait() -> Option<Duration> {
    let snapshot = crate::rate_limit_breaker::global_snapshot()?;
    if !snapshot.suppressed {
        return None;
    }
    let left = snapshot
        .cooldown_until
        .and_then(|until| (until - chrono::Utc::now()).to_std().ok())
        .unwrap_or(MAX_POLL);
    Some(left)
}

/// Wait while `wait` reports the breaker cooling (sleeping each reported
/// interval, clamped to [`MIN_POLL`]..=[`MAX_POLL`]), then run `pass` once
/// over `roots`. Returns the one pass's result; a root it skips again is not
/// retried.
pub(crate) fn run_once_after_breaker(
    roots: &[PathBuf],
    mut wait: impl FnMut() -> Option<Duration>,
    mut sleep: impl FnMut(Duration),
    pass: impl FnMut(&Path) -> Option<(usize, usize)>,
) -> Sweep {
    while let Some(left) = wait() {
        sleep(left.clamp(MIN_POLL, MAX_POLL));
    }
    sweep(roots, pass)
}

/// Schedule the one retry of the startup pass over `roots`, the roots the
/// breaker skipped (Issue #11973). Returns whether a retry was scheduled:
/// `false` when `roots` is empty or a retry was already scheduled in this
/// process.
pub(crate) fn schedule(gh_bin: PathBuf, roots: Vec<PathBuf>) -> bool {
    if roots.is_empty() || SCHEDULED.swap(true, Ordering::SeqCst) {
        return false;
    }
    log::info!(
        "quarantine_reconciliation: startup pass skipped {} workspace(s) for the rate-limit \
         breaker — running it once the breaker clears (#11973)",
        roots.len()
    );
    let spawned = std::thread::Builder::new()
        .name("quarantine-reconcile-deferred".into())
        .spawn(move || {
            let result = run_once_after_breaker(&roots, global_wait, std::thread::sleep, |root| {
                super::forge::reconcile_workspace_pass(&gh_bin, root)
            });
            log_result(&result, roots.len());
        });
    if let Err(e) = spawned {
        log::warn!(
            "quarantine_reconciliation: could not start the deferred startup pass ({e}); the \
             skipped workspace(s) wait for the next restart (#11973)"
        );
    }
    true
}

fn log_result(result: &Sweep, roots: usize) {
    log::info!(
        "quarantine_reconciliation: deferred startup pass checked {} loom:blocked issue(s) \
         across {roots} workspace(s), released {} (#11973)",
        result.checked,
        result.released
    );
    if !result.skipped.is_empty() {
        log::warn!(
            "quarantine_reconciliation: the breaker tripped again during the deferred startup \
             pass; {} workspace(s) not retried (#11973)",
            result.skipped.len()
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn roots(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    /// The acceptance case: a pass the breaker skipped runs exactly once,
    /// after the breaker clears, and makes no pass call while it cools.
    #[test]
    fn a_skipped_pass_runs_once_after_the_breaker_clears() {
        let events = RefCell::new(Vec::<String>::new());
        let cooling = RefCell::new(3u32);
        let result = run_once_after_breaker(
            &roots(&["/w/a", "/w/b"]),
            || {
                let mut left = cooling.borrow_mut();
                (*left > 0).then(|| {
                    *left -= 1;
                    Duration::from_secs(600)
                })
            },
            |d| events.borrow_mut().push(format!("sleep {}", d.as_secs())),
            |root| {
                events.borrow_mut().push(format!("pass {}", root.display()));
                Some((2, 1))
            },
        );
        assert_eq!(
            *events.borrow(),
            ["sleep 60", "sleep 60", "sleep 60", "pass /w/a", "pass /w/b"],
            "no pass while cooling (each wait clamped to MAX_POLL), then one per root"
        );
        assert_eq!(
            result,
            Sweep {
                checked: 4,
                released: 2,
                skipped: Vec::new()
            }
        );
    }

    /// A cleared breaker runs the pass at once; a root a fresh trip skips is
    /// reported, never retried.
    #[test]
    fn a_root_skipped_again_is_not_retried() {
        let calls = RefCell::new(0u32);
        let result = run_once_after_breaker(
            &roots(&["/w/a", "/w/b"]),
            || None,
            |_| panic!("no wait when the breaker is clear"),
            |root| {
                *calls.borrow_mut() += 1;
                (root != Path::new("/w/b")).then_some((1, 0))
            },
        );
        assert_eq!(*calls.borrow(), 2, "one call per root, no second round");
        assert_eq!(result.skipped, roots(&["/w/b"]));
        assert_eq!(result.checked, 1);
    }

    /// A short remaining cooldown is still waited for at least MIN_POLL.
    #[test]
    fn waits_are_clamped_to_the_poll_bounds() {
        let waits = RefCell::new(vec![Duration::ZERO, Duration::from_secs(5)]);
        let slept = RefCell::new(Vec::new());
        run_once_after_breaker(
            &[],
            || waits.borrow_mut().pop(),
            |d| slept.borrow_mut().push(d),
            |_| Some((0, 0)),
        );
        assert_eq!(*slept.borrow(), [Duration::from_secs(5), MIN_POLL]);
    }

    /// The startup sweep keeps exactly the roots the pass skipped.
    #[test]
    fn sweep_collects_the_skipped_roots() {
        let all = roots(&["/w/a", "/w/b", "/w/c"]);
        let out = sweep(&all, |root| (root != Path::new("/w/b")).then_some((3, 1)));
        assert_eq!(out.skipped, roots(&["/w/b"]));
        assert_eq!((out.checked, out.released), (6, 2));
    }

    /// Scheduling is a no-op for an empty set and at most once per process.
    #[test]
    fn nothing_to_retry_schedules_nothing() {
        assert!(!schedule(PathBuf::from("gh"), Vec::new()));
    }
}
