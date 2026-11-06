//! Named, per-reason markers for role-runner ticks that were skipped rather
//! than run (#4642, #7607, #5028, #6637).
//!
//! Each is a distinct, independently-attributable tally, deliberately never
//! folded into the generic [`RoleTickOutcome::Failure`] count a real
//! invocation failure increments (mirrors the named per-reason skip counters
//! in `sweep_registry.rs`, e.g. `OpenPrDispatchError`/`DispatchBackoffError`).
//! There is no production read-back: the process-wide totals were write-only
//! and were removed; each skip stays attributable through its distinct
//! `RoleTickOutcome` variant on the role health ring.
//!
//! ## Test isolation (#9409)
//!
//! A test that asserts "this tick bumped the counter by exactly one" cannot
//! share a process-wide tally: under the default parallel `cargo test`
//! harness, any other test skipping a tick on another thread would move it
//! too, and the `before + 1` assertion would fail intermittently (CI never
//! saw it, because `nextest` runs one process per test). Under `cfg(test)`
//! every [`SkipCounter::record`] therefore bumps a thread-local tally, and
//! tests assert on [`SkipCounter::on_this_thread`]. A role tick's skip gates
//! run synchronously on the thread that calls `invoke`, which is the test's
//! own thread, so that view is exact.
//!
//! [`RoleTickOutcome::Failure`]: super::RoleTickOutcome::Failure

/// One named skip marker.
pub(crate) struct SkipCounter {
    /// Deliberately non-zero-sized: `key()` identifies a counter by its
    /// address, and distinct zero-sized statics may share one.
    _unique: u8,
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
        Self { _unique: 0 }
    }

    /// Count one skipped tick.
    pub(crate) fn record(&'static self) {
        #[cfg(test)]
        ON_THIS_THREAD.with(|m| *m.borrow_mut().entry(self.key()).or_default() += 1);
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

#[cfg(test)]
mod tests {
    use super::*;

    static PROBE: SkipCounter = SkipCounter::new();

    /// The #9409 contract: another thread's skip never moves this thread's
    /// view, so a `before + 1` delta taken on this thread cannot be disturbed
    /// by a concurrently running test.
    #[test]
    fn another_threads_skip_does_not_move_this_threads_view() {
        let before_here = PROBE.on_this_thread();

        std::thread::spawn(|| PROBE.record()).join().unwrap();
        assert_eq!(PROBE.on_this_thread(), before_here);

        PROBE.record();
        assert_eq!(PROBE.on_this_thread(), before_here + 1);
    }

    /// Distinct counters keep distinct per-thread tallies.
    #[test]
    fn counters_do_not_share_a_per_thread_tally() {
        let other = LOAD_SKIPPED_COUNT.on_this_thread();
        let before = MODEL_RUNTIME_MISMATCH_SKIP_COUNT.on_this_thread();
        MODEL_RUNTIME_MISMATCH_SKIP_COUNT.record();
        assert_eq!(MODEL_RUNTIME_MISMATCH_SKIP_COUNT.on_this_thread(), before + 1);
        assert_eq!(LOAD_SKIPPED_COUNT.on_this_thread(), other);
    }
}
