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
    assert_eq!((r.candidate_source.as_str(), r.decisions_observed), ("ready_queue", true));
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
        gate_only(listings),
        "o/r",
        &crate::role_runner::demand::MERGE_HOLD_LABELS,
    );
    let order: Vec<u32> = r.candidates.iter().map(|c| c.number).collect();
    assert_eq!(order, vec![5, 3, 9]);
    assert_eq!(r.candidates[1].rank, 2);
    assert_eq!(r.skipped.len(), 1);
    assert_eq!((r.skipped[0].number, r.skipped[0].reason), (3, PickSkipReason::OperatorHold));
    assert!(r.acted.is_empty(), "no journal: the agent's pick was not observed");
    assert_eq!(r.candidate_source, "gate_listing");
    assert!(!r.decisions_observed);
}

#[test]
fn a_tick_that_never_ran_skips_every_candidate_with_a_reason() {
    let listings = vec![("loom:review-requested".to_string(), vec![row(1, &[]), row(2, &[])])];
    let r = role_record(
        tick("judge"),
        RoleTickResult::SkippedPoolExhausted,
        gate_only(listings),
        "o/r",
        &[],
    );
    assert_eq!(r.skipped.len(), 2);
    assert!(r.skipped.iter().all(|s| s.reason == PickSkipReason::Quota));
}

#[test]
fn an_empty_role_tick_records_cadence_with_no_candidates() {
    let r = role_record(tick("curator"), RoleTickResult::Success, gate_only(vec![]), "o/r", &[]);
    assert_eq!(r.candidates_total, 0);
    assert_eq!(r.role, "curator");
    assert_eq!(r.candidate_source, "none", "nothing observed");
    assert!(!r.decisions_observed);
}

fn gate_only(gate: Vec<(String, Vec<GateRow>)>) -> RoleObservation {
    RoleObservation {
        journal: None,
        gate,
    }
}

fn qrow(number: u32, stage: &str, labels: &[&str], key: &str) -> JournalRow {
    JournalRow {
        number,
        stage: stage.to_string(),
        labels: labels.iter().map(|l| (*l).to_string()).collect(),
        sort_key: Some(PickSortKey {
            name: "pr_queue".to_string(),
            value: key.to_string(),
        }),
    }
}

fn journal(entries: Vec<JournalEntry>) -> RoleObservation {
    RoleObservation {
        journal: Some(Journal {
            entries,
            front_expected: true,
            read: true,
        }),
        // The gate saw listing order 1, 2, 3; the serving queue differs.
        gate: vec![(
            "loom:review-requested".to_string(),
            vec![row(1, &[]), row(2, &[]), row(3, &[])],
        )],
    }
}

fn queue(rows: Vec<JournalRow>) -> JournalEntry {
    JournalEntry::Queue {
        at: at(),
        role: "judge".to_string(),
        acts_observable: true,
        total: 0,
        rows,
    }
}

fn act(number: u32, action: &str) -> JournalEntry {
    JournalEntry::Act {
        at: at(),
        number,
        action: action.to_string(),
    }
}

#[test]
fn the_serving_queue_order_wins_over_the_gate_listing_order() {
    // pr-queue put the starred #3 first, then #1; #2 is not in the queue.
    let obs = journal(vec![queue(vec![
        qrow(3, "loom:review-requested", &["loom:operator-priority"], "level=1"),
        qrow(1, "loom:review-requested", &[], "level=0"),
    ])]);
    let r = role_record(tick("judge"), RoleTickResult::Success, obs, "o/r", &[]);
    let order: Vec<u32> = r.candidates.iter().map(|c| c.number).collect();
    assert_eq!(order, vec![3, 1], "serving order, not listing order");
    assert_eq!(r.candidate_source, "serving_queue");
    assert_eq!(r.candidates[0].sort_key.as_ref().unwrap().value, "level=1");
}

#[test]
fn a_fallback_only_queue_is_recorded_though_the_gate_saw_nothing() {
    let obs = RoleObservation {
        journal: Some(Journal {
            entries: vec![queue(vec![qrow(42, "fallback", &[], "mode=fallback")])],
            front_expected: true,
            read: true,
        }),
        gate: vec![("loom:review-requested".to_string(), vec![])],
    };
    let r = role_record(tick("judge"), RoleTickResult::Success, obs, "o/r", &[]);
    assert_eq!(r.candidates.len(), 1);
    assert_eq!((r.candidates[0].number, r.candidates[0].stage.as_str()), (42, "fallback"));
}

#[test]
fn a_successful_multi_candidate_tick_records_the_pick_and_why_the_rest_were_skipped() {
    let obs = journal(vec![
        queue(vec![
            qrow(5, "loom:review-requested", &["loom:reviewing"], "a"),
            qrow(6, "loom:review-requested", &[], "b"),
            qrow(7, "loom:review-requested", &["loom:operator"], "c"),
            qrow(8, "loom:review-requested", &[], "d"),
        ]),
        act(6, "claimed"),
        act(6, "approved"),
        // A refresh after the review: #6 is gone, #8 still queued.
        queue(vec![qrow(8, "loom:review-requested", &[], "d")]),
    ]);
    let r = role_record(tick("judge"), RoleTickResult::Success, obs, "o/r", &[]);
    assert!(r.decisions_observed);
    let order: Vec<u32> = r.candidates.iter().map(|c| c.number).collect();
    assert_eq!(order, vec![8], "the latest snapshot is the served queue");
    let acted: Vec<(u32, &str)> = r
        .acted
        .iter()
        .map(|a| (a.number, a.action.as_str()))
        .collect();
    assert_eq!(acted, vec![(6, "claimed"), (6, "approved")]);
    let skipped: Vec<(u32, PickSkipReason)> =
        r.skipped.iter().map(|s| (s.number, s.reason)).collect();
    assert_eq!(skipped, vec![(8, PickSkipReason::NotSelected)]);
}

#[test]
fn a_refresh_ranks_the_latest_snapshot_not_a_first_seen_union() {
    // #3 arrives starred and #1 is gone: neither served queue ranked 1,2,3.
    let obs = journal(vec![
        queue(vec![
            qrow(1, "loom:review-requested", &[], "level=0"),
            qrow(2, "loom:review-requested", &[], "level=0"),
        ]),
        act(1, "claimed"),
        queue(vec![
            qrow(3, "loom:review-requested", &["loom:operator-priority"], "level=1"),
            qrow(2, "loom:review-requested", &[], "level=0"),
        ]),
    ]);
    let r = role_record(tick("judge"), RoleTickResult::Success, obs, "o/r", &[]);
    let ranked: Vec<(u32, u32)> = r.candidates.iter().map(|c| (c.rank, c.number)).collect();
    assert_eq!(ranked, vec![(1, 3), (2, 2)]);
    let acted: Vec<(u32, &str)> = r
        .acted
        .iter()
        .map(|a| (a.number, a.action.as_str()))
        .collect();
    assert_eq!(acted, vec![(1, "claimed")], "an earlier act stays recorded");
}

#[test]
fn an_observed_empty_queue_differs_from_an_unobserved_tick() {
    let empty = journal(vec![queue(vec![])]);
    let r = role_record(tick("doctor"), RoleTickResult::Success, empty, "o/r", &[]);
    assert_eq!((r.candidate_source.as_str(), r.decisions_observed), ("serving_queue", true));
    assert_eq!(r.candidates_total, 0);

    let unobserved = gate_only(vec![("loom:changes-requested".to_string(), vec![row(4, &[])])]);
    let r = role_record(tick("doctor"), RoleTickResult::Success, unobserved, "o/r", &[]);
    assert_eq!((r.candidate_source.as_str(), r.decisions_observed), ("gate_listing", false));
    assert!(r.skipped.is_empty(), "unobserved rows are undecided, not skipped");
}

#[test]
fn acts_are_unobserved_when_pr_queue_found_no_gh_front() {
    let obs = journal(vec![JournalEntry::Queue {
        at: at(),
        role: "champion".to_string(),
        acts_observable: false,
        total: 0,
        rows: vec![qrow(9, "loom:pr", &[], "x")],
    }]);
    let r = role_record(tick("champion"), RoleTickResult::Success, obs, "o/r", &[]);
    assert!(!r.decisions_observed);
    assert!(r.skipped.is_empty());
}

#[test]
fn curator_candidates_come_from_its_listings() {
    let listing = |rows| JournalEntry::Listing { at: at(), rows };
    let obs = RoleObservation {
        journal: Some(Journal {
            entries: vec![
                listing(vec![qrow(11, "loom:operator-priority", &[], "1")]),
                listing(vec![
                    qrow(12, "loom:issue", &[], "1"),
                    qrow(11, "loom:issue", &[], "2"),
                ]),
                act(12, "claimed"),
                act(12, "curated"),
                act(12, "promoted"),
            ],
            front_expected: false,
            read: true,
        }),
        gate: vec![],
    };
    let r = role_record(tick("curator"), RoleTickResult::Success, obs, "o/r", &[]);
    assert_eq!(r.candidate_source, "listing");
    assert!(r.decisions_observed, "the front's own entries prove it was active");
    let order: Vec<u32> = r.candidates.iter().map(|c| c.number).collect();
    assert_eq!(order, vec![11, 12]);
    assert_eq!(r.acted.len(), 3);
    assert_eq!((r.skipped[0].number, r.skipped[0].reason), (11, PickSkipReason::NotSelected));
}

#[test]
fn gate_stash_keeps_only_open_prs_and_drains_per_root() {
    clear_gate_listings();
    let pr = |n: u32, state: &str, is_pr: bool| RestIssue {
        comments: 0,
        number: n,
        title: None,
        labels: vec![],
        created_at: None,
        updated_at: None,
        closed_at: None,
        state: state.to_string(),
        body: None,
        author: None,
        author_association: None,
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

#[test]
fn a_queue_past_the_row_cap_keeps_its_uncapped_total() {
    // The journal kept 200 of 250 rows; the record lists 50 and says 250.
    let rows: Vec<JournalRow> = (1..=200)
        .map(|n| qrow(n, "loom:review-requested", &[], "k"))
        .collect();
    let obs = journal(vec![JournalEntry::Queue {
        at: at(),
        role: "judge".to_string(),
        acts_observable: true,
        total: 250,
        rows,
    }]);
    let r = role_record(tick("judge"), RoleTickResult::Success, obs, "o/r", &[]);
    assert_eq!(r.candidates.len(), 50);
    assert_eq!(r.candidates_total, 250);
}

#[test]
fn a_queue_line_without_a_total_falls_back_to_its_rows() {
    let obs = journal(vec![queue(vec![
        qrow(1, "loom:review-requested", &[], "a"),
        qrow(2, "loom:review-requested", &[], "b"),
    ])]);
    let r = role_record(tick("judge"), RoleTickResult::Success, obs, "o/r", &[]);
    assert_eq!(r.candidates_total, 2);
}

#[test]
fn an_unread_journal_leaves_decisions_unobserved() {
    let gate = vec![("loom:review-requested".to_string(), vec![row(1, &[]), row(2, &[])])];
    let unread = RoleObservation {
        journal: Some(Journal {
            entries: vec![],
            front_expected: true,
            read: false,
        }),
        gate: gate.clone(),
    };
    let r = role_record(tick("judge"), RoleTickResult::Success, unread, "o/r", &[]);
    assert_eq!(r.candidate_source, "gate_listing");
    assert!(!r.decisions_observed, "nothing was read, so nothing was observed");
    assert!(r.skipped.is_empty(), "unread rows are undecided, not not_selected");

    // A journal that was read and is genuinely empty is an observed "no act".
    let observed = RoleObservation {
        journal: Some(Journal {
            entries: vec![],
            front_expected: true,
            read: true,
        }),
        gate,
    };
    let r = role_record(tick("judge"), RoleTickResult::Success, observed, "o/r", &[]);
    assert!(r.decisions_observed);
    assert_eq!(r.skipped.len(), 2);
    assert!(r
        .skipped
        .iter()
        .all(|s| s.reason == PickSkipReason::NotSelected));
}
