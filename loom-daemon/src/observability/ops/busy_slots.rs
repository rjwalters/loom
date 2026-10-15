//! Busy agent slots by role, per host (Issue #11847).
//!
//! A daemon runs two kinds of agent under **one** host budget (#11911,
//! [`crate::host_budget`]), `loom.dispatch.max_concurrent`:
//!
//! - the **registry slots**: the work finder's steps (and any sweep an
//!   attended session dispatched);
//! - the **role-runner agents** (Champion non-item, Guide, Auditor, Hermit,
//!   Architect), whose ceiling is what the steps leave of the budget.
//!
//! `loom.dispatch.busy_slots{role}` reports both, sampled on every work-finder
//! tick: `role="sweep"` is the tick's own occupancy reading, the same one
//! `loom.dispatch.idle_slots` is computed from, so for one tick
//! `busy_slots{role="sweep"} + idle_slots = max_concurrent` (more only while
//! occupancy is over the cap; `idle_slots` is kept one more release for the
//! dashboards that have not moved to `busy_slots`). Each role-runner role has
//! its own series, `0` included for every built-in role, with
//! `loom.dispatch.role_max_concurrent` the role runner's share (the budget
//! less the occupancy) and `loom.dispatch.role_idle_slots` what is left under
//! it, so `Σ busy_slots{role≠"sweep"} + role_idle_slots = role_max_concurrent`,
//! and the host's whole capacity is `max_concurrent`.
//!
//! Absent, never a guessed zero: no occupancy reading, no `sweep` point; no
//! role-runner guard in the process, no role points. A PR-set sweep
//! (`/loom:sweep --prs`, dispatched only by hand) is in neither pool and is
//! not counted. A step (`SweepKind::Step`, #11898) draws from the sweep pool
//! (design #11894 §4.4), so it is in the tick's occupancy and counted under
//! `role="sweep"`, one slot per step, whatever its role. Those same slots are
//! also broken down by the step's role (#11906) as
//! `busy_slots{role=<role>, pool="step"}`, `0` included for every step role:
//! a breakdown of the `sweep` total, never added to it, and never part of
//! the role pool's `Σ busy_slots{role≠"sweep"}` (that sum reads only the
//! points without a `pool` label). The registries publish their live steps
//! ([`set_steps`]); with none published, no step point is emitted.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::telemetry::ops::{MetricName, MetricPoint};
use crate::work_finder::TickReport;

/// The `role` label value of the work finder's issue-sweep slots. No
/// role-runner role has this name.
pub const SWEEP_ROLE: &str = "sweep";

fn int(count: usize) -> i64 {
    i64::try_from(count).unwrap_or(i64::MAX)
}

/// The tick's busy-slot points. `occupancy` is the tick's sweep occupancy;
/// `roles` the role-runner agents in flight per role with the pool's
/// ceiling, `None` when this process runs no role runner. Pure.
#[must_use]
pub fn busy_points(
    occupancy: Option<usize>,
    roles: Option<(&BTreeMap<&'static str, usize>, usize)>,
) -> Vec<MetricPoint> {
    let mut points = Vec::new();
    if let Some(occupancy) = occupancy {
        points.push(
            MetricPoint::int(MetricName::DispatchBusySlots, int(occupancy))
                .label("role", SWEEP_ROLE),
        );
    }
    let Some((running, ceiling)) = roles else {
        return points;
    };
    let mut by_role: BTreeMap<&str, usize> = crate::role_runner::DEFAULT_ROLES
        .iter()
        .map(|spec| (spec.name, 0))
        .collect();
    for (role, count) in running {
        *by_role.entry(role).or_insert(0) += count;
    }
    let busy: usize = by_role.values().sum();
    for (role, count) in by_role {
        points
            .push(MetricPoint::int(MetricName::DispatchBusySlots, int(count)).label("role", role));
    }
    points.push(MetricPoint::int(MetricName::DispatchRoleMaxConcurrent, int(ceiling)));
    points.push(MetricPoint::int(
        MetricName::DispatchRoleIdleSlots,
        int(ceiling.saturating_sub(busy)),
    ));
    points
}

/// The `pool` label value of a step's per-role series (#11906).
pub const STEP_POOL: &str = "step";

/// The step points for live steps counted by role name: one per step role
/// name, `0` included. Pure.
#[must_use]
pub fn step_points(running: &BTreeMap<&'static str, usize>) -> Vec<MetricPoint> {
    let mut by_role: BTreeMap<&str, usize> = crate::step_role::StepRole::ALL
        .iter()
        .map(|role| (role.role_name(), 0))
        .collect();
    for (role, count) in running {
        *by_role.entry(role).or_insert(0) += count;
    }
    by_role
        .into_iter()
        .map(|(role, count)| {
            MetricPoint::int(MetricName::DispatchBusySlots, int(count))
                .label("role", role)
                .label("pool", STEP_POOL)
        })
        .collect()
}

/// Each registry's live steps by role name, keyed by workspace root.
static STEPS: Mutex<BTreeMap<PathBuf, BTreeMap<&'static str, usize>>> = Mutex::new(BTreeMap::new());

/// Record `root`'s live steps by role name (the sweep registry, on every
/// spawn, end and reaper tick).
pub fn set_steps(root: &Path, running: BTreeMap<&'static str, usize>) {
    STEPS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(root.to_path_buf(), running);
}

/// Every published registry's live steps, summed by role; `None` when no
/// registry has published.
fn published_steps() -> Option<BTreeMap<&'static str, usize>> {
    let steps = STEPS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if steps.is_empty() {
        return None;
    }
    let mut total = BTreeMap::new();
    for (role, count) in steps.values().flatten() {
        *total.entry(*role).or_insert(0) += count;
    }
    Some(total)
}

/// The role runner's share of the host budget this tick: the budget less the
/// tick's occupancy (#11911). Pure.
#[must_use]
pub fn role_share(max_concurrent: usize, occupancy: Option<usize>) -> usize {
    max_concurrent.saturating_sub(occupancy.unwrap_or(0))
}

/// Export the tick's busy slots. A no-op without an ops sink. Role points
/// are emitted only while this process runs a role runner.
pub fn record_tick(report: &TickReport, max_concurrent: usize) {
    if super::global_ops_sink().is_none() {
        return;
    }
    let running = crate::role_runner::global_active_runs_by_role();
    let roles = running
        .as_ref()
        .map(|running| (running, role_share(max_concurrent, report.occupancy)));
    let mut points = busy_points(report.occupancy, roles);
    if let Some(steps) = published_steps() {
        points.extend(step_points(&steps));
    }
    super::emit_metrics(points);
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "busy_slots_tests.rs"]
mod tests;
