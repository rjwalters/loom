//! `eta.stage_outcome` / `pr.resolved` from two `fleet.state` views (#11126).
//! Nothing here touches the ETA subsystem: no ETA state exists in these tests,
//! which is the daemon with `autonomous.eta.enabled = false`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, TimeZone, Utc};

use super::*;
use crate::observability::fleet_state::history::HistoryEvent;
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
    histories: BTreeMap<u32, LabelHistory>,
    reads: usize,
    /// History reads per number.
    history_reads: BTreeMap<u32, usize>,
    /// `pull` reads that fail before one is answered.
    pull_failures: usize,
    /// Every history read fails.
    history_fails: bool,
    /// The pass being run: a history shows only the events up to it.
    clock: Option<DateTime<Utc>>,
}

impl Forge {
    fn event(&mut self, number: u32, at: DateTime<Utc>, event: HistoryEvent) {
        let events = &mut self.histories.entry(number).or_default().events;
        events.push((at, event));
        events.sort_by_key(|(at, _)| *at);
    }

    fn label(&mut self, number: u32, label: &str, at: DateTime<Utc>) {
        self.event(number, at, HistoryEvent::Labeled(label.to_string()));
    }

    fn unlabel(&mut self, number: u32, label: &str, at: DateTime<Utc>) {
        self.event(number, at, HistoryEvent::Unlabeled(label.to_string()));
    }

    fn pull_reads(&self) -> usize {
        self.reads - self.history_reads.values().sum::<usize>()
    }
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

    fn label_history(&mut self, _repo: &str, number: u32) -> Option<LabelHistory> {
        self.reads += 1;
        *self.history_reads.entry(number).or_default() += 1;
        if self.history_fails {
            return None;
        }
        let clock = self.clock;
        self.histories.get(&number).map(|h| LabelHistory {
            events: h
                .events
                .iter()
                .filter(|(at, _)| clock.is_none_or(|now| *at <= now))
                .cloned()
                .collect(),
        })
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
        capacity: None,
        main_ci: BTreeMap::new(),
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
        forge.clock = Some(now);
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
    forge.label(21, "loom:pr", approved_at);
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
    assert_eq!(r.entered_at, None, "no review label event");
    assert_eq!(r.entered_at_source, Some(EnteredAtSource::Unknown));
    assert_eq!(r.dwell_sec, None);
    assert_eq!(r.event, "fleet.state");
}

#[test]
fn a_stage_outcome_with_no_forge_instant_is_still_emitted_at_the_pass() {
    let mut forge = Forge::default();
    let stale = t0() - Duration::days(3);
    forge.label(21, "loom:pr", stale);
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
fn a_label_event_inside_the_preceding_interval_is_not_this_move() {
    // The previous pass (12:05) already saw `review_wait`; a `loom:pr` event
    // at 12:02 is from an earlier visit, not the move seen at 12:10.
    let mut forge = Forge::default();
    forge.label(21, "loom:pr", t0() + Duration::minutes(2));
    let out = run(
        &[
            input(&[], Vec::new()),
            input(&[(21, "loom:review-requested", 20)], Vec::new()),
            input(&[(21, "loom:pr", 20)], Vec::new()),
        ],
        Duration::minutes(5),
        &mut forge,
    );
    let r = stages(&out[2])[0];
    assert_eq!(r.forge_transition_at, None);
    assert_eq!(r.left_at, t0() + Duration::minutes(10));
    assert_eq!(r.resolution_sec, Some(300));
}

fn held_merge_wait() -> HeldSweep {
    HeldSweep {
        repo: REPO.to_string(),
        issue: 20,
        stage: FleetStage::MergeWait,
        entered_at: t0() - Duration::minutes(20),
        entered_at_lower_bound: false,
        pr: Some(21),
        overflow: false,
        start: Default::default(),
    }
}

fn merged_at_7() -> (DateTime<Utc>, Forge) {
    let merged_at = t0() + Duration::minutes(7);
    let mut forge = Forge::default();
    forge.pulls.insert(
        21,
        PullFacts {
            merged_at: Some(merged_at),
            closed_at: Some(merged_at),
        },
    );
    (merged_at, forge)
}

#[test]
fn a_held_row_persisting_across_a_merge_gives_one_landed_outcome() {
    let (merged_at, mut forge) = merged_at_7();
    let out = run(
        &[
            input(&[(21, "loom:pr", 20)], vec![held_merge_wait()]),
            input(&[], vec![held_merge_wait()]),
            input(&[], vec![held_merge_wait()]),
            input(&[], Vec::new()),
            input(&[], Vec::new()),
        ],
        Duration::minutes(10),
        &mut forge,
    );
    let records = stages(&out[1]);
    assert_eq!(records.len(), 1, "{out:?}");
    assert_eq!(records[0].exit, StageExit::Landed);
    assert_eq!(records[0].stage, FleetStage::MergeWait);
    assert_eq!(records[0].next_stage, None);
    assert_eq!(records[0].forge_transition_at, Some(merged_at));
    assert_eq!(resolved(&out[1]).len(), 1);
    // The sweep then disappears: the merge is not reported a second time, nor
    // as `unknown`.
    for later in &out[2..] {
        assert!(stages(later).is_empty(), "{later:?}");
    }
}

#[test]
fn a_ready_row_surviving_the_review_departure_gives_landed_not_advance() {
    use crate::observability::fleet_state::{ReadyItem, ReadyQueue};
    let (merged_at, mut forge) = merged_at_7();
    let ready = || {
        let mut with_ready = input(&[], Vec::new());
        with_ready.ready = Some(ReadyQueue {
            items: vec![ReadyItem {
                repo: REPO.to_string(),
                issue: 20,
                rank: 1,
                star: false,
                star_at: None,
                level: 0,
                fleet_priority: 0,
                created_at: None,
                main_red_fix: false,
            }],
            listed: [REPO.to_string()].into(),
            complete: BTreeSet::new(),
            slots: None,
        });
        with_ready
    };
    let out = run(
        &[input(&[(21, "loom:pr", 20)], Vec::new()), ready(), ready()],
        Duration::minutes(10),
        &mut forge,
    );
    let records = stages(&out[1]);
    assert_eq!(records.len(), 1, "{out:?}");
    assert_eq!(records[0].exit, StageExit::Landed);
    assert_eq!(records[0].next_stage, None);
    assert_eq!(records[0].forge_transition_at, Some(merged_at));
    assert!(stages(&out[2]).is_empty(), "{:?}", out[2]);
}

/// The forge's `labeled` event dates a polled entry; the exit and the entry
/// share one events read.
#[test]
fn a_polled_entry_is_dated_by_its_label_event() {
    let mut forge = Forge::default();
    forge.label(21, "loom:review-requested", t0() + Duration::minutes(2));
    forge.label(21, "loom:pr", t0() + Duration::minutes(8));
    let out = run(
        &[
            input(&[], Vec::new()),
            input(&[(21, "loom:review-requested", 20)], Vec::new()),
            input(&[(21, "loom:pr", 20)], Vec::new()),
        ],
        Duration::minutes(5),
        &mut forge,
    );
    let r = stages(&out[2])[0];
    assert_eq!(r.entered_at, Some(t0() + Duration::minutes(2)));
    assert_eq!(r.entered_at_source, Some(EnteredAtSource::Forge));
    assert_eq!(r.dwell_sec, Some(360));
    assert_eq!(forge.history_reads[&21], 1, "one read dates the exit and the entry");
}

/// After a restart every row is first seen mid-stage; the forge still dates it.
#[test]
fn a_row_first_seen_after_a_restart_is_still_dated_by_the_forge() {
    let mut forge = Forge::default();
    forge.label(21, "loom:review-requested", t0() - Duration::hours(1));
    forge.label(21, "loom:pr", t0() + Duration::minutes(8));
    let review = input(&[(21, "loom:review-requested", 20)], Vec::new());
    let out = run(
        &[
            review.clone(),
            review,
            input(&[(21, "loom:pr", 20)], Vec::new()),
        ],
        Duration::minutes(5),
        &mut forge,
    );
    let r = stages(&out[2])[0];
    assert_eq!(r.entered_at, Some(t0() - Duration::hours(1)));
    assert_eq!(r.dwell_sec, Some(3600 + 480));
}

/// The entry is this visit's: not an older visit, not a relabel after the
/// pass that first saw the visit.
#[test]
fn the_entry_is_the_latest_label_event_at_or_before_the_first_sighting() {
    let mut forge = Forge::default();
    for at in [-2 * 24 * 60, 2, 7] {
        forge.label(21, "loom:review-requested", t0() + Duration::minutes(at));
    }
    forge.label(21, "loom:pr", t0() + Duration::minutes(8));
    let out = run(
        &[
            input(&[], Vec::new()),
            input(&[(21, "loom:review-requested", 20)], Vec::new()),
            input(&[(21, "loom:pr", 20)], Vec::new()),
        ],
        Duration::minutes(5),
        &mut forge,
    );
    assert_eq!(stages(&out[2])[0].entered_at, Some(t0() + Duration::minutes(2)));
}

#[test]
fn a_released_hold_dates_the_merge_wait_entry() {
    let (merged_at, mut forge) = merged_at_7();
    let hold = *MERGE_HOLD_LABELS.iter().next().expect("a hold label");
    forge.label(21, "loom:pr", t0() - Duration::hours(2));
    forge.label(21, hold, t0() - Duration::hours(1));
    forge.unlabel(21, hold, t0() - Duration::minutes(10));
    let out = run(
        &[
            input(&[(21, "loom:pr", 20)], Vec::new()),
            input(&[], Vec::new()),
        ],
        Duration::minutes(10),
        &mut forge,
    );
    let r = stages(&out[1])[0];
    assert_eq!((r.stage, r.exit), (FleetStage::MergeWait, StageExit::Landed));
    assert_eq!(r.entered_at, Some(t0() - Duration::minutes(10)));
    assert_eq!(r.dwell_sec, Some((merged_at - (t0() - Duration::minutes(10))).num_seconds()));
}

#[test]
fn a_failed_history_read_leaves_the_entry_unknown() {
    let mut forge = Forge {
        history_fails: true,
        ..Forge::default()
    };
    let out = run(
        &[
            input(&[(21, "loom:review-requested", 20)], Vec::new()),
            input(&[(21, "loom:pr", 20)], Vec::new()),
        ],
        Duration::minutes(5),
        &mut forge,
    );
    let r = stages(&out[1])[0];
    assert_eq!((r.entered_at, r.dwell_sec), (None, None));
    assert_eq!(r.entered_at_source, Some(EnteredAtSource::Unknown));
    assert_eq!(r.forge_transition_at, None);
    assert_eq!(forge.reads, 1, "a failed read is not repeated in the pass");
}

const ISSUE: u32 = 40;

fn ready(issues: &[u32], held: Vec<HeldSweep>) -> FleetInput {
    use crate::observability::fleet_state::{ReadyItem, ReadyQueue};
    let mut with_ready = input(&[], held);
    with_ready.ready = Some(ReadyQueue {
        items: issues
            .iter()
            .map(|issue| ReadyItem {
                repo: REPO.to_string(),
                issue: *issue,
                rank: 1,
                star: false,
                star_at: None,
                level: 0,
                fleet_priority: 0,
                created_at: None,
                main_red_fix: false,
            })
            .collect(),
        listed: [REPO.to_string()].into(),
        complete: [REPO.to_string()].into(),
        slots: None,
    });
    with_ready
}

/// This host's own claim of `ISSUE`, in `sweep.curator` since `t0() + 3m`.
fn claimed() -> FleetInput {
    ready(
        &[],
        vec![HeldSweep {
            repo: REPO.to_string(),
            issue: ISSUE,
            stage: FleetStage::SweepCurator,
            entered_at: t0() + Duration::minutes(3),
            entered_at_lower_bound: false,
            pr: None,
            overflow: false,
            start: Default::default(),
        }],
    )
}

/// `ISSUE` ready since `t0() - 30m`.
fn ready_forge() -> Forge {
    let mut forge = Forge::default();
    forge.label(ISSUE, "loom:issue", t0() - Duration::minutes(30));
    forge
}

/// `ISSUE` claimed at `at`: `loom:issue` swapped for `loom:building`.
fn claim(forge: &mut Forge, at: DateTime<Utc>) {
    forge.unlabel(ISSUE, "loom:issue", at);
    forge.label(ISSUE, "loom:building", at);
}

fn assert_claimed_at(r: &StageOutcomeRecord, at: DateTime<Utc>) {
    assert_eq!((r.stage, r.exit), (FleetStage::ReadyWait, StageExit::Advance));
    assert_eq!(r.next_stage, Some(FleetStage::SweepCurator));
    assert_eq!((r.left_at, r.forge_transition_at), (at, Some(at)));
    assert_eq!(r.resolution_sec, Some(0));
    assert_eq!(r.entered_at, Some(t0() - Duration::minutes(30)));
    assert_eq!(r.entered_at_source, Some(EnteredAtSource::Forge));
    assert_eq!(r.dwell_sec, Some((at - (t0() - Duration::minutes(30))).num_seconds()));
}

/// The claiming host and a host that only sees the row vanish read the same
/// forge facts, so their records share the fact id's key.
#[test]
fn every_host_dates_a_claimed_ready_exit_alike() {
    let at = t0() + Duration::minutes(3);
    let mut forge = ready_forge();
    claim(&mut forge, at);
    let step = Duration::minutes(5);
    let claiming = run(&[ready(&[ISSUE], Vec::new()), claimed()], step, &mut forge);
    let other = run(&[ready(&[ISSUE], Vec::new()), ready(&[], Vec::new())], step, &mut forge);
    for out in [&claiming, &other] {
        let records = stages(&out[1]);
        assert_eq!(records.len(), 1, "{out:?}");
        assert_claimed_at(records[0], at);
    }
}

/// Mechanism B: the row flaps out of the listing while the issue is still
/// `loom:issue`. Nothing is emitted until the real claim, then exactly once.
#[test]
fn a_ready_row_that_flaps_out_and_back_is_not_an_exit() {
    let at = t0() + Duration::minutes(12);
    let mut forge = ready_forge();
    claim(&mut forge, at);
    let out = run(
        &[
            ready(&[ISSUE], Vec::new()),
            ready(&[], Vec::new()),
            ready(&[ISSUE], Vec::new()),
            ready(&[], Vec::new()),
            ready(&[], Vec::new()),
        ],
        Duration::minutes(5),
        &mut forge,
    );
    let all: Vec<_> = out.iter().flat_map(|o| stages(o)).collect();
    assert_eq!(all.len(), 1, "{out:?}");
    assert!(stages(&out[3]).len() == 1, "{out:?}");
    assert_claimed_at(all[0], at);
}

#[test]
fn a_ready_exit_waits_for_its_forge_departure() {
    let at = t0() + Duration::minutes(7);
    let mut forge = ready_forge();
    claim(&mut forge, at);
    let gone = ready(&[], Vec::new());
    let out = run(
        &[
            ready(&[ISSUE], Vec::new()),
            gone.clone(),
            gone.clone(),
            gone,
        ],
        Duration::minutes(5),
        &mut forge,
    );
    assert!(out[1].is_empty(), "no departure on the forge yet: {out:?}");
    assert_eq!(stages(&out[2]).len(), 1, "{out:?}");
    assert_claimed_at(stages(&out[2])[0], at);
    assert!(out[3].is_empty(), "emitted once: {out:?}");
}

#[test]
fn a_closed_ready_issue_is_cut_short_at_the_close() {
    let at = t0() + Duration::minutes(3);
    let mut forge = ready_forge();
    forge.event(ISSUE, at, HistoryEvent::Closed);
    let out = run(
        &[ready(&[ISSUE], Vec::new()), ready(&[], Vec::new())],
        Duration::minutes(5),
        &mut forge,
    );
    let r = stages(&out[1])[0];
    assert_eq!((r.exit, r.next_stage), (StageExit::CutShort, None));
    assert_eq!(r.forge_transition_at, Some(at));
    assert_eq!(r.dwell_sec, None);
    assert_eq!(r.entered_at, Some(t0() - Duration::minutes(30)));
}

#[test]
fn a_ready_exit_the_forge_never_shows_is_one_unknown_after_the_cap() {
    let mut forge = ready_forge();
    let mut inputs = vec![ready(&[ISSUE], Vec::new())];
    let cap = usize::try_from(PENDING_READY_MAX_PASSES).unwrap();
    inputs.extend(std::iter::repeat_n(ready(&[], Vec::new()), cap + 2));
    let out = run(&inputs, Duration::minutes(5), &mut forge);
    for (i, pass) in out.iter().enumerate() {
        assert_eq!(stages(pass).len(), usize::from(i == cap + 1), "pass {i}: {pass:?}");
    }
    let r = stages(&out[cap + 1])[0];
    assert_eq!((r.exit, r.next_stage), (StageExit::Unknown, None));
    assert_eq!(r.forge_transition_at, None, "no fact id");
    assert_eq!(r.left_at, t0() + Duration::minutes(5), "the pass that saw it gone");
    assert_eq!(r.entered_at, Some(t0() - Duration::minutes(30)));
}

#[test]
fn a_ready_row_of_an_unwhole_or_unmanaged_listing_is_not_an_exit() {
    let mut forge = ready_forge();
    claim(&mut forge, t0() + Duration::minutes(3));
    let mut partial = ready(&[], Vec::new());
    partial.ready.as_mut().unwrap().complete.clear();
    let mut unmanaged = ready(&[], Vec::new());
    unmanaged.managed.clear();
    for gone in [partial, unmanaged] {
        let out = run(&[ready(&[ISSUE], Vec::new()), gone], Duration::minutes(5), &mut forge);
        assert!(out.iter().all(Vec::is_empty), "{out:?}");
    }
    assert_eq!(forge.reads, 0);
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
        start: Default::default(),
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
    assert_eq!(r.entered_at_source, Some(EnteredAtSource::Checkpoint));
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
    assert_eq!(forge.pull_reads(), 2);
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
    forge.label(21, "loom:pr", t0() + Duration::minutes(1));
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
    // The release still reads the hold's own entry; the re-hold's exit and
    // entry share one read.
    assert_eq!(forge.reads, 3, "one events read per move");
}

/// No per-pass limit: every one of 40 review-label moves in one pass is
/// dated by its own label event, with one events read per move.
#[test]
fn more_than_thirty_stage_outcome_moves_in_one_pass_all_get_their_forge_instant() {
    let approved_at = t0() + Duration::minutes(3);
    let mut forge = Forge::default();
    let numbers: Vec<u32> = (0..40).map(|i| 100 + 2 * i).collect();
    for n in &numbers {
        forge.label(*n, "loom:pr", approved_at);
    }
    let listing = |label: &'static str| -> Vec<(u32, &'static str, u32)> {
        numbers.iter().map(|n| (*n, label, n - 1)).collect()
    };
    let out = run(
        &[
            input(&listing("loom:review-requested"), Vec::new()),
            input(&listing("loom:pr"), Vec::new()),
        ],
        Duration::minutes(5),
        &mut forge,
    );
    let records = stages(&out[1]);
    assert_eq!(records.len(), 40);
    assert!(
        records
            .iter()
            .all(|r| r.forge_transition_at == Some(approved_at)),
        "every move is forge-dated"
    );
    assert_eq!(forge.reads, 40, "one events read per actual move");
}

fn labeled(label: &str, at: DateTime<Utc>) -> serde_json::Value {
    serde_json::json!({
        "event": "labeled",
        "label": {"name": label},
        "created_at": at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    })
}

/// The events API lists oldest first, so the move this pass saw is on the
/// last page: every page is read, and the newest `labeled` event wins.
#[test]
fn a_label_event_beyond_page_one_dates_the_stage_outcome() {
    let old = t0() - Duration::days(30);
    let new = t0() + Duration::minutes(3);
    let mut first: Vec<serde_json::Value> = (0..EVENTS_PAGE_SIZE - 1)
        .map(|_| serde_json::json!({"event": "commented"}))
        .collect();
    first.push(labeled("loom:pr", old));
    let pages = [
        serde_json::Value::Array(first),
        serde_json::Value::Array(vec![
            labeled("loom:review-requested", old),
            labeled("loom:pr", new),
        ]),
    ];
    let mut read = Vec::new();
    let history = LabelHistory::read(|page| {
        read.push(page);
        pages.get(page - 1).cloned()
    })
    .unwrap();
    let approved = history.latest(t0() + Duration::hours(1), |e| {
        *e == HistoryEvent::Labeled("loom:pr".to_string())
    });
    assert_eq!(approved, Some(new), "the page-two event, not page one's");
    assert_eq!(read, [1, 2], "stops at the short page");
    assert_eq!(history.events.len(), 3, "only label events are kept");
}

/// `unlabeled` and `closed` events are kept with their instants, oldest first.
#[test]
fn the_history_keeps_unlabeled_and_closed_events_in_order() {
    let at = |m| t0() + Duration::minutes(m);
    let stamp = |m| at(m).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let page = serde_json::json!([
        {"event": "closed", "created_at": stamp(9)},
        {"event": "unlabeled", "label": {"name": "loom:issue"}, "created_at": stamp(5)},
        labeled("loom:issue", at(1)),
        {"event": "renamed", "created_at": stamp(2)},
    ]);
    let history = LabelHistory::read(|_| Some(page.clone())).unwrap();
    assert_eq!(
        history.events,
        vec![
            (at(1), HistoryEvent::Labeled("loom:issue".to_string())),
            (at(5), HistoryEvent::Unlabeled("loom:issue".to_string())),
            (at(9), HistoryEvent::Closed),
        ]
    );
}

#[test]
fn a_failed_events_page_is_a_failed_read() {
    let full = serde_json::Value::Array(
        (0..EVENTS_PAGE_SIZE)
            .map(|_| serde_json::json!({"event": "commented"}))
            .collect(),
    );
    assert!(LabelHistory::read(|page| (page == 1).then(|| full.clone())).is_none());
}
