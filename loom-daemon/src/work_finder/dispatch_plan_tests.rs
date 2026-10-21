//! Coverage for `dispatch_plan::annotate` (Issue #9288) over hand-built
//! tick reports. End-to-end coverage against a real tick (plan order equals
//! the `dispatch()` call order) lives in the work finder's tick tests.

use std::path::PathBuf;

use super::super::{ready_queue, PriorityCandidate, TickReport};
use super::*;
use crate::types::{PlanGate, PlanShard, PlanState, QueueDisposition as Qd};

fn key(ws: usize, prio: u32, number: u32) -> PriorityCandidate {
    PriorityCandidate {
        workspace_idx: ws,
        workspace_weight: prio,
        number,
        ..PriorityCandidate::default()
    }
}

fn qrow(ws: usize, number: u32, d: Qd) -> ready_queue::TickQueueRow {
    qrow_held(ws, number, d, None)
}

/// [`qrow`] plus a time-boxed hold's absolute expiry (Issue #9311).
fn qrow_held(
    ws: usize,
    number: u32,
    d: Qd,
    held_until: Option<chrono::DateTime<chrono::Utc>>,
) -> ready_queue::TickQueueRow {
    ready_queue::TickQueueRow {
        // One priority per workspace, so rank order is (ws, number).
        key: key(ws, u32::try_from(ws).unwrap() * 10, number),
        disposition: Some(d),
        detail: None,
        updated_at: None,
        held_until,
        story_points: None,
        step: None,
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

/// `in_slice` is retired (#11682): no row carries it, sharded or not.
#[test]
fn no_row_carries_the_retired_in_slice_field() {
    let report = TickReport {
        queue: vec![qrow(0, 1, Qd::Dispatched), qrow(1, 2, Qd::DeferredCapacity)],
        plan_order: vec![(0, 1), (1, 2)],
        ..TickReport::default()
    };
    let (rows, ctx) = plan(&report, &inputs());
    for row in &rows {
        let wire = serde_json::to_value(row).unwrap();
        assert!(wire.get("in_slice").is_none(), "{wire}");
    }
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

/// Same-length rows that do not pair up by issue number with the tick's
/// queue are left unannotated rather than labelled with another row's plan.
#[test]
fn mismatched_rows_are_left_unannotated() {
    let report = TickReport {
        queue: vec![qrow(0, 1, Qd::Dispatched), qrow(0, 2, Qd::DeferredCapacity)],
        plan_order: vec![(0, 1)],
        ..TickReport::default()
    };
    // Same length as `report.queue`, but a different pair of issues: no
    // position lines up with `ready_queue::ranked(&report.queue)`.
    let mut rows = ready_queue::finish(
        &[qrow(0, 9, Qd::Dispatched), qrow(0, 8, Qd::DeferredCapacity)],
        &[PathBuf::from("/a")],
    );
    let before = rows.clone();
    let ctx = annotate(&report, &mut rows, &inputs());
    assert_eq!(ctx.slots.max_concurrent, 4);
    assert_eq!(rows, before);
    assert!(rows
        .iter()
        .all(|r| r.plan == crate::types::RowPlan::default()));
}

/// Each of the five time-boxed hold dispositions (Issue #9311) carries its
/// recorded expiry straight through to `plan.held_until`.
#[test]
fn held_until_carries_each_time_boxed_holds_expiry() {
    let until = |secs: i64| chrono::Utc::now() + chrono::Duration::seconds(secs);
    let expiries = [
        (1, Qd::RecheckInterval, until(60)),
        (2, Qd::DispatchBackoff, until(120)),
        (3, Qd::OpenPrBackoff, until(180)),
        (4, Qd::NoopCooldown, until(240)),
        (5, Qd::Declined, until(300)),
        (6, Qd::PrlessRetry, until(360)),
    ];
    let report = TickReport {
        queue: expiries
            .iter()
            .map(|(n, d, u)| qrow_held(0, *n, *d, Some(*u)))
            .collect(),
        plan_order: Vec::new(),
        ..TickReport::default()
    };
    let (rows, _ctx) = plan(&report, &inputs());
    for (issue, _, expected) in expiries {
        assert_eq!(by_issue(&rows, issue).plan.held_until, Some(expected), "#{issue}");
    }
}

/// `plan_state` per disposition (#12406: no ramp, saturation or repo-cap
/// gates): `running` for dispatched/in-flight; `next` for the first
/// `max_concurrent` capacity-deferred rows in plan order; `queued` for the
/// other deferred rows; `blocked` otherwise — and `position` follows
/// `plan_order`, not `rank`.
#[test]
fn plan_state_and_position_follow_the_draw_order() {
    let report = TickReport {
        queue: vec![
            qrow(0, 1, Qd::DeferredCapacity),
            qrow(0, 2, Qd::Parked),
            qrow(1, 3, Qd::Dispatched),
            qrow(1, 4, Qd::DeferredBuildBackoff),
            qrow(1, 6, Qd::InFlight),
            qrow(1, 7, Qd::DeferredCapacity),
            qrow(1, 8, Qd::OpenPr),
        ],
        plan_order: vec![(1, 3), (1, 4), (1, 8), (1, 7), (0, 1)],
        occupancy: Some(3),
        ..TickReport::default()
    };
    let inputs = PlanInputs {
        max_concurrent: 1,
        ..inputs()
    };
    let (rows, ctx) = plan(&report, &inputs);
    let expect = [
        (1, Some(5), PlanState::Queued, Some(PlanGate::Capacity)),
        (2, None, PlanState::Blocked, None),
        (3, Some(1), PlanState::Running, None),
        (4, Some(2), PlanState::Queued, Some(PlanGate::BuildBackoff)),
        (6, None, PlanState::Running, None),
        (7, Some(4), PlanState::Next, Some(PlanGate::Capacity)),
        (8, None, PlanState::Blocked, None),
    ];
    for (issue, position, state, gate) in expect {
        let p = &by_issue(&rows, issue).plan;
        assert_eq!((p.position, p.plan_state, p.gate), (position, state, gate), "#{issue}");
    }
    assert_eq!(ctx.slots.max_concurrent, 1);
    assert_eq!(ctx.slots.free, Some(0));
}
