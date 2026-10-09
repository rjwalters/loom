//! `dispatch_plan` (Issue #9288): the order this host will work in, as a pure
//! projection of the tick that just ran.
//!
//! There is no second ranking function here. The tick records the shaped
//! pass-2 order it iterated ([`TickReport::plan_order`], written by
//! `repo_cap::shape_queue`) plus the shaping inputs and admission knobs it ran
//! under; [`annotate`] reads those and fills each ready-queue row's plan
//! fields and the per-tick [`DispatchPlanContext`]. The comparator keys come
//! from [`ready_queue::candidate_keys`], the same seam `candidate_cmp` is
//! built from.
//!
//! # `plan_state`
//!
//! - `running` — `dispatched` / `in_flight`.
//! - `next` — the first `max_admissions_per_tick` rows, in plan order, among
//!   rows deferred by the concurrency or ramp cap: what the next tick admits
//!   once slots free. Saturation-held, repo-capped and out-of-slice rows are
//!   never `next` (the saturation brake holds every admission; the other two
//!   need something besides a free slot).
//! - `queued` — every other deferred row.
//! - `blocked` — everything else.
//!
//! # `position`
//!
//! The 1-based index in [`TickReport::plan_order`]. Out-of-slice rows, which
//! the slice partition removed from pass 2's list, follow it in comparator
//! order (the order they would be offered in once the slice runs dry).
//! `blocked` rows, and rows dropped before the sort (`in_flight` included),
//! have none. `rank` is unchanged: the bare comparator rank over every row.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use super::{ready_queue, TickReport};
use crate::role_shard::ShardDecision;
use crate::types::{
    DispatchPlanContext, PlanGate, PlanShard, PlanSlots, PlanState, QueueDisposition as Qd,
    ReadyQueueRow, RepoCapView, PLAN_SCOPE,
};

/// What the tick loop knows that the [`TickReport`] does not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlanInputs {
    /// The dynamic concurrency cap the tick ran under.
    pub max_concurrent: usize,
    /// The work finder's tick interval.
    pub tick_interval_secs: Option<u64>,
    /// This host's shard posture.
    pub shard: PlanShard,
    /// Per workspace index: the shard that owns it, when sharded.
    pub owning_shard: Vec<Option<usize>>,
}

impl PlanInputs {
    /// Inputs for a multi-workspace tick, from the loop's own per-root
    /// [`ShardDecision`]s (the same ones its preferred slice came from).
    #[must_use]
    pub fn new(max_concurrent: usize, interval: Duration, shards: &[ShardDecision]) -> Self {
        let posture = shards.iter().map(|d| &d.posture).find(|p| p.is_sharded());
        PlanInputs {
            max_concurrent,
            tick_interval_secs: Some(interval.as_secs()),
            shard: PlanShard {
                configured: posture.is_some(),
                host_shard: posture.and_then(|p| p.index()).and_then(to_u32),
                shard_count: posture.and_then(|p| p.count()).and_then(to_u32),
            },
            owning_shard: shards.iter().map(|d| d.owning_shard).collect(),
        }
    }
}

/// Every root's [`ShardDecision`] this tick, in `roots` order.
#[must_use]
pub fn shard_decisions(roots: &[PathBuf]) -> Vec<ShardDecision> {
    roots.iter().map(|r| crate::role_shard::decide(r)).collect()
}

fn to_u32(n: usize) -> Option<u32> {
    u32::try_from(n).ok()
}

/// The admission gate behind a deferral, or `None` for a non-deferred row.
#[must_use]
pub fn gate_of(disposition: Qd) -> Option<PlanGate> {
    match disposition {
        Qd::DeferredCapacity => Some(PlanGate::Capacity),
        Qd::DeferredRampCap => Some(PlanGate::Ramp),
        Qd::DeferredSaturation => Some(PlanGate::Saturation),
        Qd::DeferredBuildBackoff => Some(PlanGate::BuildBackoff),
        Qd::DeferredRepoCap => Some(PlanGate::RepoCap),
        Qd::DeferredOutOfSlice => Some(PlanGate::OutOfSlice),
        _ => None,
    }
}

/// Fill `rows` (the output of [`ready_queue::finish`] over `report.queue`)
/// with their plan fields and return the tick's plan block. Pure.
///
/// A `rows` slice that does not line up with `report.queue` (which only a
/// caller bug can produce) is left unannotated rather than mislabelled. Lining
/// up means both the same length *and* the same issue at every position —
/// `ready_queue::finish` and `ready_queue::ranked` sort by the same
/// comparator over the same input, so in practice this always holds; the
/// per-pair check is what makes that a verified guarantee rather than an
/// assumption.
pub fn annotate(
    report: &TickReport,
    rows: &mut [ReadyQueueRow],
    inputs: &PlanInputs,
) -> DispatchPlanContext {
    let ranked = ready_queue::ranked(&report.queue);
    let aligned = ranked.len() == rows.len()
        && ranked
            .iter()
            .zip(rows.iter())
            .all(|(tick_row, row)| tick_row.key.number == row.issue);
    if aligned {
        annotate_rows(report, &ranked, rows, inputs);
    } else {
        log::warn!(
            "dispatch_plan: {} row(s) do not line up with the tick's {} queue row(s); left unannotated",
            rows.len(),
            ranked.len()
        );
    }
    let occupancy = report.occupancy;
    DispatchPlanContext {
        slots: PlanSlots {
            max_concurrent: inputs.max_concurrent,
            occupancy,
            free: occupancy.map(|o| inputs.max_concurrent.saturating_sub(o)),
            max_admissions_per_tick: report.max_admissions_per_tick,
            saturation_held: report.saturation_held,
            any_halted: report.halted,
            overflow_free: report.overflow_free,
        },
        tick_interval_secs: inputs.tick_interval_secs,
        shard: inputs.shard,
        scope: PLAN_SCOPE.iter().map(|s| (*s).to_string()).collect(),
        ordering: ready_queue::ordering_names(),
        complete: report.listing_not_whole().is_empty(),
    }
}

fn annotate_rows(
    report: &TickReport,
    ranked: &[&ready_queue::TickQueueRow],
    rows: &mut [ReadyQueueRow],
    inputs: &PlanInputs,
) {
    let in_plan: HashMap<(usize, u32), u32> = report
        .plan_order
        .iter()
        .zip(1u32..)
        .map(|(key, pos)| (*key, pos))
        .collect();
    let mut next_position = u32::try_from(in_plan.len())
        .unwrap_or(u32::MAX)
        .saturating_add(1);
    for (tick_row, row) in ranked.iter().zip(rows.iter_mut()) {
        let idx = tick_row.key.workspace_idx;
        let gate = gate_of(row.disposition);
        let plan_state = match row.disposition {
            Qd::Dispatched | Qd::InFlight => PlanState::Running,
            _ if gate.is_some() => PlanState::Queued,
            _ => PlanState::Blocked,
        };
        let position = match plan_state {
            PlanState::Blocked => None,
            _ if gate == Some(PlanGate::OutOfSlice) => {
                let pos = next_position;
                next_position = next_position.saturating_add(1);
                Some(pos)
            }
            _ => in_plan.get(&(idx, tick_row.key.number)).copied(),
        };
        let cap = report.repo_cap.as_ref();
        let occupancy = cap.map(|c| c.occupancy.get(idx).copied().unwrap_or(0));
        let plan = &mut row.plan;
        plan.position = position;
        plan.plan_state = plan_state;
        plan.keys = ready_queue::plan_keys(&tick_row.key);
        plan.gate = gate;
        // Same fail-open default `apply_slice` uses for a missing entry.
        plan.in_slice = report
            .in_slice
            .as_ref()
            .map(|s| s.get(idx).copied().unwrap_or(true));
        plan.hot = occupancy.map(|o| o > 0);
        plan.owning_shard = inputs
            .owning_shard
            .get(idx)
            .copied()
            .flatten()
            .and_then(to_u32);
        plan.repo_cap = cap.zip(occupancy).map(|(c, occupancy)| RepoCapView {
            cap: c.cap,
            occupancy,
        });
        // Issue #9311: a time-boxed hold's absolute expiry, straight from the
        // tick row — never touched for any other disposition, so it stays
        // `None` for capacity/ramp gates, in-flight, quarantined, peer-claimed
        // and every disposition that isn't one of the five recorded holds.
        plan.held_until = tick_row.held_until;
    }
    promote_next(rows, report.max_admissions_per_tick.unwrap_or(usize::MAX));
}

/// Mark the first `ramp` capacity- or ramp-deferred rows, in plan order, as
/// `next`.
fn promote_next(rows: &mut [ReadyQueueRow], ramp: usize) {
    let mut eligible: Vec<(u32, usize)> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| matches!(r.plan.gate, Some(PlanGate::Capacity | PlanGate::Ramp)))
        .filter_map(|(i, r)| r.plan.position.map(|p| (p, i)))
        .collect();
    eligible.sort_unstable();
    for (_, i) in eligible.into_iter().take(ramp) {
        rows[i].plan.plan_state = PlanState::Next;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "dispatch_plan_tests.rs"]
mod tests;
