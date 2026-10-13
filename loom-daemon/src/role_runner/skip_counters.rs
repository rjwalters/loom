//! Process-wide, per-reason tallies of role-runner ticks that were skipped
//! rather than run (#4642, #7607, #5028, #6637).
//!
//! Each is a distinct, independently-attributable tally, deliberately never
//! folded into the generic [`RoleTickOutcome::Failure`] count a real
//! invocation failure increments (mirrors the named per-reason skip counters
//! in `sweep_registry.rs`, e.g. `OpenPrDispatchError`/`DispatchBackoffError`).
//! The daemon never resets them across its lifetime.
//!
//! ## Test isolation (#9409)
//!
//! A test that asserts "this tick bumped the counter by exactly one" cannot
//! read the process-wide total: under the default parallel `cargo test`
//! harness, any other test skipping a tick on another thread moves it too,
//! and the `before + 1` assertion fails intermittently (CI never saw it,
//! because `nextest` runs one process per test). Under `cfg(test)` every
//! [`SkipCounter::record`] therefore also bumps a thread-local tally, and
//! tests assert on [`SkipCounter::on_this_thread`]. A role tick's skip gates
//! run synchronously on the thread that calls `invoke`, which is the test's
//! own thread, so that view is exact. The production total is unchanged.
//!
//! [`RoleTickOutcome::Failure`]: super::RoleTickOutcome::Failure

use std::sync::atomic::{AtomicU64, Ordering};

/// One named skip tally.
pub(crate) struct SkipCounter {
    total: AtomicU64,
}

#[cfg(test)]
thread_local! {
    /// Per-thread view of every [`SkipCounter`], keyed by the counter's
    /// address (each is a `static`, so the address is stable and unique).
    static ON_THIS_THREAD: std::cell::RefCell<std::collections::HashMap<usize, u64>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

impl SkipCounter {
    const fn new() -> Self {
        Self {
            total: AtomicU64::new(0),
        }
    }

    /// Count one skipped tick.
    pub(crate) fn record(&'static self) {
        self.total.fetch_add(1, Ordering::Relaxed);
        #[cfg(test)]
        ON_THIS_THREAD.with(|m| *m.borrow_mut().entry(self.key()).or_default() += 1);
    }

    /// Process-wide total so far.
    pub(crate) fn total(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }

    /// Ticks recorded by the **calling thread** only — the race-free read for
    /// a test's `before`/`after` delta (see the module docs).
    #[cfg(test)]
    pub(crate) fn on_this_thread(&'static self) -> u64 {
        ON_THIS_THREAD.with(|m| m.borrow().get(&self.key()).copied().unwrap_or(0))
    }

    #[cfg(test)]
    fn key(&'static self) -> usize {
        std::ptr::from_ref(self) as usize
    }
}

/// Ticks skipped with [`RoleTickOutcome::NoTokenPool`] (#4642).
///
/// [`RoleTickOutcome::NoTokenPool`]: super::RoleTickOutcome::NoTokenPool
pub(crate) static NO_TOKEN_POOL_SKIP_COUNT: SkipCounter = SkipCounter::new();

/// Ticks skipped with [`RoleTickOutcome::PoolExhausted`] (#7607): a pool
/// present but fully exhausted (every account bad-marked or
/// `.ranking`-hard-excluded) is self-healing, not a code/config defect.
///
/// [`RoleTickOutcome::PoolExhausted`]: super::RoleTickOutcome::PoolExhausted
pub(crate) static POOL_EXHAUSTED_SKIP_COUNT: SkipCounter = SkipCounter::new();

/// Ticks skipped with [`RoleTickOutcome::ModelRuntimeMismatch`] (#5028,
/// follow-up to #5001 AC2/AC3): a permanent config conflict, not a transient
/// failure worth retrying identically forever.
///
/// [`RoleTickOutcome::ModelRuntimeMismatch`]: super::RoleTickOutcome::ModelRuntimeMismatch
pub(crate) static MODEL_RUNTIME_MISMATCH_SKIP_COUNT: SkipCounter = SkipCounter::new();

/// Ticks skipped with [`RoleTickOutcome::LoadSkipped`] (#6637): the tick
/// ceiling fired while the host was measurably saturated, which is evidence
/// against (not for) the invocation itself being broken.
///
/// [`RoleTickOutcome::LoadSkipped`]: super::RoleTickOutcome::LoadSkipped
pub(crate) static LOAD_SKIPPED_COUNT: SkipCounter = SkipCounter::new();

/// Total number of role-runner ticks skipped so far for having no available
/// token pool (see [`RoleTickOutcome::NoTokenPool`]). Exposed for tests and
/// future status surfacing.
///
/// [`RoleTickOutcome::NoTokenPool`]: super::RoleTickOutcome::NoTokenPool
#[must_use]
pub fn no_token_pool_skip_count() -> u64 {
    NO_TOKEN_POOL_SKIP_COUNT.total()
}

/// Total number of role-runner ticks skipped so far because the resolved
/// token pool was present but had zero spawnable accounts (see
/// [`RoleTickOutcome::PoolExhausted`]). Exposed for tests and future status
/// surfacing.
///
/// [`RoleTickOutcome::PoolExhausted`]: super::RoleTickOutcome::PoolExhausted
#[must_use]
pub fn pool_exhausted_skip_count() -> u64 {
    POOL_EXHAUSTED_SKIP_COUNT.total()
}

/// Total number of role-runner ticks skipped so far for a provable
/// model/runtime mismatch (see [`RoleTickOutcome::ModelRuntimeMismatch`]).
/// Exposed for tests and future status surfacing.
///
/// [`RoleTickOutcome::ModelRuntimeMismatch`]: super::RoleTickOutcome::ModelRuntimeMismatch
#[must_use]
pub fn model_runtime_mismatch_skip_count() -> u64 {
    MODEL_RUNTIME_MISMATCH_SKIP_COUNT.total()
}

/// Total number of role-runner ticks skipped so far because the tick ceiling
/// was reached under measured host saturation (see
/// [`RoleTickOutcome::LoadSkipped`]). Exposed for tests and future status
/// surfacing.
///
/// [`RoleTickOutcome::LoadSkipped`]: super::RoleTickOutcome::LoadSkipped
#[must_use]
pub fn load_skipped_count() -> u64 {
    LOAD_SKIPPED_COUNT.total()
}

#[cfg(test)]
mod tests {
    use super::*;

    static PROBE: SkipCounter = SkipCounter::new();

    /// The #9409 contract: another thread's skip moves the process-wide
    /// total but never this thread's view, so a `before + 1` delta taken on
    /// this thread cannot be disturbed by a concurrently running test.
    #[test]
    fn another_threads_skip_moves_the_total_but_not_this_threads_view() {
        let before_total = PROBE.total();
        let before_here = PROBE.on_this_thread();

        std::thread::spawn(|| PROBE.record()).join().unwrap();
        assert_eq!(PROBE.on_this_thread(), before_here);
        assert!(PROBE.total() > before_total);

        PROBE.record();
        assert_eq!(PROBE.on_this_thread(), before_here + 1);
        assert!(PROBE.total() >= before_total + 2);
    }

    /// Distinct counters keep distinct per-thread tallies.
    #[test]
    fn counters_do_not_share_a_per_thread_tally() {
        let other = LOAD_SKIPPED_COUNT.on_this_thread();
        let before = MODEL_RUNTIME_MISMATCH_SKIP_COUNT.on_this_thread();
        MODEL_RUNTIME_MISMATCH_SKIP_COUNT.record();
        assert_eq!(MODEL_RUNTIME_MISMATCH_SKIP_COUNT.on_this_thread(), before + 1);
        assert_eq!(LOAD_SKIPPED_COUNT.on_this_thread(), other);
        assert!(model_runtime_mismatch_skip_count() >= 1);
    }
}
