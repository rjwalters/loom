//! Free-space precheck before a daemon-initiated `git fetch` (#10995).
//!
//! A fetch that runs out of disk aborts and leaves its partial pack behind as
//! `.git/objects/pack/tmp_pack_*`, which nothing but a two-week-old `gc`
//! removes. The daemon retries its fetches every tick, so on a full disk each
//! retry added another partial pack: 35 GiB of them on loom-worker-1. This
//! module is the one shared gate the daemon's managed-checkout fetch sites ask
//! first. Below the reaper's free-space floor
//! ([`crate::worktree_reaper::resolve_disk_warn_free_gb`]) the fetch is
//! skipped and logged; the caller treats the skip as "not evaluated", never as
//! a git failure.
//!
//! An unmeasurable volume (`df` failed) never skips a fetch: unknown is not
//! zero (#4164), the same contract as every other disk probe in the daemon.
//! The debris an earlier fetch already left is reclaimed separately, by
//! [`crate::git_tmp_reclaim`].

use std::path::Path;
use std::process::{Command, Stdio};

/// Pure decision: `Some(reason)` when a fetch into a checkout whose volume has
/// `free_gb` free should be skipped against `floor_gb`.
#[must_use]
pub fn skip_reason_for(free_gb: Option<u64>, floor_gb: u64) -> Option<String> {
    let free = free_gb?;
    (free < floor_gb).then(|| {
        format!(
            "skipped git fetch: {free}G free on the checkout's volume is below the {floor_gb}G \
             floor (diskWarnFreeGb) — a fetch on a full disk leaves a partial tmp_pack_* behind \
             (#10995)"
        )
    })
}

/// [`skip_reason_for`] with the free-space probe and floor injected, so call
/// sites' tests can drive both sides of the floor.
#[must_use]
pub fn skip_reason_with(
    repo_root: &Path,
    free_gb: &dyn Fn(&Path) -> Option<u64>,
    floor_gb: u64,
) -> Option<String> {
    skip_reason_for(free_gb(repo_root), floor_gb)
}

/// Production precheck for a fetch into `repo_root`: probes the checkout's own
/// volume and the repo's resolved floor. Logs at `warn` when it skips, so the
/// skip is visible in SigNoz next to the reclaim that should follow it.
#[must_use]
pub fn skip_reason(repo_root: &Path) -> Option<String> {
    let config = crate::worktree_reaper::read_worktree_reaper_config(repo_root);
    let floor_gb = crate::worktree_reaper::resolve_disk_warn_free_gb(&config);
    let reason = skip_reason_with(repo_root, &probe_free_gb, floor_gb);
    if let Some(r) = &reason {
        log::warn!("fetch_headroom: {} {r}", repo_root.display());
    }
    reason
}

/// The checkout volume's free space. Unit-test builds never consult the
/// host's real disk: the existing fetch-path tests must not flip to "skipped"
/// on a CI runner that happens to sit below the floor, so a test that wants
/// the precheck pins the value with [`test_override::with_free_gb`].
#[cfg(not(test))]
fn probe_free_gb(repo_root: &Path) -> Option<u64> {
    crate::disk_headroom::path_free_gb(repo_root)
}

#[cfg(test)]
fn probe_free_gb(_repo_root: &Path) -> Option<u64> {
    test_override::get()
}

/// Best-effort quiet `git <args>` in `repo_root` (stdio nulled, result
/// ignored), unless [`skip_reason`] says the volume is below the floor.
/// Returns whether the command was attempted.
pub fn fetch_quietly(repo_root: &Path, args: &[&str]) -> bool {
    if skip_reason(repo_root).is_some() {
        return false;
    }
    let _ = Command::new("git")
        .args(args)
        .current_dir(repo_root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    true
}

/// Test-only: pin the free space [`skip_reason`] sees on the current thread,
/// so a call site's test can drive the real precheck below the floor without
/// a process-global env var racing other tests.
#[cfg(test)]
pub(crate) mod test_override {
    use std::cell::Cell;

    thread_local! {
        static FREE_GB: Cell<Option<u64>> = const { Cell::new(None) };
    }

    pub(crate) fn get() -> Option<u64> {
        FREE_GB.with(Cell::get)
    }

    /// Pin (or unpin) the probe from inside a running test, for a disk that
    /// fills partway through the code under test.
    pub(crate) fn set(free_gb: Option<u64>) {
        FREE_GB.with(|c| c.set(free_gb));
    }

    /// Run `f` with the probe pinned to `free_gb`.
    pub(crate) fn with_free_gb<T>(free_gb: u64, f: impl FnOnce() -> T) -> T {
        FREE_GB.with(|c| c.set(Some(free_gb)));
        let out = f();
        FREE_GB.with(|c| c.set(None));
        out
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn below_the_floor_skips_with_a_reason() {
        let r = skip_reason_for(Some(3), 20).expect("3G < 20G must skip");
        assert!(r.contains("3G free") && r.contains("20G floor"), "{r}");
    }

    #[test]
    fn at_or_above_the_floor_fetches() {
        assert!(skip_reason_for(Some(20), 20).is_none());
        assert!(skip_reason_for(Some(500), 20).is_none());
    }

    #[test]
    fn unmeasurable_free_space_never_skips() {
        assert!(skip_reason_for(None, 20).is_none(), "unknown != zero (#4164)");
    }

    #[test]
    fn a_zero_floor_never_skips() {
        assert!(skip_reason_for(Some(0), 0).is_none());
    }

    #[test]
    fn injected_probe_is_consulted_for_the_given_root() {
        let seen = Cell::new(false);
        let probe = |p: &Path| {
            seen.set(p == Path::new("/repo"));
            Some(1)
        };
        assert!(skip_reason_with(Path::new("/repo"), &probe, 20).is_some());
        assert!(seen.get());
        let roomy = |_: &Path| Some(100);
        assert!(skip_reason_with(Path::new("/repo"), &roomy, 20).is_none());
    }

    #[test]
    fn fetch_quietly_does_not_run_git_below_the_floor() {
        // A non-git directory: were `git fetch` invoked it would simply fail,
        // so the observable is the return value — whether it was attempted.
        let tmp = tempfile::tempdir().unwrap();
        let attempted = test_override::with_free_gb(0, || {
            fetch_quietly(tmp.path(), &["fetch", "origin", "--", "main"])
        });
        assert!(!attempted, "below the floor the fetch must not be attempted");
        let attempted = test_override::with_free_gb(10_000, || {
            fetch_quietly(tmp.path(), &["fetch", "origin", "--", "main"])
        });
        assert!(attempted, "above the floor the fetch runs");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "fetch_headroom/call_sites_tests.rs"]
mod call_sites_tests;
