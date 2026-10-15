//! One host budget for every agent the daemon starts (#11911).
//!
//! Operator ruling (2am#4463): the budget is **one configured number** per
//! host, set in the fleet store from experience (20 to start), not a slot
//! calculation. It is `autonomous.workFinder.maxConcurrent` (or
//! `hyperparameters.dispatch.maxConcurrent`, or
//! `LOOM_WORK_FINDER_MAX_CONCURRENT`), default 20
//! ([`crate::work_finder::DEFAULT_WORK_FINDER_MAX_CONCURRENT`]). Disk and RAM
//! headroom no longer size it; an exhausted disk or RAM still halts admission
//! ([`crate::work_finder::resolve_dynamic_max_concurrent`]).
//!
//! - **Steps** get the budget less the role-runner agents in flight
//!   ([`step_capacity`]).
//! - **Role-runner agents** (Champion non-item, Guide, Auditor, Hermit,
//!   Architect) get the budget less the registry occupancy (steps, plus any
//!   sweep an attended session dispatched) the last work-finder tick saw
//!   ([`role_ceiling`]).
//!
//! The work finder publishes the budget and its occupancy on every tick
//! ([`publish`]). Before its first tick, or on a host whose work finder is
//! off, the role runner uses the root's configured budget less the occupancy
//! the startup adoption seeded ([`seed_occupancy`]).
//!
//! `autonomous.roleRunner.maxConcurrent` and `LOOM_ROLE_RUNNER_MAX_CONCURRENT`
//! are retired: parsed and ignored with a warning ([`crate::retired_config`]).
//! `loom.dispatch.idle_slots` stays `budget − occupancy` for one more release;
//! it overstates idle while role runs are in flight.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

/// The last work-finder tick's budget and occupancy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Published {
    /// The tick's dynamic cap.
    pub budget: usize,
    /// Registry slots in use at the tick: steps, plus any sweep an attended
    /// session dispatched.
    pub occupancy: usize,
}

static LAST: Mutex<Option<Published>> = Mutex::new(None);
static SEEDED: AtomicUsize = AtomicUsize::new(0);

/// Record the work finder's budget and occupancy for this tick.
pub fn publish(budget: usize, occupancy: usize) {
    *LAST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Published { budget, occupancy });
}

/// Record the registries' live occupancy found at startup, so the role runner's
/// first admissions before any work-finder tick count it.
pub fn seed_occupancy(occupancy: usize) {
    SEEDED.store(occupancy, Ordering::Relaxed);
}

/// What the last tick published, if any tick has.
#[must_use]
pub fn published() -> Option<Published> {
    *LAST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The slots steps may fill this tick: the budget less the role-runner agents
/// in flight. Pure.
#[must_use]
pub fn step_capacity(budget: usize, role_runs: usize) -> usize {
    budget.saturating_sub(role_runs)
}

/// The role-runner ceiling `published` leaves: the budget less what the
/// registries hold. `None` before any tick. Pure.
#[must_use]
pub fn role_ceiling_from(published: Option<Published>) -> Option<usize> {
    published.map(|p| p.budget.saturating_sub(p.occupancy))
}

/// The role runner's ceiling for an admission at `root`: what the last
/// work-finder tick left ([`role_ceiling_from`]), else `root`'s configured
/// budget less the occupancy seeded at startup.
#[must_use]
pub fn role_ceiling(root: &Path) -> usize {
    role_ceiling_from(published())
        .unwrap_or_else(|| configured(root).saturating_sub(SEEDED.load(Ordering::Relaxed)))
}

/// Log the resolved budget when it changes (at start, and on a config edit).
pub fn log_resolved(budget: usize) {
    static LOGGED: AtomicUsize = AtomicUsize::new(usize::MAX);
    if LOGGED.swap(budget, Ordering::Relaxed) != budget {
        log::info!(
            "host_budget: one host budget = {budget} (autonomous.workFinder.maxConcurrent; \
             steps and role-runner agents share it, #11911)"
        );
    }
}

/// `root`'s configured budget: its `workFinder.maxConcurrent` (env > config >
/// default 20).
#[must_use]
pub fn configured(root: &Path) -> usize {
    crate::work_finder::resolve_max_concurrent_with_config(
        &crate::work_finder::read_work_finder_config(root),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_and_role_runs_share_the_budget() {
        assert_eq!(step_capacity(8, 3), 5);
        let p = Some(Published {
            budget: 8,
            occupancy: 6,
        });
        assert_eq!(role_ceiling_from(p), Some(2));
        assert_eq!(step_capacity(2, 5), 0);
        let over = Some(Published {
            budget: 2,
            occupancy: 5,
        });
        assert_eq!(role_ceiling_from(over), Some(0));
        assert_eq!(role_ceiling_from(None), None, "no tick yet: the caller falls back");
    }
}
