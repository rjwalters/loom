//! Named event counters beside the per-call accounting (W3a).
//!
//! Some signals are not forge calls at all: "a checkout's remote names a
//! renamed repository" (`repo_facts.redirected`) or "our local resolver
//! disagreed with gh's" (`repo_facts.resolver_disagree`). Recording them as a
//! call row would book budget nobody spent, so they are plain monotonic
//! counters keyed by a stable, low-cardinality name. They are process-local
//! and cost no forge call; the emitter also logs each increment. The
//! rate-limit tick exports them as `loom.forge.facade.events` (a delta
//! counter labelled `reason` = the counter name, [`drain_deltas`]).

use std::collections::BTreeMap;

/// `name` -> `(value, value at the last export)`.
type Store = BTreeMap<&'static str, (u64, u64)>;

/// Add one to the counter `name` and return its new value.
pub fn bump(name: &'static str) -> u64 {
    with(|m| {
        let v = m.entry(name).or_default();
        v.0 += 1;
        v.0
    })
}

/// Add `n` to the counter `name` (a no-op for `0`).
pub fn add(name: &'static str, n: u64) {
    if n > 0 {
        with(|m| {
            let v = m.entry(name).or_default();
            v.0 = v.0.saturating_add(n);
        });
    }
}

/// The current value of counter `name` (`0` when never bumped).
#[must_use]
pub fn get(name: &str) -> u64 {
    with(|m| m.get(name).map_or(0, |v| v.0))
}

/// Every counter, by name.
#[must_use]
pub fn snapshot() -> BTreeMap<String, u64> {
    with(|m| m.iter().map(|(k, v)| ((*k).to_string(), v.0)).collect())
}

/// What each counter gained since the previous drain, by name (only the
/// ones that moved); marks those values exported. The value itself is never
/// reset, so [`get`] and the log lines keep counting since start.
#[must_use]
pub fn drain_deltas() -> Vec<(&'static str, u64)> {
    with(|m| {
        m.iter_mut()
            .filter(|(_, v)| v.0 > v.1)
            .map(|(k, v)| {
                let delta = v.0 - v.1;
                v.1 = v.0;
                (*k, delta)
            })
            .collect()
    })
}

#[cfg(not(test))]
fn with<R>(f: impl FnOnce(&mut Store) -> R) -> R {
    use std::sync::{Mutex, OnceLock};
    static COUNTERS: OnceLock<Mutex<Store>> = OnceLock::new();
    let lock = COUNTERS.get_or_init(|| Mutex::new(BTreeMap::new()));
    let mut guard = lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    f(&mut guard)
}

/// Test builds: per thread, so parallel tests never see each other's counts.
#[cfg(test)]
fn with<R>(f: impl FnOnce(&mut Store) -> R) -> R {
    thread_local! {
        static COUNTERS: std::cell::RefCell<Store> =
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

    #[test]
    fn a_drain_reports_what_moved_since_the_last_drain() {
        let _ = super::drain_deltas();
        super::bump("test.drain.a");
        super::bump("test.drain.a");
        super::bump("test.drain.b");
        let mut d = super::drain_deltas();
        d.sort_unstable();
        assert_eq!(d, vec![("test.drain.a", 2), ("test.drain.b", 1)]);
        assert!(super::drain_deltas().is_empty(), "nothing moved since");
        super::bump("test.drain.a");
        assert_eq!(super::drain_deltas(), vec![("test.drain.a", 1)]);
        assert_eq!(super::get("test.drain.a"), 3, "the value keeps counting");
    }
}
