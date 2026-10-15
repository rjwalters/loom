//! `pick.decision` `dispatch_result` (#11840): a completed work-finder tick
//! carries the canonical `loom.dispatch.result` its own [`TickReport`]
//! classifies to, beside the unchanged legacy `outcome`; role ticks and older
//! records omit it; every chunk repeats it.

use std::sync::{Arc, Mutex};

use chrono::{TimeZone, Utc};

use super::*;
use crate::observability::ops::dispatch::tick_result_with;
use crate::observability::ops::OpsSink;
use crate::observability::queue::QueueSink;
use crate::telemetry::TelemetryEnvelope;
use crate::work_finder::ready_queue::TickQueueRow;
use crate::work_finder::{PriorityCandidate, TickReport};

fn at() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 14, 12, 0, 0).unwrap()
}

fn row(number: u32, disposition: QueueDisposition) -> TickQueueRow {
    TickQueueRow {
        key: PriorityCandidate {
            number,
            workspace_weight: 100,
            ..PriorityCandidate::default()
        },
        disposition: Some(disposition),
        detail: None,
        updated_at: None,
        held_until: None,
        story_points: None,
        step: None,
    }
}

/// A producer report: the counters the tick loop sets, plus its rows.
fn report(rows: &[(u32, QueueDisposition)], set: impl FnOnce(&mut TickReport)) -> TickReport {
    let mut report = TickReport {
        queue: rows.iter().map(|&(n, d)| row(n, d)).collect(),
        seen: rows.len(),
        dispatched: rows
            .iter()
            .filter(|(_, d)| *d == QueueDisposition::Dispatched)
            .count(),
        ..TickReport::default()
    };
    set(&mut report);
    report
}

/// What `publish_tick` builds: the report's summary, and its result
/// classified once from the report itself.
fn record_for(report: &TickReport, billing_blocked: bool) -> PickDecisionRecord {
    let summary = crate::work_finder::tick_summary(report, 4, at(), &[], None);
    completed_work_finder_record(
        &summary,
        "host-a",
        (at(), at()),
        (None, &[]),
        &[],
        tick_result_with(report, billing_blocked),
    )
}

#[test]
fn dispatch_result_matches_the_canonical_tick_result_in_precedence_order() {
    use crate::types::QueueDisposition as Qd;
    use crate::work_finder::halt_cause::HaltCause::{Drain, MainRed};
    let halted_rows = [(1, Qd::WorkspaceHalted), (2, Qd::WorkspaceHalted)];
    // (case, report, billing_blocked, dispatch_result, legacy outcome)
    let cases: Vec<(&str, TickReport, bool, &str, &str)> = vec![
        (
            "dispatched wins over a hold, saturation and errors",
            report(&[(1, Qd::Dispatched), (2, Qd::DispatchError)], |r| {
                (r.halted, r.halt_causes, r.saturation_held, r.errors) =
                    (true, vec![MainRed], true, 1);
            }),
            false,
            "dispatched",
            "dispatched",
        ),
        (
            "an attributed whole hold wins over saturation",
            report(&halted_rows, |r| {
                (r.halted, r.halt_causes, r.saturation_held) = (true, vec![MainRed], true);
            }),
            false,
            "halted_main_red",
            "halted",
        ),
        (
            "a mixed-cause hold is neutral",
            report(&halted_rows, |r| {
                (r.halted, r.halt_causes) = (true, vec![MainRed, Drain]);
            }),
            false,
            "halted",
            "halted",
        ),
        (
            "a cause-less hold while billing-blocked",
            report(&halted_rows, |r| r.halted = true),
            true,
            "halted_ci_billing",
            "halted",
        ),
        (
            "a partial hold falls through to saturation",
            report(&[(1, Qd::WorkspaceHalted), (2, Qd::DeferredSaturation)], |r| {
                (r.halted, r.halt_partial, r.halt_causes, r.saturation_held) =
                    (true, true, vec![Drain], true);
            }),
            false,
            "saturation_held",
            "none_dispatched",
        ),
        (
            "saturation wins over errors",
            report(&[(1, Qd::DispatchError)], |r| {
                (r.saturation_held, r.errors) = (true, 1);
            }),
            false,
            "saturation_held",
            "none_dispatched",
        ),
        (
            "an error wins over build back-off",
            report(&[(1, Qd::DispatchError), (2, Qd::DeferredBuildBackoff)], |r| {
                (r.errors, r.deferred_build_backoff) = (1, 1);
            }),
            false,
            "error",
            "none_dispatched",
        ),
        (
            "build back-off wins over capacity",
            report(&[(1, Qd::DeferredBuildBackoff), (2, Qd::DeferredCapacity)], |r| {
                (r.deferred_build_backoff, r.deferred_capacity) = (1, 1);
            }),
            false,
            "build_backoff_held",
            "none_dispatched",
        ),
        ("an empty queue", report(&[], |_| {}), false, "no_eligible_work", "idle"),
        (
            "capacity deferral",
            report(&[(1, Qd::DeferredCapacity), (2, Qd::Parked)], |r| {
                (r.deferred_capacity, r.skipped_labeled) = (1, 1);
            }),
            false,
            "capacity_full",
            "none_dispatched",
        ),
        (
            "ramp-cap deferral",
            report(&[(1, Qd::DeferredRampCap)], |r| r.deferred_ramp_cap = 1),
            false,
            "capacity_full",
            "none_dispatched",
        ),
        (
            "every candidate skipped",
            report(&[(1, Qd::OpenPr), (2, Qd::Parked)], |r| {
                (r.skipped_pr_open, r.skipped_labeled) = (1, 1);
            }),
            false,
            "all_skipped",
            "none_dispatched",
        ),
    ];
    for (case, report, billing_blocked, result, outcome) in cases {
        let record = record_for(&report, billing_blocked);
        assert_eq!(
            record.dispatch_result.as_deref(),
            Some(tick_result_with(&report, billing_blocked)),
            "{case}: parity with the canonical classifier"
        );
        assert_eq!(record.dispatch_result.as_deref(), Some(result), "{case}");
        assert_eq!(record.outcome, outcome, "{case}: legacy outcome unchanged");
        assert_eq!(record.candidates.len(), report.queue.len(), "{case}");
    }
}

#[test]
fn every_chunk_repeats_the_dispatch_result_and_no_candidate_is_dropped() {
    let rows: Vec<(u32, QueueDisposition)> = (1..=300)
        .map(|n| (n, QueueDisposition::DeferredCapacity))
        .collect();
    let report = report(&rows, |r| r.deferred_capacity = 300);
    let record = record_for(&report, false);
    let chunks = split_into_chunks(record.clone(), 4_000);
    assert!(chunks.len() > 1, "the fixture must split");
    let mut numbers = Vec::new();
    for chunk in &chunks {
        let json = serde_json::to_value(chunk).unwrap();
        assert_eq!(json["dispatch_result"], "capacity_full");
        let back: PickDecisionRecord = serde_json::from_value(json).unwrap();
        assert_eq!(back.dispatch_result.as_deref(), Some("capacity_full"));
        assert_eq!(back.outcome, record.outcome);
        numbers.extend(back.candidates.iter().map(|c| c.number));
    }
    assert_eq!(numbers, (1..=300).collect::<Vec<u32>>());
}

#[test]
fn role_ticks_and_older_records_carry_no_dispatch_result() {
    let tick = PickTick {
        role: "judge".to_string(),
        host: "host-a".to_string(),
        tick_id: "t".to_string(),
        started_at: at(),
        ended_at: at(),
        outcome: "success".to_string(),
    };
    let role = role_record(tick, RoleTickResult::Success, RoleObservation::default(), "o/r", &[]);
    assert_eq!(role.dispatch_result, None);
    let json = serde_json::to_value(&role).unwrap();
    assert!(json.get("dispatch_result").is_none(), "absent, not null");

    // An older daemon's work-finder record: no field, read back as `None`.
    let mut old = serde_json::to_value(record_for(&report(&[], |_| {}), false)).unwrap();
    old.as_object_mut().unwrap().remove("dispatch_result");
    let back: PickDecisionRecord = serde_json::from_value(old).unwrap();
    assert_eq!(back.dispatch_result, None);
    assert_eq!(back.outcome, "idle");
}

#[derive(Default)]
struct Recording(Mutex<Vec<TelemetryEnvelope>>);

impl QueueSink for Recording {
    fn offer(&self, envelope: TelemetryEnvelope) {
        self.0.lock().unwrap().push(envelope);
    }
    fn offer_durable(&self, envelope: TelemetryEnvelope) -> std::io::Result<()> {
        self.offer(envelope);
        Ok(())
    }
}

/// With an exporter configured, every exported chunk carries the result;
/// with none, nothing is built (`emit_work_finder` returns first).
#[test]
fn the_result_is_exported_only_through_a_configured_sink() {
    let report = report(&[(1, QueueDisposition::Dispatched)], |_| {});
    let queue = Arc::new(Recording::default());
    let sink = OpsSink::new(queue.clone(), "host-a");
    for chunk in split_into_chunks(record_for(&report, false), CHUNK_BYTES) {
        sink.emit_record(TelemetryRecord::PickDecision(chunk));
    }
    let got = queue.0.lock().unwrap();
    assert_eq!(got.len(), 1);
    let TelemetryRecord::PickDecision(exported) = &got[0].record else {
        panic!("not a pick.decision");
    };
    assert_eq!(exported.dispatch_result.as_deref(), Some("dispatched"));

    let summary = crate::work_finder::tick_summary(&report, 4, at(), &[], None);
    crate::observability::ops::capture::without_global_sink(|| {
        assert!(!exporting());
        emit_work_finder(&summary, at(), at(), (None, &[], None), &[], "dispatched");
    });
}

/// #12083: a tick the rate-limit breaker skipped whole is a record that names
/// why, with no candidates and nothing observed.
#[test]
fn a_rate_limit_skipped_tick_is_a_named_empty_record() {
    let record = skipped_work_finder_record("host-a", at(), RATE_LIMIT_HELD);
    assert_eq!(record.role, crate::telemetry::kinds::pick_decision::WORK_FINDER_ROLE);
    assert_eq!(record.outcome, "skipped");
    assert_eq!(record.dispatch_result.as_deref(), Some("rate_limit_held"));
    assert!(record.candidates.is_empty() && record.skipped.is_empty());
    assert!(!record.decisions_observed);
}
