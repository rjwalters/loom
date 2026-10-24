//! The host-wide launch gap for concurrent step spawns (#12298, #3887).
//!
//! The dispatch stagger (#3887) keeps child launches at least
//! `dispatchStaggerMs` apart because children launched within about a
//! second of each other can hang at startup, and the suspected causes (the
//! token pool's files, `mcp-loom/dist`, the keychain) are host-wide. Its
//! state, `last_spawn_at`, lives on each repo's registry. A step tick that
//! spawns in several repos at once (`spawnConcurrency` > 1) would launch
//! children in different registries within milliseconds, so those launches
//! also pass through this one process-wide gate: only the launch itself
//! (the stagger and `Command::spawn`) is serialised, at least the gap
//! apart; the resolvers and the pre-claim keep overlapping.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// One process-wide launch gap.
#[derive(Debug, Default)]
pub struct LaunchGate {
    last: Mutex<Option<Instant>>,
}

/// The gate concurrent step spawns launch through.
pub static HOST_LAUNCH_GATE: LaunchGate = LaunchGate::new();

impl LaunchGate {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            last: Mutex::new(None),
        }
    }

    /// Run `launch` at least `gap` after the previous launch through this
    /// gate, holding the gate while it runs so no other launch through it
    /// can start in between. A zero `gap` (the stagger disabled) still
    /// serialises the launches but never waits.
    pub fn launch<T>(&self, gap: Duration, launch: impl FnOnce() -> T) -> T {
        let mut last = self
            .last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let wait = super::super::dispatch::stagger_wait(*last, gap, Instant::now());
        if !wait.is_zero() {
            log::debug!(
                "sweep_registry: staggering step launch by {}ms across repos (#3887, #12298)",
                wait.as_millis()
            );
            std::thread::sleep(wait);
        }
        *last = Some(Instant::now());
        launch()
    }

    /// When the last launch through the gate started.
    #[cfg(test)]
    pub(crate) fn last_launch(&self) -> Option<Instant> {
        *self
            .last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Two launches started at the same moment, as a concurrent round's
    /// spawns in two repos are, end up at least the gap apart.
    #[test]
    fn concurrent_launches_are_at_least_the_gap_apart() {
        let gate = Arc::new(LaunchGate::new());
        let gap = Duration::from_millis(250);
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let handles: Vec<_> = (0..3)
            .map(|_| {
                let (gate, barrier) = (gate.clone(), barrier.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    gate.launch(gap, Instant::now)
                })
            })
            .collect();
        let mut at: Vec<Instant> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        at.sort();
        for pair in at.windows(2) {
            assert!(pair[1].duration_since(pair[0]) >= gap, "{:?}", pair[1] - pair[0]);
        }
    }

    /// The first launch never waits, and a disabled stagger never waits.
    #[test]
    fn the_first_launch_and_a_zero_gap_do_not_wait() {
        let gate = LaunchGate::new();
        let started = Instant::now();
        gate.launch(Duration::from_secs(5), || ());
        gate.launch(Duration::ZERO, || ());
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
