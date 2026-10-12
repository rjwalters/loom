//! A bounded wait for a periodic loop's blocking cycle (Issue #10414).
//!
//! The ETA fleet refresh awaited its `spawn_blocking` cycle with no bound.
//! A cycle stuck in a forge read, the SigNoz walk or a lock stalled the loop
//! forever, and nothing logged it. A blocking thread cannot be cancelled, so
//! the bound cannot stop the cycle. What it does:
//!
//! - **Reports the overrun** once the cycle has run longer than the bound, so
//!   the caller can log it and count a fault.
//! - **Never stacks a second cycle**: while the overrunning one is still
//!   running, every later tick is [`CycleTick::StillRunning`] and starts
//!   nothing, so two cycles never contend for the same state lock.
//! - **Collects the late result** on the first tick after the stuck cycle
//!   finally returns, then starts the next cycle as normal.

use std::time::Duration;

use tokio::task::{JoinError, JoinHandle};
use tokio::time::Instant;

/// What one tick of a guarded loop did.
#[derive(Debug)]
pub enum CycleTick<T> {
    /// A cycle ran and finished within the bound (or a late one was
    /// collected): its result.
    Finished(Result<T, JoinError>),
    /// The cycle started this tick is still running past the bound. It is
    /// kept, and the next ticks wait for it.
    Overran {
        /// How long it had run when the bound passed.
        running_for: Duration,
    },
    /// An earlier overrunning cycle is still running; nothing was started.
    StillRunning {
        /// How long it has been running.
        running_for: Duration,
    },
}

/// One loop's guard: at most one cycle in flight, each awaited for at most
/// `bound`.
#[derive(Debug)]
pub struct CycleGuard<T> {
    bound: Duration,
    running: Option<(JoinHandle<T>, Instant)>,
}

impl<T> CycleGuard<T> {
    /// A guard that waits at most `bound` for each cycle.
    #[must_use]
    pub fn new(bound: Duration) -> Self {
        Self {
            bound,
            running: None,
        }
    }

    /// Whether an overrunning cycle is still held.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    /// One tick: collect a held cycle if it finished (returning its result
    /// without starting another), report it if it is still running, else
    /// `start` a new cycle and wait for it up to the bound.
    pub async fn tick(&mut self, start: impl FnOnce() -> JoinHandle<T>) -> CycleTick<T> {
        if let Some((handle, started)) = self.running.take() {
            if !handle.is_finished() {
                let running_for = started.elapsed();
                self.running = Some((handle, started));
                return CycleTick::StillRunning { running_for };
            }
            return CycleTick::Finished(handle.await);
        }
        let started = Instant::now();
        let mut handle = start();
        match tokio::time::timeout(self.bound, &mut handle).await {
            Ok(result) => CycleTick::Finished(result),
            Err(_) => {
                let running_for = started.elapsed();
                self.running = Some((handle, started));
                CycleTick::Overran { running_for }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[tokio::test]
    async fn a_cycle_within_the_bound_finishes() {
        let mut guard = CycleGuard::new(Duration::from_secs(5));
        let tick = guard.tick(|| tokio::task::spawn_blocking(|| 3)).await;
        assert!(matches!(tick, CycleTick::Finished(Ok(3))));
        assert!(!guard.is_running());
    }

    /// The #10414 wedge: a cycle that blocks past the bound is reported, and
    /// no second cycle starts beside it until it returns.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_wedged_cycle_is_reported_and_never_stacked() {
        let started = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(std::sync::Barrier::new(2));
        let mut guard = CycleGuard::new(Duration::from_millis(30));
        let start = |started: Arc<AtomicUsize>, release: Arc<std::sync::Barrier>| {
            move || {
                started.fetch_add(1, Ordering::SeqCst);
                tokio::task::spawn_blocking(move || {
                    release.wait();
                    "late"
                })
            }
        };
        let first = guard.tick(start(started.clone(), release.clone())).await;
        assert!(matches!(first, CycleTick::Overran { .. }), "{first:?}");
        assert!(guard.is_running());
        for _ in 0..3 {
            let tick = guard.tick(start(started.clone(), release.clone())).await;
            assert!(matches!(tick, CycleTick::StillRunning { .. }), "{tick:?}");
        }
        assert_eq!(started.load(Ordering::SeqCst), 1, "no second concurrent cycle");
        // Unstick it; the next tick collects the late result.
        release.wait();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let collected = guard.tick(start(started.clone(), release.clone())).await;
        assert!(matches!(collected, CycleTick::Finished(Ok("late"))), "{collected:?}");
        assert_eq!(started.load(Ordering::SeqCst), 1, "collecting does not start one");
        assert!(!guard.is_running());
    }
}
