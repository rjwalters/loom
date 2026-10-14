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
                "workspace_weight": 100, "urgent": false,
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
fn a_file_overlap_deferral_carries_its_shared_paths_into_the_record() {
    let mut s = summary(vec![
        (1, QueueDisposition::Dispatched),
        (2, QueueDisposition::DeferredFileOverlap),
        (3, QueueDisposition::DispatchError),
    ]);
    s.queue[1].detail = Some("file overlap: src/a.rs, src/b.rs".to_string());
    s.queue[2].detail = Some("free-form error text".to_string());
    let r = work_finder_record(&s, "host-a", at(), at(), resolve);
    let overlap = r.skipped.iter().find(|k| k.number == 2).unwrap();
    assert_eq!(overlap.reason, PickSkipReason::OverlapChain);
    assert_eq!(overlap.detail.as_deref(), Some("file overlap: src/a.rs, src/b.rs"));
    let error = r.skipped.iter().find(|k| k.number == 3).unwrap();
    assert_eq!(error.detail, None, "only the file-overlap detail is exported");
    let json = serde_json::to_value(&r).unwrap();
    assert_eq!(json["skipped"][0]["detail"], "file overlap: src/a.rs, src/b.rs");
    assert!(json["skipped"][1].get("detail").is_none(), "absent, not null");
}

/// #11811: each skip carries the daemon's un-collapsed disposition, so the
/// four caps `cap` lumps together stay distinct, plus its `held_until`.
#[test]
fn a_skip_carries_its_uncollapsed_disposition_and_hold_expiry() {
    let mut s = summary(vec![
        (1, QueueDisposition::Dispatched),
        (2, QueueDisposition::DeferredRampCap),
        (3, QueueDisposition::DeferredCapacity),
        (4, QueueDisposition::DeferredRepoCap),
        (5, QueueDisposition::DeferredSaturation),
        (6, QueueDisposition::DispatchBackoff),
    ]);
    let until = Utc.with_ymd_and_hms(2026, 10, 4, 12, 30, 0).unwrap();
    s.queue[5].plan.held_until = Some(until);
    let r = work_finder_record(&s, "host-a", at(), at(), resolve);
    let got: Vec<(u32, PickSkipReason, Option<QueueDisposition>)> = r
        .skipped
        .iter()
        .map(|k| (k.number, k.reason, k.disposition))
        .collect();
    assert_eq!(
        got,
        vec![
            (2, PickSkipReason::Cap, Some(QueueDisposition::DeferredRampCap)),
            (3, PickSkipReason::Cap, Some(QueueDisposition::DeferredCapacity)),
            (4, PickSkipReason::Cap, Some(QueueDisposition::DeferredRepoCap)),
            (5, PickSkipReason::Cap, Some(QueueDisposition::DeferredSaturation)),
            (6, PickSkipReason::Backoff, Some(QueueDisposition::DispatchBackoff)),
        ]
    );
    let json = serde_json::to_value(&r).unwrap();
    assert_eq!(json["skipped"][0]["reason"], "cap", "reason is unchanged");
    assert_eq!(json["skipped"][0]["disposition"], "deferred_ramp_cap");
    assert!(json["skipped"][0].get("held_until").is_none(), "absent, not null");
    assert_eq!(json["skipped"][4]["disposition"], "dispatch_backoff");
    assert_eq!(json["skipped"][4]["held_until"], "2026-10-04T12:30:00Z");
    // Older records (no disposition / held_until) still parse.
    let mut old = json.clone();
    for k in old["skipped"].as_array_mut().unwrap() {
        let k = k.as_object_mut().unwrap();
        k.remove("disposition");
        k.remove("held_until");
    }
    let parsed: PickDecisionRecord = serde_json::from_value(old).unwrap();
    assert!(parsed
        .skipped
        .iter()
        .all(|k| k.disposition.is_none() && k.held_until.is_none()));
}

/// #11811: a halted row names its cause, but only a closed-vocabulary token.
#[test]
fn a_halted_skip_carries_only_a_closed_vocabulary_cause() {
    let mut s = summary(vec![
        (1, QueueDisposition::WorkspaceHalted),
        (2, QueueDisposition::WorkspaceHalted),
        (3, QueueDisposition::WorkspaceHalted),
    ]);
    s.queue[0].detail = Some("drain".to_string());
    s.queue[1].detail = Some("not a cause".to_string());
    let r = work_finder_record(&s, "host-a", at(), at(), resolve);
    let details: Vec<Option<&str>> = r.skipped.iter().map(|k| k.detail.as_deref()).collect();
    assert_eq!(details, vec![Some("drain"), None, None]);
    assert!(r.skipped.iter().all(|k| k.reason == PickSkipReason::Halted
        && k.disposition == Some(QueueDisposition::WorkspaceHalted)));
}

/// #11858: an already-landed skip names its evidence, closed vocabulary only.
#[test]
fn an_already_landed_skip_carries_only_its_evidence_token() {
    let mut s = summary(vec![
        (1, QueueDisposition::AlreadyLanded),
        (2, QueueDisposition::AlreadyLanded),
        (3, QueueDisposition::AlreadyLanded),
    ]);
    s.queue[0].detail = Some("journal_landed".to_string());
    s.queue[1].detail = Some("forge_closed".to_string());
    s.queue[2].detail = Some("landed by a free-text reason".to_string());
    let r = work_finder_record(&s, "host-a", at(), at(), resolve);
    let details: Vec<Option<&str>> = r.skipped.iter().map(|k| k.detail.as_deref()).collect();
    assert_eq!(details, vec![Some("journal_landed"), Some("forge_closed"), None]);
}

/// #11857: the outcome agrees with the tick result. A hold that covered the
/// tick's work reads `halted`; one held root beside a healthy repo with
/// candidates is a partial hold and the outcome follows the rows.
#[test]
fn a_partial_hold_is_not_a_halted_outcome() {
    let mut s = summary(vec![
        (1, QueueDisposition::WorkspaceHalted),
        (2, QueueDisposition::OpenPr),
    ]);
    s.halted = true;
    s.held_roots = 1;
    s.halt_partial = true;
    let r = work_finder_record(&s, "host-a", at(), at(), resolve);
    assert_eq!(r.outcome, "none_dispatched");

    s.queue.clear();
    let r = work_finder_record(&s, "host-a", at(), at(), resolve);
    assert_eq!(r.outcome, "idle");

    // A full hold still reads `halted`, as does a pre-#11857 summary that
    // carries no `halt_partial` (the old meaning of `halted`).
    s.halt_partial = false;
    let r = work_finder_record(&s, "host-a", at(), at(), resolve);
    assert_eq!(r.outcome, "halted");
}

/// Role ticks have no work-finder disposition: the fields stay absent.
#[test]
fn a_role_skip_carries_no_disposition() {
    let r = role_record(
        tick("judge"),
        RoleTickResult::SkippedLoad,
        gate_only(vec![("loom:review-requested".into(), vec![row(7, &[])])]),
        "o/r",
        &[],
    );
    assert_eq!(r.skipped.len(), 1);
    assert_eq!((r.skipped[0].disposition, r.skipped[0].held_until), (None, None));
    let json = serde_json::to_value(&r).unwrap();
    assert!(json["skipped"][0].get("disposition").is_none());
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
    // #11858: an empty queue still names its repo.
    assert_eq!(r.repo.as_deref(), Some("o/r"));
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
    // pr-queue put the important #3 first, then #1; #2 is not in the queue.
    let obs = journal(vec![queue(vec![
        qrow(3, "loom:review-requested", &["loom:important"], "level=1"),
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
    // #3 arrives important and #1 is gone: neither served queue ranked 1,2,3.
    let obs = journal(vec![
        queue(vec![
            qrow(1, "loom:review-requested", &[], "level=0"),
            qrow(2, "loom:review-requested", &[], "level=0"),
        ]),
        act(1, "claimed"),
        queue(vec![
            qrow(3, "loom:review-requested", &["loom:important"], "level=1"),
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
                listing(vec![qrow(11, "loom:important", &[], "1")]),
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
        open_sub_issues: None,
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
fn a_queue_past_the_journal_row_cap_lists_every_kept_row() {
    // The journal kept 200 of 250 rows; the record lists all 200 and says 250.
    // The journal's own 200-row cap is upstream of the record and unchanged.
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
    assert_eq!(r.candidates.len(), 200);
    assert_eq!(r.candidates_total, 250);
    assert_eq!(r.skipped.len(), 200, "every kept row's decision is carried");
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

/// #11103: the work finder's record carries the tick's workspace draws —
/// seed, every draw's candidates with weights, roll and pick — named by repo.
#[test]
fn work_finder_record_carries_the_workspace_draw() {
    use crate::work_finder::workspace_draw::draw_order;
    use crate::work_finder::{ready_queue, WorkItem};
    let item = |n: u32| WorkItem::with_created_at(n, vec!["loom:issue".into()], None);
    let candidates = vec![
        ready_queue::key_of(0, 0, &item(1), false),
        ready_queue::key_of(1, 1, &item(2), false),
    ];
    let (order, log) =
        draw_order(candidates, &[3, 1], 99, &crate::priority_pick::WeightBonus::default());
    let log = log.unwrap();
    let names = ["o/tool", "o/product"];
    let draw = workspace_draw_record(&log, |idx| names[idx].to_string());
    let r = work_finder_record(&summary(Vec::new()), "h", at(), at(), resolve)
        .with_workspace_draw(Some(draw));

    let json = serde_json::to_value(&r).unwrap();
    let d = &json["workspace_draw"];
    assert_eq!(d["seed"], "99");
    assert_eq!(d["draws_total"], 2);
    // A draw with no derived seed (pinned / passed in) names no inputs.
    assert!(d.get("seed_scheme").is_none());
    let first = &d["draws"][0];
    assert!(first.get("pool").is_none(), "no tiers since #11742");
    assert!(d.get("temperature").is_none());
    // #11742: linear weights, with each candidate's base weight and bonus.
    assert_eq!(
        first["candidates"][0],
        serde_json::json!({"repo": "o/tool", "weight": 3, "base_weight": 3, "bonus": 0})
    );
    assert_eq!(
        first["candidates"][1],
        serde_json::json!({"repo": "o/product", "weight": 1, "base_weight": 1, "bonus": 0})
    );
    assert_eq!(first["total_weight"], 4);
    assert!(first["roll"].as_u64().unwrap() < 4);
    assert_eq!(first["picked"], names[order[0].workspace_idx]);
    assert_eq!(first["number"], order[0].number);
    // The second draw is over the one workspace left.
    assert_eq!(d["draws"][1]["candidates"].as_array().unwrap().len(), 1);
    // It round-trips, and a role record (no draw) omits the field.
    let back: PickDecisionRecord = serde_json::from_value(json.clone()).unwrap();
    assert_eq!(back, r);
    // A pre-#11742 record (tier `pool`, softmax `temperature` / `p_min`, no
    // `base_weight` / `bonus`) still parses.
    let mut old = json;
    old["workspace_draw"]["temperature"] = serde_json::json!(80);
    for draw in old["workspace_draw"]["draws"].as_array_mut().unwrap() {
        draw["pool"] = serde_json::json!("all");
        draw["p_min"] = serde_json::json!(0);
        for c in draw["candidates"].as_array_mut().unwrap() {
            let c = c.as_object_mut().unwrap();
            c.remove("base_weight");
            c.remove("bonus");
        }
    }
    let back: PickDecisionRecord = serde_json::from_value(old).unwrap();
    let back_draw = back.workspace_draw.unwrap();
    assert!(back_draw
        .draws
        .iter()
        .all(|d| d.candidates.iter().all(|c| c.bonus.is_none())));
    let plain = work_finder_record(&summary(Vec::new()), "h", at(), at(), resolve);
    assert!(serde_json::to_value(&plain)
        .unwrap()
        .get("workspace_draw")
        .is_none());
}

/// #11683: a clock-and-host-seeded draw exports its inputs beside the seed,
/// and they recompute it.
#[test]
fn work_finder_record_carries_the_seed_inputs() {
    use crate::work_finder::workspace_draw::{
        draw_order, minute_seed, SeedInputs, DRAW_SEED_SCHEME,
    };
    use crate::work_finder::{ready_queue, WorkItem};
    let item = |n: u32| WorkItem::with_created_at(n, vec!["loom:issue".into()], None);
    let seed = minute_seed(29_000_000, "host-a");
    let (_, log) = draw_order(
        vec![ready_queue::key_of(0, 0, &item(1), false)],
        &[1],
        seed,
        &crate::priority_pick::WeightBonus::default(),
    );
    let mut log = log.unwrap();
    log.seed_inputs = Some(SeedInputs {
        scheme: DRAW_SEED_SCHEME,
        minute: 29_000_000,
        host: "host-a".into(),
    });
    let d = serde_json::to_value(workspace_draw_record(&log, |_| "o/r".into())).unwrap();
    assert_eq!(d["seed"], "18365900956060715395");
    assert_eq!(d["seed_scheme"], "loom-draw/v1");
    assert_eq!(d["seed_minute"], "29000000");
    assert_eq!(d["seed_host"], "host-a");
    let minute: u64 = d["seed_minute"].as_str().unwrap().parse().unwrap();
    assert_eq!(
        minute_seed(minute, d["seed_host"].as_str().unwrap()).to_string(),
        d["seed"].as_str().unwrap()
    );
}

/// #11509: the work finder's record carries the tick's `loom:very-important`
/// flood-cap demotions, named by repo; a tick with none omits the field.
#[test]
fn work_finder_record_carries_the_very_important_demotions() {
    use crate::work_finder::vi_cap::ViDemotion;
    let demoted = [ViDemotion {
        workspace_idx: 1,
        number: 30,
        holder: 20,
    }];
    let names = ["o/tool", "o/product"];
    let records = vi_demotion_records(&demoted, |idx| names[idx].to_string());
    let r = work_finder_record(&summary(Vec::new()), "h", at(), at(), resolve)
        .with_very_important_demoted(records);
    let json = serde_json::to_value(&r).unwrap();
    assert_eq!(
        json["very_important_demoted"],
        serde_json::json!([{"repo": "o/product", "number": 30, "holder": 20}])
    );
    let back: PickDecisionRecord = serde_json::from_value(json).unwrap();
    assert_eq!(back, r);
    let plain = work_finder_record(&summary(Vec::new()), "h", at(), at(), resolve);
    assert!(serde_json::to_value(&plain)
        .unwrap()
        .get("very_important_demoted")
        .is_none());
}

#[test]
fn lane_plan_is_stashed_converted_and_drained_once() {
    use crate::role_runner::concurrent_dispatch::lane_rule::RepoLane;
    stash_lane_plan(
        Path::new("/tmp/loom-10630-obs"),
        vec![RepoLane {
            root: "/tmp/loom-10630-obs".into(),
            debt: 60,
            wanted: 3,
            lanes: 2,
        }],
    );
    let lanes = take_lane_plan();
    assert_eq!((lanes.len(), lanes[0].debt, lanes[0].wanted, lanes[0].lanes), (1, 60, 3, 2));
    assert!(take_lane_plan().is_empty(), "drained");
    let json = serde_json::to_value(&lanes).unwrap();
    assert_eq!(json[0]["debt"], 60);
}
