//! Coverage for `dispatch_plan::annotate` (Issue #9288) over hand-built
//! tick reports. End-to-end coverage against a real tick (plan order equals
//! the `dispatch()` call order) lives in `repo_cap_tests.rs`, beside the
//! fakes it needs.

use std::path::PathBuf;

use super::super::repo_cap::RepoCapSnapshot;
use super::super::{ready_queue, PriorityCandidate, TickReport};
use super::*;
use crate::types::{PlanGate, PlanShard, PlanState, QueueDisposition as Qd, RepoCapView};

fn key(ws: usize, prio: u32, number: u32) -> PriorityCandidate {
    PriorityCandidate {
        workspace_idx: ws,
        workspace_priority: prio,
        number,
        ..PriorityCandidate::default()
    }
}

fn qrow(ws: usize, number: u32, d: Qd) -> ready_queue::TickQueueRow {
    ready_queue::TickQueueRow {
        // One priority per workspace, so rank order is (ws, number).
        key: key(ws, u32::try_from(ws).unwrap() * 10, number),
        tier: None,
        disposition: Some(d),
        detail: None,
        updated_at: None,
    }
}

fn inputs() -> PlanInputs {
    PlanInputs {
        max_concurrent: 4,
        tick_interval_secs: Some(60),
        shard: PlanShard::default(),
        owning_shard: Vec::new(),
    }
}

/// Run `annotate` the way `tick_summary` does, returning `(issue, row)` pairs.
fn plan(report: &TickReport, inputs: &PlanInputs) -> (Vec<ReadyQueueRow>, DispatchPlanContext) {
    let mut rows = ready_queue::finish(&report.queue, &[PathBuf::from("/a"), PathBuf::from("/b")]);
    let ctx = annotate(report, &mut rows, inputs);
    (rows, ctx)
}

fn by_issue(rows: &[ReadyQueueRow], issue: u32) -> &ReadyQueueRow {
    rows.iter().find(|r| r.issue == issue).unwrap()
}

/// `plan_state` per disposition: `running` for dispatched/in-flight; `next`
/// for at most `max_admissions_per_tick` capacity/ramp-deferred rows in plan
/// order, skipping a repo-capped one; `queued` for the other deferred rows;
/// `blocked` otherwise — and `position` follows `plan_order`, not `rank`.
#[test]
fn plan_state_and_position_follow_the_shaped_order() {
    let report = TickReport {
        queue: vec![
            qrow(0, 1, Qd::DeferredCapacity),
            qrow(0, 2, Qd::Parked),
            qrow(1, 3, Qd::Dispatched),
            qrow(1, 4, Qd::DeferredRepoCap),
            qrow(1, 5, Qd::DeferredRampCap),
            qrow(1, 6, Qd::InFlight),
            qrow(1, 7, Qd::DeferredCapacity),
            qrow(1, 8, Qd::OpenPr),
        ],
        // Affinity floated workspace 1 ahead of workspace 0.
        plan_order: vec![(1, 3), (1, 4), (1, 5), (1, 8), (1, 7), (0, 1)],
        max_admissions_per_tick: Some(2),
        occupancy: Some(3),
        ..TickReport::default()
    };
    let (rows, ctx) = plan(&report, &inputs());

    let expect = [
        (1, Some(6), PlanState::Queued, Some(PlanGate::Capacity)),
        (2, None, PlanState::Blocked, None),
        (3, Some(1), PlanState::Running, None),
        (4, Some(2), PlanState::Queued, Some(PlanGate::RepoCap)),
        (5, Some(3), PlanState::Next, Some(PlanGate::Ramp)),
        (6, None, PlanState::Running, None),
        (7, Some(5), PlanState::Next, Some(PlanGate::Capacity)),
        (8, None, PlanState::Blocked, None),
    ];
    for (issue, position, state, gate) in expect {
        let p = &by_issue(&rows, issue).plan;
        assert_eq!((p.position, p.plan_state, p.gate), (position, state, gate), "#{issue}");
    }
    // `rank` is untouched: still the bare comparator rank over every row.
    assert_eq!(by_issue(&rows, 1).rank, 1);
    assert_eq!(by_issue(&rows, 3).rank, 3);
    assert_eq!(ctx.slots.free, Some(1));
    assert_eq!(ctx.slots.max_admissions_per_tick, Some(2));
    assert_eq!(ctx.tick_interval_secs, Some(60));
    assert_eq!(ctx.scope, vec!["loom:issue", "loom:blocked"]);
    assert_eq!(ctx.ordering, ready_queue::ordering_names());
    assert!(ctx.complete);
    // Every row names the comparator keys that placed it.
    assert!(rows.iter().all(|r| r.plan.keys.len() == ctx.ordering.len()));
}

/// Saturation held ⇒ nothing is `next` (the brake holds every admission),
/// while the slots are still reported.
#[test]
fn saturation_held_leaves_next_empty() {
    let report = TickReport {
        queue: vec![
            qrow(0, 1, Qd::DeferredSaturation),
            qrow(0, 2, Qd::DeferredSaturation),
        ],
        plan_order: vec![(0, 1), (0, 2)],
        saturation_held: true,
        max_admissions_per_tick: Some(4),
        occupancy: Some(1),
        ..TickReport::default()
    };
    let (rows, ctx) = plan(&report, &inputs());
    assert!(rows.iter().all(|r| r.plan.plan_state == PlanState::Queued));
    assert!(rows
        .iter()
        .all(|r| r.plan.gate == Some(PlanGate::Saturation)));
    assert!(ctx.slots.saturation_held);
    assert_eq!(ctx.slots.free, Some(3));
}

/// Out-of-slice rows follow the plan, in comparator order; the slice and
/// cap inputs are reported per row; `owning_shard` comes from the inputs.
#[test]
fn out_of_slice_rows_follow_the_plan_with_shaping_inputs() {
    let report = TickReport {
        queue: vec![
            qrow(0, 1, Qd::DeferredOutOfSlice),
            qrow(0, 2, Qd::DeferredOutOfSlice),
            qrow(1, 3, Qd::Dispatched),
            qrow(1, 4, Qd::DeferredCapacity),
        ],
        plan_order: vec![(1, 3), (1, 4)],
        in_slice: Some(vec![false, true]),
        repo_cap: Some(RepoCapSnapshot {
            cap: Some(2),
            occupancy: vec![0, 1],
        }),
        max_admissions_per_tick: Some(1),
        ..TickReport::default()
    };
    let inputs = PlanInputs {
        shard: PlanShard {
            configured: true,
            host_shard: Some(1),
            shard_count: Some(2),
        },
        owning_shard: vec![Some(0), Some(1)],
        ..inputs()
    };
    let (rows, ctx) = plan(&report, &inputs);
    let (one, two) = (&by_issue(&rows, 1).plan, &by_issue(&rows, 2).plan);
    assert_eq!((one.position, one.plan_state), (Some(3), PlanState::Queued));
    assert_eq!(two.position, Some(4));
    assert_eq!(one.gate, Some(PlanGate::OutOfSlice));
    assert_eq!((one.in_slice, one.hot, one.owning_shard), (Some(false), Some(false), Some(0)));
    assert_eq!(
        one.repo_cap,
        Some(RepoCapView {
            cap: Some(2),
            occupancy: 0
        })
    );
    let four = &by_issue(&rows, 4).plan;
    assert_eq!((four.position, four.plan_state), (Some(2), PlanState::Next));
    assert_eq!((four.in_slice, four.hot, four.owning_shard), (Some(true), Some(true), Some(1)));
    assert_eq!(ctx.shard.host_shard, Some(1));
    // No occupancy reading ⇒ no free-slot figure, never a guessed one.
    assert_eq!(ctx.slots.free, None);
}

/// Unsharded: the slice owns every workspace, so every row is in-slice.
#[test]
fn unsharded_marks_every_row_in_slice() {
    let report = TickReport {
        queue: vec![qrow(0, 1, Qd::Dispatched), qrow(1, 2, Qd::DeferredCapacity)],
        plan_order: vec![(0, 1), (1, 2)],
        in_slice: Some(vec![true, true]),
        ..TickReport::default()
    };
    let (rows, ctx) = plan(&report, &inputs());
    assert!(rows.iter().all(|r| r.plan.in_slice == Some(true)));
    assert!(!ctx.shard.configured);
}

/// A failed listing marks the plan incomplete, like the queue.
#[test]
fn a_failed_listing_marks_the_plan_incomplete() {
    let report = TickReport {
        listing_failed: vec![1],
        ..TickReport::default()
    };
    let (rows, ctx) = plan(&report, &inputs());
    assert!(rows.is_empty());
    assert!(!ctx.complete);
}

/// Rows that do not line up with the tick's queue are left unannotated
/// rather than labelled with another row's plan.
#[test]
fn mismatched_rows_are_left_unannotated() {
    let report = TickReport {
        queue: vec![qrow(0, 1, Qd::Dispatched)],
        plan_order: vec![(0, 1)],
        ..TickReport::default()
    };
    let mut rows = Vec::new();
    let ctx = annotate(&report, &mut rows, &inputs());
    assert_eq!(ctx.slots.max_concurrent, 4);
}

#[test]
fn gates_map_only_deferrals() {
    assert_eq!(gate_of(Qd::DeferredOutOfSlice), Some(PlanGate::OutOfSlice));
    assert_eq!(gate_of(Qd::DeferredSaturation), Some(PlanGate::Saturation));
    for d in Qd::ALL {
        assert_eq!(gate_of(d).is_some(), d.state() == "ready", "{d:?}");
    }
}
