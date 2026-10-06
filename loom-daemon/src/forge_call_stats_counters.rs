//! Named event counters beside the per-call accounting (W3a).
//!
//! Some signals are not forge calls at all: "a checkout's remote names a
//! renamed repository" (`repo_facts.redirected`) or "our local resolver
//! disagreed with gh's" (`repo_facts.resolver_disagree`). Recording them as a
//! call row would book budget nobody spent, so they are plain monotonic
//! counters keyed by a stable, low-cardinality name. They are process-local
//! and cost no forge call; the emitter also logs each increment, so the daemon
//! log carries them until the metrics export picks the snapshot up.

use std::collections::BTreeMap;

/// Add one to the counter `name` and return its new value.
pub fn bump(name: &'static str) -> u64 {
    with(|m| {
        let v = m.entry(name).or_default();
        *v += 1;
        *v
    })
}

/// The current value of counter `name` (`0` when never bumped).
#[must_use]
pub fn get(name: &str) -> u64 {
    with(|m| m.get(name).copied().unwrap_or(0))
}

/// Every counter, by name.
#[must_use]
pub fn snapshot() -> BTreeMap<String, u64> {
    with(|m| m.iter().map(|(k, v)| ((*k).to_string(), *v)).collect())
}

#[cfg(not(test))]
fn with<R>(f: impl FnOnce(&mut BTreeMap<&'static str, u64>) -> R) -> R {
    use std::sync::{Mutex, OnceLock};
    static COUNTERS: OnceLock<Mutex<BTreeMap<&'static str, u64>>> = OnceLock::new();
    let lock = COUNTERS.get_or_init(|| Mutex::new(BTreeMap::new()));
    let mut guard = lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    f(&mut guard)
}

/// Test builds: per thread, so parallel tests never see each other's counts.
#[cfg(test)]
fn with<R>(f: impl FnOnce(&mut BTreeMap<&'static str, u64>) -> R) -> R {
    thread_local! {
        static COUNTERS: std::cell::RefCell<BTreeMap<&'static str, u64>> =
            const { std::cell::RefCell::new(BTreeMap::new()) };
    }
    COUNTERS.with(|c| f(&mut c.borrow_mut()))
}

#[cfg(test)]
mod tests {
    #[test]
    fn counters_start_at_zero_and_count_up() {
        assert_eq!(super::get("test.counter"), 0);
        assert_eq!(super::bump("test.counter"), 1);
        assert_eq!(super::bump("test.counter"), 2);
        assert_eq!(super::snapshot().get("test.counter"), Some(&2));
    }
}
