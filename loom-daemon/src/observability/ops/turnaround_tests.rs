//! Worker turnaround and idle-slot tests (Issue #8929, part 1).

use chrono::{Duration, TimeZone, Utc};

use super::{idle_points, is_issue_sweep, turnaround_points, IdleHold, SlotLedger, MAX_FREED};
use crate::telemetry::ops::{MetricName, MetricPoint};
use crate::types::{Event, SweepKind, SweepOutcome};
use crate::work_finder::TickReport;

fn at(secs: i64) -> chrono::DateTime<Utc> {
    Utc.timestamp_opt(1_790_000_000 + secs, 0).unwrap()
}

fn completed(sweep_id: &str) -> Event {
    Event::SweepGlobalCompleted {
        sweep_id: sweep_id.into(),
        outcome: SweepOutcome::Exited,
    }
}

fn dispatched(kind: SweepKind) -> Event {
    Event::SweepGlobalDispatch {
        sweep_id: "sweep-x".into(),
        kind,
        runtime: None,
        runtime_source: None,
        repo: None,
    }
}

#[test]
fn a_dispatch_after_a_completion_samples_the_gap() {
    let mut ledger = SlotLedger::default();
    assert_eq!(ledger.observe(&completed("sweep-issue-7-100"), at(0)), None);
    assert_eq!(ledger.observe(&completed("sweep-issue-8-100"), at(10)), None);
    // FIFO: the oldest freed slot is refilled first.
    assert_eq!(ledger.observe(&dispatched(SweepKind::Issue(9)), at(45)), Some(45));
    assert_eq!(ledger.observe(&dispatched(SweepKind::Issue(10)), at(50)), Some(40));
    // Nothing freed: no sample, never a guess.
    assert_eq!(ledger.observe(&dispatched(SweepKind::Issue(11)), at(60)), None);
}

#[test]
fn non_issue_sweeps_neither_free_nor_fill_slots() {
    let mut ledger = SlotLedger::default();
    ledger.observe(&completed("sweep-prs-1-2-100"), at(0));
    assert_eq!(ledger.pending(), 0);
    ledger.observe(&completed("sweep-issue-3-recovered-100"), at(0));
    assert_eq!(ledger.pending(), 1);
    assert_eq!(ledger.observe(&dispatched(SweepKind::PrSet(vec![1])), at(5)), None);
    assert_eq!(ledger.pending(), 1);
    assert!(is_issue_sweep("sweep-issue-1-1") && !is_issue_sweep("sweep-prs-1-1"));
}

#[test]
fn the_ledger_is_bounded() {
    let mut ledger = SlotLedger::default();
    for i in 0..(MAX_FREED as i64 + 5) {
        ledger.observe(&completed("sweep-issue-1-1"), at(i));
    }
    assert_eq!(ledger.pending(), MAX_FREED);
    // The oldest entries were dropped: the first refill is from t=5.
    assert_eq!(ledger.observe(&dispatched(SweepKind::Issue(2)), at(105)), Some(100));
}

#[test]
fn a_turnaround_sample_is_a_delta_pair_with_no_labels() {
    let points = turnaround_points(30);
    assert_eq!(
        points,
        vec![
            MetricPoint::int(MetricName::DispatchSlotTurnaround, 30),
            MetricPoint::int(MetricName::DispatchSlotTurnaroundSamples, 1),
        ]
    );
    assert!(points.iter().all(|p| p.labels.is_empty()));
}

#[test]
fn idle_slots_is_cap_minus_occupancy() {
    let report = TickReport {
        occupancy: Some(3),
        ..TickReport::default()
    };
    let (points, hold) = idle_points(&report, 5, None, at(0));
    assert_eq!(points, vec![MetricPoint::int(MetricName::DispatchIdleSlots, 2)]);
    // No work waited, so nothing is held for idle-slot-seconds.
    assert_eq!(
        hold,
        Some(IdleHold {
            at: at(0),
            idle_with_work: 0
        })
    );
    let over = TickReport {
        occupancy: Some(9),
        ..TickReport::default()
    };
    let (points, _) = idle_points(&over, 5, None, at(0));
    assert_eq!(points, vec![MetricPoint::int(MetricName::DispatchIdleSlots, 0)]);
}

#[test]
fn idle_slot_seconds_accrue_only_while_ready_work_waited() {
    // 4 idle slots, 1 issue held by the ramp cap: one usable slot sat idle.
    let report = TickReport {
        occupancy: Some(1),
        deferred_ramp_cap: 1,
        ..TickReport::default()
    };
    let (_, hold) = idle_points(&report, 5, None, at(0));
    assert_eq!(hold.unwrap().idle_with_work, 1);
    // The next tick credits the held state over the 60 s interval.
    let next = TickReport {
        occupancy: Some(2),
        ..TickReport::default()
    };
    let (points, hold) = idle_points(&next, 5, hold, at(60));
    assert!(points.contains(&MetricPoint::int(MetricName::DispatchIdleSlotSeconds, 60)));
    assert_eq!(hold.unwrap().idle_with_work, 0);
    // With nothing waiting, the tick after credits nothing.
    let (points, _) = idle_points(&next, 5, hold, at(120));
    assert!(!points
        .iter()
        .any(|p| p.name == MetricName::DispatchIdleSlotSeconds));
}

#[test]
fn a_long_gap_is_capped_and_a_missing_reading_emits_no_gauge() {
    let hold = Some(IdleHold {
        at: at(0),
        idle_with_work: 2,
    });
    let halted = TickReport::default();
    let (points, next) = idle_points(&halted, 5, hold, at(0) + Duration::hours(3));
    assert_eq!(
        points,
        vec![MetricPoint::int(
            MetricName::DispatchIdleSlotSeconds,
            2 * super::MAX_HOLD_SECS
        )]
    );
    assert_eq!(next, None, "no occupancy reading, no gauge and no new hold");
}
