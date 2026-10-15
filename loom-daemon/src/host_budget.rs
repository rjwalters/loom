//! One host budget for every agent the daemon starts (#11911).
//!
//! Until #11911 a host had two ceilings: `autonomous.workFinder.maxConcurrent`
//! bounded the work finder's sweeps (and, since #11909, its steps), and
//! `autonomous.roleRunner.maxConcurrent` (#6102) bounded the role runner's
//! agents beside it, so a host's real agent capacity was their sum. Step
//! dispatch moved every item role (Curator, Judge, Doctor, Champion's
//! promotion and merge, Builder) into the work finder, and the role runner
//! keeps only the roles with no item to draw (Champion non-item, Guide,
//! Auditor, Hermit, Architect). Both now draw from **one** budget:
//!
//! - **The budget** is the work finder's dynamic cap: `min(maxConcurrent,
//!   disk cap, RAM cap)` ([`crate::work_finder::resolve_dynamic_max_concurrent`]),
//!   with `autonomous.workFinder.maxConcurrent` (or
//!   `hyperparameters.dispatch.maxConcurrent`, or
//!   `LOOM_WORK_FINDER_MAX_CONCURRENT`) the operator's knob.
//! - **Steps** get the budget less the role-runner agents in flight
//!   ([`step_capacity`]).
//! - **Role-runner agents** get the budget less the steps (and any attended
//!   sweep) the last work-finder tick saw in flight ([`role_ceiling`]).
//!
//! The work finder publishes the budget and its occupancy on every tick
//! ([`publish`]). Before its first tick, or on a host whose work finder is
//! off, the role runner falls back to the root's configured
//! `workFinder.maxConcurrent` with nothing counted against it.
//!
//! `autonomous.roleRunner.maxConcurrent` and `LOOM_ROLE_RUNNER_MAX_CONCURRENT`
//! are retired: parsed and ignored with one warning ([`crate::retired_config`]).
//! `loom.dispatch.idle_slots` stays `maxConcurrent − occupancy` for one more
//! release, for dashboards that have not moved to `busy_slots`.

use std::path::Path;
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

/// Record the work finder's budget and occupancy for this tick.
pub fn publish(budget: usize, occupancy: usize) {
    *LAST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Published { budget, occupancy });
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
/// `workFinder.maxConcurrent` (env > config > default) with nothing counted
/// against it.
#[must_use]
pub fn role_ceiling(root: &Path) -> usize {
    role_ceiling_from(published()).unwrap_or_else(|| configured(root))
}

/// `root`'s configured budget, env > config > default.
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
        // 8 slots, 3 role agents running: steps may fill 5.
        assert_eq!(step_capacity(8, 3), 5);
        // 8 slots, 6 steps running: role agents may fill 2.
        let p = Some(Published {
            budget: 8,
            occupancy: 6,
        });
        assert_eq!(role_ceiling_from(p), Some(2));
        // Never negative: a budget lowered below what is running admits none.
        assert_eq!(step_capacity(2, 5), 0);
        let over = Some(Published {
            budget: 2,
            occupancy: 5,
        });
        assert_eq!(role_ceiling_from(over), Some(0));
        assert_eq!(role_ceiling_from(None), None, "no tick yet: the caller falls back");
    }
}
