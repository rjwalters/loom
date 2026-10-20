//! Busy agent slots by role, per host (Issue #11847).
//!
//! A daemon runs two agent pools, each under its own ceiling:
//!
//! - the **sweep pool**: the work finder's issue sweeps, bounded by
//!   `autonomous.workFinder.maxConcurrent` (`loom.dispatch.max_concurrent`);
//! - the **role pool**: role-runner agents (Curator, Champion, Judge, Doctor,
//!   …), bounded by `autonomous.roleRunner.maxConcurrent`.
//!
//! `loom.dispatch.busy_slots{role}` reports both, sampled on every work-finder
//! tick: `role="sweep"` is the tick's own occupancy reading, the same one
//! `loom.dispatch.idle_slots` is computed from, so for one tick
//! `busy_slots{role="sweep"} + idle_slots = max_concurrent` (more only while
//! occupancy is over the cap). Each role-runner role has its own series, `0`
//! included for every built-in role, with `loom.dispatch.role_max_concurrent`
//! the pool's ceiling and `loom.dispatch.role_idle_slots` what is left under
//! it, so `Σ busy_slots{role≠"sweep"} + role_idle_slots = role_max_concurrent`.
//! The host's whole agent capacity is the sum of the two ceilings.
//!
//! The role ceiling is the largest among the tick's repos with the role
//! runner enabled (admission resolves it per repo), re-read at most every
//! [`CEILING_TTL`]. `loom-daemon status` resolves it from the primary
//! workspace only, so on a host whose repos disagree the two can differ.
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
//!
//! The same published steps carry each step's start time (its registry
//! entry's `started_at`), so the tick also reports how long the step pool's
//! work has been running (#12873): `loom.dispatch.running_oldest_age_sec
//! {role, pool="step"}`, the oldest live step's age per role (no point for a
//! role with no live step, never `0`), and `loom.dispatch.running_by_age
//! {role, pool="step", age}`, live steps per fixed age bucket
//! ([`AGE_BUCKETS`]), `0` included for every step role and bucket. Ages are
//! computed at the tick (`now` minus each start), so they advance between
//! spawn and end events. A roll-resumed step's age restarts at the resume
//! (its registry entry is new).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use crate::telemetry::ops::{MetricName, MetricPoint};
use crate::work_finder::TickReport;

/// The `role` label value of the work finder's issue-sweep slots. No
/// role-runner role has this name.
pub const SWEEP_ROLE: &str = "sweep";

fn int(count: usize) -> i64 {
    i64::try_from(count).unwrap_or(i64::MAX)
}

/// The role-runner pool's ceiling for this host: the largest
/// `autonomous.roleRunner.maxConcurrent` among `roots` whose role runner is
/// enabled (admission resolves the ceiling per root, so the largest is the
/// most any root admits to), `0` when none is. Pure given the configs.
#[must_use]
pub fn role_ceiling(configs: &[crate::role_runner::RoleRunnerConfig]) -> usize {
    configs
        .iter()
        .filter(|config| crate::role_runner::resolve_enabled(config))
        .map(crate::role_runner::resolve_max_concurrent)
        .max()
        .unwrap_or(0)
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

/// Each step role name's live steps, as their start times (#12873).
pub type StepStarts = BTreeMap<&'static str, Vec<DateTime<Utc>>>;

/// The step points for live steps by role name: one per step role name,
/// `0` included, counted from each role's start times. Pure.
#[must_use]
pub fn step_points(running: &StepStarts) -> Vec<MetricPoint> {
    let mut by_role: BTreeMap<&str, usize> = step_role_names().map(|role| (role, 0)).collect();
    for (role, starts) in running {
        *by_role.entry(role).or_insert(0) += starts.len();
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

fn step_role_names() -> impl Iterator<Item = &'static str> {
    crate::step_role::StepRole::ALL
        .iter()
        .map(|role| role.role_name())
}

/// Upper edge of the `lt_15m` age bucket.
pub const AGE_EDGE_15M: Duration = Duration::from_secs(15 * 60);
/// Upper edge of the `15m_1h` age bucket.
pub const AGE_EDGE_1H: Duration = Duration::from_secs(60 * 60);
/// Upper edge of the `1h_4h` age bucket; `ge_4h` is everything from here.
pub const AGE_EDGE_4H: Duration = Duration::from_secs(4 * 60 * 60);

/// The `age` label values of `loom.dispatch.running_by_age`, youngest first.
pub const AGE_BUCKETS: [&str; 4] = ["lt_15m", "15m_1h", "1h_4h", "ge_4h"];

/// The [`AGE_BUCKETS`] index of a step `age_secs` old.
fn age_bucket(age_secs: u64) -> usize {
    [AGE_EDGE_15M, AGE_EDGE_1H, AGE_EDGE_4H]
        .iter()
        .position(|edge| age_secs < edge.as_secs())
        .unwrap_or(AGE_BUCKETS.len() - 1)
}

/// Seconds from `started` to `now`; `0` for a start in the future (a clock
/// step backwards).
fn age_secs(now: DateTime<Utc>, started: DateTime<Utc>) -> u64 {
    u64::try_from((now - started).num_seconds()).unwrap_or(0)
}

/// The running-age points of the live steps `running` at `now` (#12873):
/// per step role name, the oldest step's age (only for a role with a live
/// step) and the count in every [`AGE_BUCKETS`] bucket (`0` included). Pure.
#[must_use]
pub fn age_points(now: DateTime<Utc>, running: &StepStarts) -> Vec<MetricPoint> {
    let mut by_role: BTreeMap<&str, Vec<u64>> =
        step_role_names().map(|role| (role, Vec::new())).collect();
    for (role, starts) in running {
        by_role
            .entry(role)
            .or_default()
            .extend(starts.iter().map(|started| age_secs(now, *started)));
    }
    let mut points = Vec::new();
    for (role, ages) in by_role {
        if let Some(oldest) = ages.iter().max() {
            points.push(
                MetricPoint::int(
                    MetricName::DispatchRunningOldestAgeSec,
                    i64::try_from(*oldest).unwrap_or(i64::MAX),
                )
                .label("role", role)
                .label("pool", STEP_POOL),
            );
        }
        let mut buckets = [0_usize; AGE_BUCKETS.len()];
        for age in &ages {
            buckets[age_bucket(*age)] += 1;
        }
        for (age, count) in AGE_BUCKETS.iter().zip(buckets) {
            points.push(
                MetricPoint::int(MetricName::DispatchRunningByAge, int(count))
                    .label("role", role)
                    .label("pool", STEP_POOL)
                    .label("age", *age),
            );
        }
    }
    points
}

/// Each registry's live steps by role name, keyed by workspace root.
static STEPS: Mutex<BTreeMap<PathBuf, StepStarts>> = Mutex::new(BTreeMap::new());

/// Record `root`'s live steps by role name, each as its start time (the
/// sweep registry, on every spawn, end and reaper tick).
pub fn set_steps(root: &Path, running: StepStarts) {
    STEPS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(root.to_path_buf(), running);
}

/// Every published registry's live steps, merged by role; `None` when no
/// registry has published.
fn published_steps() -> Option<StepStarts> {
    let steps = STEPS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if steps.is_empty() {
        return None;
    }
    let mut total = StepStarts::new();
    for (role, starts) in steps.values().flatten() {
        total
            .entry(*role)
            .or_default()
            .extend(starts.iter().copied());
    }
    Some(total)
}

/// How long a resolved role ceiling is reused before the roots' configs are
/// read again. The ceiling is a config value, not load: re-reading every
/// root's `.loom/config.json` on every tick buys nothing, while this bound
/// still picks up an operator's `maxConcurrent` edit within minutes.
pub const CEILING_TTL: Duration = Duration::from_secs(300);

/// The last resolved ceiling, with the roots it was resolved for.
struct CeilingCache {
    roots: Vec<PathBuf>,
    at: Instant,
    ceiling: usize,
}

static CEILING: Mutex<Option<CeilingCache>> = Mutex::new(None);

/// [`role_ceiling`] of `roots`' configs, cached for [`CEILING_TTL`] and
/// re-resolved at once when the root set changes.
fn cached_role_ceiling(roots: &[PathBuf]) -> usize {
    let mut cache = CEILING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(hit) = cache
        .as_ref()
        .filter(|c| c.roots == roots && c.at.elapsed() < CEILING_TTL)
    {
        return hit.ceiling;
    }
    let configs: Vec<_> = roots
        .iter()
        .map(|root| crate::role_runner::read_role_runner_config(root))
        .collect();
    let ceiling = role_ceiling(&configs);
    *cache = Some(CeilingCache {
        roots: roots.to_vec(),
        at: Instant::now(),
        ceiling,
    });
    ceiling
}

/// Export the tick's busy slots. A no-op without an ops sink.
pub fn record_tick(report: &TickReport, roots: &[PathBuf]) {
    if super::global_ops_sink().is_none() {
        return;
    }
    let running = crate::role_runner::global_active_runs_by_role();
    let roles = running
        .as_ref()
        .map(|running| (running, cached_role_ceiling(roots)));
    let mut points = busy_points(report.occupancy, roles);
    if let Some(steps) = published_steps() {
        points.extend(step_points(&steps));
        points.extend(age_points(Utc::now(), &steps));
    }
    super::emit_metrics(points);
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "busy_slots_tests.rs"]
mod tests;
