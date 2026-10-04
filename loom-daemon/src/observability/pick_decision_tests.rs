//! `pick.decision` emitter tests (#10212): work-finder and role-tick records,
//! the thread-local gate stash, and routing through the ops sink.

use std::path::Path;
use std::sync::{Arc, Mutex};

use chrono::{TimeZone, Utc};

use super::*;
use crate::observability::ops::OpsSink;
use crate::observability::queue::QueueSink;
use crate::telemetry::{TelemetryEnvelope, TelemetryRecord};
use crate::types::{QueueDisposition, ReadyQueueRow};

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

fn at() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 4, 12, 0, 0).unwrap()
}

fn summary(rows: Vec<(u32, QueueDisposition)>) -> WorkFinderTickSummary {
    let mut s: WorkFinderTickSummary = serde_json::from_value(serde_json::json!({
        "at": "2026-10-04T12:00:00Z", "max_concurrent": 2, "seen": 0,
        "dispatched": 0, "skipped_labeled": 0, "skipped_in_flight": 0,
        "skipped_quarantined": 0, "skipped_pr_open": 0,
        "skipped_peer_claim": 0, "skipped_backoff": 0,
        "deferred_capacity": 0, "deferred_ramp_cap": 0, "errors": 0,
        "halted": false
    }))
    .unwrap();
    s.dispatched = rows
        .iter()
        .filter(|(_, d)| *d == QueueDisposition::Dispatched)
        .count();
    s.queue = rows
        .into_iter()
        .enumerate()
        .map(|(i, (issue, disposition))| {
            serde_json::from_value::<ReadyQueueRow>(serde_json::json!({
                "rank": i + 1, "repo": "/work/loom", "issue": issue,
                "workspace_priority": 100, "urgent": false,
                "disposition": disposition,
            }))
            .unwrap()
        })
        .collect();
    s
}

fn resolve(_: &str) -> String {
    "o/r".to_string()
}

#[test]
fn work_finder_record_keeps_rank_order_and_classifies_each_row() {
    let s = summary(vec![
        (30, QueueDisposition::Dispatched),
        (10, QueueDisposition::OpenPr),
        (20, QueueDisposition::DeferredCapacity),
        (40, QueueDisposition::Parked),
    ]);
    let r = work_finder_record(&s, "host-a", at(), at(), resolve);
    assert_eq!(r.role, "work_finder");
    assert_eq!(r.outcome, "dispatched");
    let order: Vec<u32> = r.candidates.iter().map(|c| c.number).collect();
    assert_eq!(order, vec![30, 10, 20, 40], "ranker order, not numeric");
    assert_eq!(r.acted.len(), 1);
    assert_eq!(r.acted[0].number, 30);
    let reasons: Vec<(u32, PickSkipReason)> =
        r.skipped.iter().map(|s| (s.number, s.reason)).collect();
    assert_eq!(
        reasons,
        vec![
            (10, PickSkipReason::PrOpenSkip),
            (20, PickSkipReason::Cap),
            (40, PickSkipReason::OperatorHold),
        ]
    );
    assert!(r
        .candidates
        .iter()
        .all(|c| c.repo == "o/r" && !c.repo.starts_with('/')));
}

#[test]
fn an_empty_work_finder_tick_still_yields_a_record() {
    let r = work_finder_record(&summary(vec![]), "host-a", at(), at(), resolve);
    assert_eq!(r.outcome, "idle");
    assert_eq!(r.candidates_total, 0);
    assert!(r.tick_id.starts_with("work_finder-"));
}

fn tick(role: &str) -> PickTick {
    PickTick {
        role: role.to_string(),
        host: "host-a".to_string(),
        tick_id: "role-judge-x".to_string(),
        started_at: at(),
        ended_at: at(),
        outcome: "success".to_string(),
    }
}

fn row(number: u32, labels: &[&str]) -> GateRow {
    GateRow {
        number,
        labels: labels.iter().map(|l| (*l).to_string()).collect(),
    }
}

#[test]
fn role_record_ranks_in_listing_order_and_marks_held_rows() {
    let listings = vec![(
        "loom:pr".to_string(),
        vec![row(5, &[]), row(3, &["loom:operator"]), row(9, &[])],
    )];
    let r = role_record(
        tick("champion"),
        RoleTickResult::Success,
        listings,
        "o/r",
        &crate::role_runner::demand::MERGE_HOLD_LABELS,
    );
    let order: Vec<u32> = r.candidates.iter().map(|c| c.number).collect();
    assert_eq!(order, vec![5, 3, 9]);
    assert_eq!(r.candidates[1].rank, 2);
    assert_eq!(r.skipped.len(), 1);
    assert_eq!((r.skipped[0].number, r.skipped[0].reason), (3, PickSkipReason::OperatorHold));
    assert!(r.acted.is_empty(), "the daemon cannot see the agent's pick");
}

#[test]
fn a_tick_that_never_ran_skips_every_candidate_with_a_reason() {
    let listings = vec![("loom:review-requested".to_string(), vec![row(1, &[]), row(2, &[])])];
    let r = role_record(tick("judge"), RoleTickResult::SkippedPoolExhausted, listings, "o/r", &[]);
    assert_eq!(r.skipped.len(), 2);
    assert!(r.skipped.iter().all(|s| s.reason == PickSkipReason::Quota));
}

#[test]
fn an_empty_role_tick_records_cadence_with_no_candidates() {
    let r = role_record(tick("curator"), RoleTickResult::Success, vec![], "o/r", &[]);
    assert_eq!(r.candidates_total, 0);
    assert_eq!(r.role, "curator");
}

#[test]
fn gate_stash_keeps_only_open_prs_and_drains_per_root() {
    clear_gate_listings();
    let pr = |n: u32, state: &str, is_pr: bool| RestIssue {
        number: n,
        title: None,
        labels: vec![],
        created_at: None,
        updated_at: None,
        closed_at: None,
        state: state.to_string(),
        body: None,
        author: None,
        is_pull_request: is_pr,
    };
    let root = Path::new("/stash-test/a");
    record_gate_listing(
        root,
        "loom:review-requested",
        &[
            pr(1, "open", true),
            pr(2, "open", false),
            pr(3, "closed", true),
        ],
    );
    record_gate_listing(Path::new("/stash-test/b"), "loom:pr", &[pr(4, "open", true)]);
    let taken = take_gate_listings(root);
    assert_eq!(taken.len(), 1);
    assert_eq!(taken[0].1.iter().map(|r| r.number).collect::<Vec<_>>(), vec![1]);
    // Drained: nothing carries into the next tick.
    assert!(take_gate_listings(root).is_empty());
}

#[test]
fn emit_record_routes_through_the_ops_sink_as_a_pick_decision() {
    let queue = Arc::new(Recording::default());
    let sink = OpsSink::new(queue.clone(), "host-a");
    let record = work_finder_record(&summary(vec![]), "host-a", at(), at(), resolve);
    sink.emit_record(TelemetryRecord::PickDecision(record));
    let got = queue.0.lock().unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].record.kind(), "pick.decision");
}
