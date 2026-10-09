//! `eta.stage_outcome` / `pr.resolved` from two `fleet.state` views (#11126).
//! Nothing here touches the ETA subsystem: no ETA state exists in these tests,
//! which is the daemon with `autonomous.eta.enabled = false`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, TimeZone, Utc};

use super::*;
use crate::observability::fleet_state::{
    build_view, FleetInput, FleetStateSink, HeldSweep, ListedPr, RepoListing,
};
use crate::observability::queue::QueueSink;
use crate::telemetry::TelemetryEnvelope;

const REPO: &str = "rjwalters/loom";

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 9, 12, 0, 0).unwrap()
}

fn provenance() -> Provenance {
    Provenance {
        version: "0.19.800".to_string(),
        revision: "9d8e226ce0123456789abcdef0123456789abcde".to_string(),
        tree_state: "clean".to_string(),
        complete: true,
    }
}

/// Canned forge answers, counting reads.
#[derive(Default)]
struct Forge {
    pulls: BTreeMap<u32, PullFacts>,
    labels: BTreeMap<u32, BTreeMap<String, DateTime<Utc>>>,
    reads: usize,
    /// `pull` reads that fail before one is answered.
    pull_failures: usize,
}

impl ForgeReads for Forge {
    fn pull(&mut self, _repo: &str, number: u32) -> Option<PullFacts> {
        self.reads += 1;
        if self.pull_failures > 0 {
            self.pull_failures -= 1;
            return None;
        }
        self.pulls.get(&number).copied()
    }

    fn label_times(&mut self, _repo: &str, number: u32) -> Option<BTreeMap<String, DateTime<Utc>>> {
        self.reads += 1;
        self.labels.get(&number).cloned()
    }
}

#[derive(Default)]
struct CapturingQueue {
    offered: Mutex<Vec<TelemetryEnvelope>>,
}

impl QueueSink for CapturingQueue {
    fn offer(&self, envelope: TelemetryEnvelope) {
        self.offered.lock().unwrap().push(envelope);
    }
    fn offer_durable(&self, envelope: TelemetryEnvelope) -> std::io::Result<()> {
        self.offer(envelope);
        Ok(())
    }
}

fn input(prs: &[(u32, &str, u32)], held: Vec<HeldSweep>) -> FleetInput {
    FleetInput {
        host_id: "host-a".to_string(),
        managed: [REPO.to_string()].into(),
        held,
        listings: vec![RepoListing {
            repo: REPO.to_string(),
            prs: prs
                .iter()
                .map(|(number, label, issue)| ListedPr {
                    number: *number,
                    labels: vec![(*label).to_string()],
                    issue: Some(*issue),
                })
                .collect(),
        }],
        listed_at: Some(t0()),
        ready: None,
    }
}

/// The records of passes over `inputs`, the first at `t0()` and each next
/// `step` later.
fn run(inputs: &[FleetInput], step: Duration, forge: &mut Forge) -> Vec<Vec<TelemetryRecord>> {
    let mut memory = Memory::default();
    let mut prev_view = None;
    let mut out = Vec::new();
    for (i, input) in inputs.iter().enumerate() {
        let now = t0() + step * i32::try_from(i).unwrap();
        let view = build_view(input, prev_view.as_ref(), now);
        let listed = listed(&input.listings);
        let pass = Pass {
            view: &view,
            listed: &listed,
            managed: &input.managed,
            now,
        };
        out.push(diff(&mut memory, &pass, forge, &provenance()));
        memory = remember(memory, &pass);
        prev_view = Some(view);
    }
    out
}

fn stages(records: &[TelemetryRecord]) -> Vec<&StageOutcomeRecord> {
    records
        .iter()
        .filter_map(|r| match r {
            TelemetryRecord::StageOutcome(s) => Some(s),
            _ => None,
        })
        .collect()
}

fn resolved(records: &[TelemetryRecord]) -> Vec<&PrResolvedRecord> {
    records
        .iter()
        .filter_map(|r| match r {
            TelemetryRecord::PrResolved(p) => Some(p),
            _ => None,
        })
        .collect()
}

#[test]
fn the_first_pass_and_an_unchanged_pass_give_nothing() {
    let mut forge = Forge::default();
    let pass = input(&[(21, "loom:review-requested", 20)], Vec::new());
    let out = run(&[pass.clone(), pass], Duration::minutes(5), &mut forge);
    assert!(out.iter().all(Vec::is_empty), "{out:?}");
    assert_eq!(forge.reads, 0);
}

#[test]
fn a_stage_change_across_two_views_is_exactly_one_stage_outcome() {
    let approved_at = t0() + Duration::minutes(8);
    let mut forge = Forge::default();
    forge
        .labels
        .insert(21, [("loom:pr".to_string(), approved_at)].into());
    let out = run(
        &[
            input(&[(21, "loom:review-requested", 20)], Vec::new()),
            input(&[(21, "loom:review-requested", 20)], Vec::new()),
            input(&[(21, "loom:pr", 20)], Vec::new()),
            input(&[(21, "loom:pr", 20)], Vec::new()),
        ],
        Duration::minutes(5),
        &mut forge,
    );
    assert!(out[1].is_empty() && out[3].is_empty());
    let records = stages(&out[2]);
    assert_eq!(records.len(), 1, "{out:?}");
    let r = records[0];
    assert_eq!((r.issue, r.pr_number), (20, Some(21)));
    assert_eq!(r.stage, FleetStage::ReviewWait);
    assert_eq!(r.next_stage, Some(FleetStage::MergeWait));
    assert_eq!(r.exit, StageExit::Pass);
    assert_eq!(r.forge_transition_at, Some(approved_at), "the label event");
    assert_eq!(r.left_at, approved_at);
    assert_eq!(r.resolution_sec, Some(0));
    assert_eq!(r.observed_at, t0() + Duration::minutes(10));
    assert_eq!(r.entered_at, None, "first seen mid-stage");
    assert_eq!(r.dwell_sec, None);
    assert_eq!(r.event, "fleet.state");
}

#[test]
fn a_stage_outcome_with_no_forge_instant_is_still_emitted_at_the_pass() {
    let mut forge = Forge::default();
    let stale = t0() - Duration::days(3);
    forge
        .labels
        .insert(21, [("loom:pr".to_string(), stale)].into());
    let out = run(
        &[
            input(&[(21, "loom:review-requested", 20)], Vec::new()),
            input(&[(21, "loom:pr", 20)], Vec::new()),
        ],
        Duration::minutes(5),
        &mut forge,
    );
    let r = stages(&out[1])[0];
    assert_eq!(r.forge_transition_at, None, "a stale label event is not this move");
    assert_eq!(r.left_at, t0() + Duration::minutes(5));
    assert_eq!(r.resolution_sec, Some(300));
}

#[test]
fn a_polled_entry_between_two_passes_has_no_dwell() {
    let mut forge = Forge::default();
    let approved_at = t0() + Duration::minutes(8);
    forge
        .labels
        .insert(21, [("loom:pr".to_string(), approved_at)].into());
    let out = run(
        &[
            input(&[], Vec::new()),
            input(&[(21, "loom:review-requested", 20)], Vec::new()),
            input(&[(21, "loom:pr", 20)], Vec::new()),
        ],
        Duration::minutes(5),
        &mut forge,
    );
    // The review began at some instant in (t0, t0 + 5m]; the pass that first
    // saw it is not that instant, so an exact dwell would be a guess.
    let r = stages(&out[2])[0];
    assert_eq!(r.entered_at, None);
    assert_eq!(r.dwell_sec, None);
}

#[test]
fn a_merged_pr_leaving_gives_landed_and_pr_resolved() {
    let merged_at = t0() + Duration::minutes(7);
    let mut forge = Forge::default();
    forge.pulls.insert(
        21,
        PullFacts {
            merged_at: Some(merged_at),
            closed_at: Some(merged_at),
        },
    );
    let out = run(
        &[
            input(&[(21, "loom:pr", 20)], Vec::new()),
            input(&[], Vec::new()),
        ],
        Duration::minutes(10),
        &mut forge,
    );
    let records = stages(&out[1]);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].exit, StageExit::Landed);
    assert_eq!(records[0].left_at, merged_at);
    assert_eq!(records[0].forge_transition_at, Some(merged_at));
    assert_eq!(records[0].next_stage, None);
    let prs = resolved(&out[1]);
    assert_eq!(prs.len(), 1);
    assert_eq!((prs[0].pr_number, prs[0].issue), (21, Some(20)));
    assert_eq!(prs[0].state, PrResolution::Merged);
    assert_eq!(prs[0].resolved_at, merged_at);
    assert_eq!(prs[0].closed_at, Some(merged_at));
    assert_eq!(prs[0].resolution_sec, 0);
}

#[test]
fn a_closed_pr_leaving_gives_cut_short_and_pr_resolved() {
    let closed_at = t0() + Duration::minutes(2);
    let mut forge = Forge::default();
    forge.pulls.insert(
        31,
        PullFacts {
            merged_at: None,
            closed_at: Some(closed_at),
        },
    );
    let out = run(
        &[
            input(&[(31, "loom:changes-requested", 30)], Vec::new()),
            input(&[], Vec::new()),
        ],
        Duration::minutes(5),
        &mut forge,
    );
    let r = stages(&out[1])[0];
    assert_eq!((r.stage, r.exit), (FleetStage::Doctor, StageExit::CutShort));
    assert_eq!(r.left_at, closed_at);
    assert_eq!(r.dwell_sec, None, "cut short has no dwell");
    let p = resolved(&out[1])[0];
    assert_eq!((p.state, p.resolved_at), (PrResolution::Closed, closed_at));
}

#[test]
fn a_pr_still_open_after_leaving_gives_no_pr_resolved() {
    let mut forge = Forge::default();
    forge.pulls.insert(
        21,
        PullFacts {
            merged_at: None,
            closed_at: None,
        },
    );
    let out = run(
        &[
            input(&[(21, "loom:pr", 20)], Vec::new()),
            input(&[], Vec::new()),
        ],
        Duration::minutes(5),
        &mut forge,
    );
    assert!(resolved(&out[1]).is_empty());
    let r = stages(&out[1])[0];
    assert_eq!((r.exit, r.forge_transition_at), (StageExit::CutShort, None));
}

#[test]
fn a_failed_listing_is_not_a_departure() {
    let mut forge = Forge::default();
    let mut failed = input(&[], Vec::new());
    failed.listings.clear();
    failed.listed_at = None;
    let out = run(
        &[input(&[(21, "loom:pr", 20)], Vec::new()), failed],
        Duration::minutes(5),
        &mut forge,
    );
    assert!(out[1].is_empty(), "{out:?}");
    assert_eq!(forge.reads, 0);
}

#[test]
fn a_held_sweep_advancing_is_a_stage_outcome_without_a_forge_instant() {
    let held = |stage| HeldSweep {
        repo: REPO.to_string(),
        issue: 40,
        stage,
        entered_at: t0() - Duration::minutes(20),
        entered_at_lower_bound: false,
        pr: None,
        overflow: false,
    };
    let mut forge = Forge::default();
    let out = run(
        &[
            input(&[], vec![held(FleetStage::SweepCurator)]),
            input(&[], vec![held(FleetStage::SweepBuilder)]),
        ],
        Duration::minutes(5),
        &mut forge,
    );
    let r = stages(&out[1])[0];
    assert_eq!((r.stage, r.exit), (FleetStage::SweepCurator, StageExit::Advance));
    assert_eq!(r.forge_transition_at, None);
    assert_eq!(r.entered_at, Some(t0() - Duration::minutes(20)), "the checkpoint dates it");
    assert_eq!(r.dwell_sec, Some(1500));
    assert_eq!(forge.reads, 0, "a sweep stage has no forge label");
}

#[test]
fn stage_outcome_emits_with_eta_disabled() {
    let mut forge = Forge::default();
    let out = run(
        &[
            input(&[(21, "loom:review-requested", 20)], Vec::new()),
            input(&[(21, "loom:changes-requested", 20)], Vec::new()),
        ],
        Duration::minutes(5),
        &mut forge,
    );
    let queue = Arc::new(CapturingQueue::default());
    let sink = FleetStateSink::new(queue.clone(), "host-a");
    assert_eq!(offer(out[1].clone(), &sink), 1);
    let offered = queue.offered.lock().unwrap();
    assert_eq!(offered[0].record.kind(), "eta.stage_outcome");
    assert_eq!(offered[0].host_id, "host-a");
}

#[test]
fn pr_resolved_emits_with_eta_disabled() {
    let mut forge = Forge::default();
    let at = t0() + Duration::minutes(1);
    forge.pulls.insert(
        21,
        PullFacts {
            merged_at: Some(at),
            closed_at: Some(at),
        },
    );
    let out = run(
        &[
            input(&[(21, "loom:pr", 20)], Vec::new()),
            input(&[], Vec::new()),
        ],
        Duration::minutes(5),
        &mut forge,
    );
    let queue = Arc::new(CapturingQueue::default());
    let sink = FleetStateSink::new(queue.clone(), "host-a");
    assert_eq!(offer(out[1].clone(), &sink), 2);
    let offered = queue.offered.lock().unwrap();
    let kinds: Vec<&str> = offered.iter().map(|e| e.record.kind()).collect();
    assert!(kinds.contains(&"pr.resolved"), "{kinds:?}");
}

#[test]
fn a_record_without_valid_provenance_is_never_offered() {
    let mut forge = Forge::default();
    let mut out = run(
        &[
            input(&[(21, "loom:review-requested", 20)], Vec::new()),
            input(&[(21, "loom:changes-requested", 20)], Vec::new()),
        ],
        Duration::minutes(5),
        &mut forge,
    );
    if let TelemetryRecord::StageOutcome(r) = &mut out[1][0] {
        r.loom.revision = "not-a-sha".to_string();
    }
    let queue = Arc::new(CapturingQueue::default());
    let sink = FleetStateSink::new(queue.clone(), "host-a");
    assert_eq!(offer(out[1].clone(), &sink), 0);
    assert!(queue.offered.lock().unwrap().is_empty());
}

#[test]
fn an_unread_departure_is_retried_until_it_resolves_exactly_once() {
    let at = t0() + Duration::minutes(7);
    let mut forge = Forge {
        pull_failures: 1,
        ..Forge::default()
    };
    forge.pulls.insert(
        21,
        PullFacts {
            merged_at: Some(at),
            closed_at: Some(at),
        },
    );
    let out = run(
        &[
            input(&[(21, "loom:pr", 20)], Vec::new()),
            input(&[], Vec::new()),
            input(&[], Vec::new()),
            input(&[], Vec::new()),
        ],
        Duration::minutes(10),
        &mut forge,
    );
    assert!(resolved(&out[1]).is_empty(), "the first read failed");
    assert_eq!(stages(&out[1])[0].exit, StageExit::Unknown);
    let late = resolved(&out[2]);
    assert_eq!(late.len(), 1, "{out:?}");
    assert_eq!((late[0].pr_number, late[0].state), (21, PrResolution::Merged));
    assert_eq!(late[0].resolved_at, at);
    assert!(resolved(&out[3]).is_empty(), "read once, not again");
    assert_eq!(forge.reads, 2);
}

#[test]
fn a_hold_release_is_not_dated_by_the_old_approval_label() {
    let held_labels = |labels: &[&str]| {
        let mut i = input(&[(21, "loom:pr", 20)], Vec::new());
        i.listings[0].prs[0].labels = labels.iter().map(|l| (*l).to_string()).collect();
        i
    };
    let hold = MERGE_HOLD_LABELS.iter().next().expect("a hold label");
    let mut forge = Forge::default();
    // The approval is inside the release pass's window and is the only
    // `loom:pr` instant on the PR.
    forge
        .labels
        .insert(21, [("loom:pr".to_string(), t0() + Duration::minutes(1))].into());
    let held = held_labels(&["loom:pr", hold]);
    let open = held_labels(&["loom:pr"]);
    let out = run(
        &[held.clone(), held.clone(), open.clone(), held, open],
        Duration::minutes(5),
        &mut forge,
    );
    for pass in [&out[2], &out[4]] {
        let r = stages(pass);
        assert_eq!(r.len(), 1, "{out:?}");
        assert_eq!(r[0].stage, FleetStage::MergeHold);
        assert_eq!(r[0].next_stage, Some(FleetStage::MergeWait));
        assert_eq!(r[0].forge_transition_at, None, "no hold-removal instant is known");
        assert_eq!(r[0].resolution_sec, Some(300));
    }
    assert_eq!(forge.reads, 1, "only the re-hold reads events, never a release");
}
