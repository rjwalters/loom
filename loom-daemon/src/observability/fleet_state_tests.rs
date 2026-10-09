//! `fleet.state` building, anchor/delta decisions, stamps and routing (Issue
//! #10196). Everything here is pure: no ETA state, no forge.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex as StdMutex};

use chrono::{DateTime, Duration, TimeZone, Utc};

use super::{
    build_view, decide, held_stage, needs_anchor, planner_config_hash, pr_stage, Emitted,
    FleetInput, FleetStateSink, FleetView, HeldSweep, ListedPr, ReadyItem, ReadyQueue, RepoListing,
    Source,
};
use crate::observability::queue::QueueSink;
use crate::telemetry::kinds::fleet_state::{
    split_into_chunks, FleetSlot, FleetSlots, FleetStage, PlannerStamps, ANCHOR_INTERVAL_SECS,
    CHUNK_BYTES,
};
use crate::telemetry::{TelemetryEnvelope, TelemetryRecord};

const REPO: &str = "rjwalters/loom";
const OTHER: &str = "rjwalters/anvil";

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 4, 12, 0, 0).unwrap()
}

fn stamps() -> PlannerStamps {
    PlannerStamps {
        planner_version: "0.19.958".to_string(),
        planner_config_hash: "0123456789ab".to_string(),
        fleet_config_hash: None,
    }
}

fn held(repo: &str, issue: u32, stage: FleetStage, overflow: bool) -> HeldSweep {
    HeldSweep {
        repo: repo.to_string(),
        issue,
        stage,
        entered_at: t0() - Duration::minutes(30),
        entered_at_lower_bound: false,
        pr: None,
        overflow,
    }
}

fn pr(number: u32, label: &str, body_issue: Option<u32>) -> ListedPr {
    ListedPr {
        number,
        labels: vec![label.to_string()],
        issue: body_issue,
    }
}

fn ready(repo: &str, issue: u32, rank: u32) -> ReadyItem {
    ReadyItem {
        repo: repo.to_string(),
        issue,
        rank,
        star: rank == 1,
        star_at: (rank == 1).then(|| t0() - Duration::hours(2)),
        level: u8::from(rank == 1),
        fleet_priority: 100,
        created_at: Some(t0() - Duration::days(3)),
        main_red_fix: false,
    }
}

fn input() -> FleetInput {
    FleetInput {
        host_id: "robb-studio".to_string(),
        managed: [REPO, OTHER].iter().map(|s| (*s).to_string()).collect(),
        held: vec![held(REPO, 10, FleetStage::SweepBuilder, false)],
        listings: vec![RepoListing {
            repo: REPO.to_string(),
            prs: vec![
                pr(21, "loom:review-requested", Some(20)),
                pr(31, "loom:pr", None),
            ],
        }],
        listed_at: Some(t0() - Duration::minutes(1)),
        ready: Some(ReadyQueue {
            items: vec![ready(OTHER, 5, 1), ready(OTHER, 6, 2)],
            listed: [REPO, OTHER].iter().map(|s| (*s).to_string()).collect(),
            slots: Some(FleetSlots {
                max_concurrent: 4,
                occupancy: Some(1),
            }),
        }),
    }
}

fn emitted(view: FleetView, as_of: DateTime<Utc>, anchor_as_of: DateTime<Utc>) -> Emitted {
    Emitted {
        view,
        stamps: stamps(),
        as_of,
        anchor_as_of,
    }
}

#[test]
fn rows_come_from_the_registry_the_review_listings_and_the_ready_queue() {
    let view = build_view(&input(), None, t0());
    let rows = &view.repos[REPO].rows;
    // Held: this host and its slot.
    assert_eq!(rows[&10].host.as_deref(), Some("robb-studio"));
    assert_eq!(rows[&10].slot, Some(FleetSlot::Regular));
    assert_eq!(rows[&10].stage, FleetStage::SweepBuilder);
    assert_eq!(rows[&10].entered_at, t0() - Duration::minutes(30));
    // Listed: no host, no slot, keyed by the linked issue.
    assert_eq!(rows[&20].pr, Some(21));
    assert_eq!(rows[&20].stage, FleetStage::ReviewWait);
    assert_eq!((rows[&20].host.as_deref(), rows[&20].slot), (None, None));
    // The census counts every listed PR, the unlinked one included.
    let census = view.repos[REPO].census.as_ref().unwrap();
    assert_eq!(census.open, 2);
    assert_eq!(census.by_stage["review_wait"], 1);
    assert_eq!(census.by_stage["merge_wait"], 1);
    // Ready: rank and the planner's inputs, from the tick.
    let five = &view.repos[OTHER].rows[&5];
    assert_eq!(five.stage, FleetStage::ReadyWait);
    assert_eq!(five.rank, Some(1));
    assert!(five.star);
    assert_eq!(five.level, 1);
    assert_eq!(five.fleet_priority, Some(100));
    assert_eq!(five.created_at, Some(t0() - Duration::days(3)));
    assert_eq!(view.repos[OTHER].rows[&6].rank, Some(2));
    // A repo not listed this pass has an unknown census.
    assert_eq!(view.repos[OTHER].census, None);
    assert_eq!(view.slots.unwrap().max_concurrent, 4);
    assert_eq!(view.census_at, Some(t0() - Duration::minutes(1)));
}

#[test]
fn the_overflow_sweep_holds_the_overflow_slot() {
    let mut inp = input();
    inp.held = vec![held(REPO, 10, FleetStage::SweepCurator, true)];
    let view = build_view(&inp, None, t0());
    assert_eq!(view.repos[REPO].rows[&10].slot, Some(FleetSlot::Overflow));
}

#[test]
fn a_held_row_wins_over_a_listing_and_a_listing_over_the_queue() {
    let mut inp = input();
    inp.listings[0]
        .prs
        .push(pr(11, "loom:review-requested", Some(10)));
    inp.ready.as_mut().unwrap().items.push(ready(REPO, 20, 9));
    let view = build_view(&inp, None, t0());
    let rows = &view.repos[REPO].rows;
    assert_eq!(rows[&10].stage, FleetStage::SweepBuilder);
    assert_eq!(view.repos[REPO].sources[&10], Source::Held);
    assert_eq!(rows[&20].stage, FleetStage::ReviewWait);
    assert_eq!(rows[&20].rank, None);
}

#[test]
fn two_prs_for_one_issue_keep_the_lowest_pr() {
    let mut inp = input();
    inp.listings[0].prs = vec![
        pr(70, "loom:pr", Some(60)),
        pr(65, "loom:review-requested", Some(60)),
    ];
    let view = build_view(&inp, None, t0());
    assert_eq!(view.repos[REPO].rows[&60].pr, Some(65));
}

#[test]
fn first_sight_is_a_lower_bound_and_a_later_arrival_is_exact() {
    let first = build_view(&input(), None, t0());
    assert!(first.repos[REPO].rows[&20].entered_at_lower_bound);
    assert!(first.repos[OTHER].rows[&5].entered_at_lower_bound);
    // The host-held row's entry comes from the checkpoint, not first sight.
    assert!(!first.repos[REPO].rows[&10].entered_at_lower_bound);

    let later = t0() + Duration::minutes(5);
    let mut inp = input();
    inp.ready.as_mut().unwrap().items.push(ready(OTHER, 7, 3));
    let second = build_view(&inp, Some(&first), later);
    // Unchanged rows keep their entry.
    assert_eq!(second.repos[OTHER].rows[&5].entered_at, t0());
    assert!(second.repos[OTHER].rows[&5].entered_at_lower_bound);
    // A new row in a listing complete last pass entered now, exactly.
    let seven = &second.repos[OTHER].rows[&7];
    assert_eq!(seven.entered_at, later);
    assert!(!seven.entered_at_lower_bound);
}

#[test]
fn a_stage_change_is_a_new_entry_and_a_handoff_keeps_it() {
    let first = build_view(&input(), None, t0());
    let later = t0() + Duration::minutes(5);
    // #20's PR is approved: a new stage, observed this pass.
    let mut inp = input();
    inp.listings[0].prs[0] = pr(21, "loom:pr", Some(20));
    // #10's sweep exits after building; its PR is now listed in review.
    inp.held.clear();
    inp.listings[0]
        .prs
        .push(pr(11, "loom:review-requested", Some(10)));
    let mut prev = first.clone();
    prev.repos
        .get_mut(REPO)
        .unwrap()
        .rows
        .get_mut(&10)
        .unwrap()
        .stage = FleetStage::ReviewWait;
    let second = build_view(&inp, Some(&prev), later);
    let rows = &second.repos[REPO].rows;
    assert_eq!(rows[&20].stage, FleetStage::MergeWait);
    assert_eq!(rows[&20].entered_at, later);
    assert!(!rows[&20].entered_at_lower_bound);
    // Same stage across the held-to-listed handoff: the entry carries over.
    assert_eq!(rows[&10].entered_at, t0() - Duration::minutes(30));
    assert_eq!(rows[&10].host, None);
}

#[test]
fn a_failed_listing_carries_the_last_rows_and_drops_the_census() {
    let first = build_view(&input(), None, t0());
    let mut inp = input();
    inp.listings.clear();
    inp.listed_at = None;
    inp.ready.as_mut().unwrap().listed.remove(OTHER);
    inp.ready.as_mut().unwrap().items.clear();
    let second = build_view(&inp, Some(&first), t0() + Duration::minutes(5));
    assert_eq!(second.repos[REPO].rows[&20], first.repos[REPO].rows[&20]);
    assert_eq!(second.repos[REPO].census, None, "unknown, not zero");
    assert_eq!(second.repos[OTHER].rows[&5], first.repos[OTHER].rows[&5]);
    assert_eq!(second.census_at, None);
    // A repo no longer managed is not carried.
    inp.managed.remove(OTHER);
    let third = build_view(&inp, Some(&first), t0() + Duration::minutes(5));
    assert!(!third.repos.contains_key(OTHER));
}

#[test]
fn ready_rows_need_no_eta() {
    // Only the work finder's tick: no listings, no sweeps.
    let mut inp = input();
    inp.held.clear();
    inp.listings.clear();
    let view = build_view(&inp, None, t0());
    let record = decide(&view, &stamps(), None, t0()).unwrap();
    assert_eq!(record.row_count(), 2);
}

#[test]
fn pr_stages_follow_the_review_labels() {
    let labels = |ls: &[&str]| ls.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
    assert_eq!(pr_stage(&labels(&["loom:review-requested"])), Some(FleetStage::ReviewWait));
    assert_eq!(pr_stage(&labels(&["loom:changes-requested"])), Some(FleetStage::Doctor));
    assert_eq!(pr_stage(&labels(&["loom:treating"])), Some(FleetStage::Doctor));
    assert_eq!(pr_stage(&labels(&["loom:pr"])), Some(FleetStage::MergeWait));
    assert_eq!(pr_stage(&labels(&["loom:pr", "loom:operator"])), Some(FleetStage::MergeHold));
    assert_eq!(pr_stage(&labels(&["loom:pr", "loom:review-requested"])), None);
}

#[test]
fn a_sweep_stage_comes_from_its_checkpoint() {
    let start = t0();
    let at = t0() + Duration::minutes(9);
    assert_eq!(held_stage(None, None, start), (FleetStage::SweepCurator, start, false));
    assert_eq!(
        held_stage(Some("curator-done"), Some(at), start),
        (FleetStage::SweepBuilder, at, false)
    );
    assert_eq!(held_stage(Some("builder-done"), Some(at), start).0, FleetStage::ReviewWait);
    assert_eq!(held_stage(Some("doctor-done"), Some(at), start).0, FleetStage::ReviewWait);
    assert_eq!(held_stage(Some("judge-rejected"), Some(at), start).0, FleetStage::Doctor);
    assert_eq!(held_stage(Some("judge-done"), Some(at), start).0, FleetStage::MergeWait);
    // No timestamp: the start is only a lower bound.
    assert_eq!(
        held_stage(Some("judge-done"), None, start),
        (FleetStage::MergeWait, start, true)
    );
}

#[test]
fn the_first_pass_is_a_full_anchor_with_stamps() {
    let view = build_view(&input(), None, t0());
    let record = decide(&view, &stamps(), None, t0()).expect("first pass always sends");
    assert!(record.anchor);
    assert_eq!(record.anchor_as_of, t0());
    assert_eq!(record.prev_as_of, None);
    assert_eq!(record.row_count(), 4);
    assert_eq!(record.repos.len(), 2);
    assert_eq!(record.stamps, stamps());
    assert_eq!((record.chunk_index, record.chunk_count), (0, 1));
}

#[test]
fn an_empty_host_still_sends_an_empty_anchor() {
    let view = build_view(&FleetInput::default(), None, t0());
    let record = decide(&view, &stamps(), None, t0()).unwrap();
    assert!(record.anchor);
    assert!(record.repos.is_empty());
}

#[test]
fn an_unchanged_pass_sends_nothing() {
    let view = build_view(&input(), None, t0());
    let last = emitted(view.clone(), t0(), t0());
    // `census_at` moves every pass; that alone is not a change.
    let mut next = view;
    next.census_at = Some(t0() + Duration::minutes(4));
    assert_eq!(decide(&next, &stamps(), Some(&last), t0() + Duration::minutes(5)), None);
}

#[test]
fn a_delta_carries_changed_rows_removals_and_the_full_census() {
    let first = build_view(&input(), None, t0());
    let last = emitted(first.clone(), t0(), t0());
    let mut inp = input();
    inp.held.clear();
    inp.ready.as_mut().unwrap().items.retain(|i| i.issue != 6);
    let later = t0() + Duration::minutes(5);
    let second = build_view(&inp, Some(&first), later);
    let record = decide(&second, &stamps(), Some(&last), later).unwrap();
    assert!(!record.anchor);
    assert_eq!(record.anchor_as_of, t0());
    assert_eq!(record.prev_as_of, Some(t0()));
    let loom = record.repos.iter().find(|r| r.repo == REPO).unwrap();
    assert_eq!(loom.removed, vec![10]);
    assert!(loom.census.is_some());
    let other = record.repos.iter().find(|r| r.repo == OTHER).unwrap();
    assert_eq!(other.removed, vec![6]);
    assert!(other.rows.is_empty());
}

#[test]
fn a_rank_shift_is_a_change() {
    let first = build_view(&input(), None, t0());
    let last = emitted(first.clone(), t0(), t0());
    let mut inp = input();
    inp.ready.as_mut().unwrap().items[1].rank = 1;
    inp.ready.as_mut().unwrap().items[0].rank = 2;
    let later = t0() + Duration::minutes(5);
    let record =
        decide(&build_view(&inp, Some(&first), later), &stamps(), Some(&last), later).unwrap();
    assert_eq!(record.row_count(), 2);
}

#[test]
fn a_slot_change_alone_sends_a_delta_with_no_repos() {
    let first = build_view(&input(), None, t0());
    let last = emitted(first.clone(), t0(), t0());
    let mut next = first;
    next.slots = Some(FleetSlots {
        max_concurrent: 4,
        occupancy: Some(2),
    });
    let record = decide(&next, &stamps(), Some(&last), t0() + Duration::minutes(5)).unwrap();
    assert!(!record.anchor && record.repos.is_empty());
}

#[test]
fn the_anchor_repeats_hourly_and_on_a_regime_change() {
    let view = build_view(&input(), None, t0());
    let last = emitted(view.clone(), t0(), t0());
    let at = |s: i64| t0() + Duration::seconds(s);
    assert!(!needs_anchor(Some(&last), &stamps(), at(ANCHOR_INTERVAL_SECS - 1)));
    assert!(needs_anchor(Some(&last), &stamps(), at(ANCHOR_INTERVAL_SECS)));
    let record = decide(&view, &stamps(), Some(&last), at(ANCHOR_INTERVAL_SECS)).unwrap();
    assert!(record.anchor);
    // A planner config change is a regime boundary: a fresh anchor at once.
    let mut changed = stamps();
    changed.planner_config_hash = "ffffffffffff".to_string();
    assert!(needs_anchor(Some(&last), &changed, at(60)));
    let record = decide(&view, &changed, Some(&last), at(60)).unwrap();
    assert!(record.anchor);
    assert_eq!(record.stamps, changed);
    // So is a fleet config change.
    let mut fleet = stamps();
    fleet.fleet_config_hash = Some("abc".to_string());
    assert!(needs_anchor(Some(&last), &fleet, at(60)));
}

#[test]
fn the_planner_config_hash_covers_only_the_planner_blocks() {
    let base = serde_json::json!({
        "autonomous": {"workFinder": {"maxConcurrent": 4, "intervalSecs": 60}},
        "terminals": []
    });
    let h = planner_config_hash(&base);
    assert_eq!(h.len(), 12);
    // Key order and unrelated config do not matter.
    let reordered = serde_json::json!({
        "terminals": [{"role": "builder"}],
        "autonomous": {"eta": {"enabled": true},
                       "workFinder": {"intervalSecs": 60, "maxConcurrent": 4}}
    });
    assert_eq!(planner_config_hash(&reordered), h);
    // A planner knob does.
    let capped = serde_json::json!({
        "autonomous": {"workFinder": {"maxConcurrent": 5, "intervalSecs": 60}}
    });
    assert_ne!(planner_config_hash(&capped), h);
    let sequenced = serde_json::json!({
        "autonomous": {"workFinder": {"maxConcurrent": 4, "intervalSecs": 60},
                       "mergeSequencing": {"enabled": true}}
    });
    assert_ne!(planner_config_hash(&sequenced), h);
}

/// Today's queue (~3000 ready rows) goes out as ONE record; the emitter has
/// no row cap.
#[test]
fn a_3000_row_queue_is_one_anchor_record() {
    let mut inp = input();
    inp.ready.as_mut().unwrap().items = (1..=3000).map(|n| ready(OTHER, 1000 + n, n)).collect();
    let view = build_view(&inp, None, t0());
    let record = decide(&view, &stamps(), None, t0()).unwrap();
    assert_eq!(record.row_count(), 3002);
    let chunks = split_into_chunks(record, CHUNK_BYTES);
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].row_count(), 3002);
}

#[derive(Default)]
struct CapturingQueue {
    offered: StdMutex<Vec<TelemetryEnvelope>>,
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

#[test]
fn the_sink_stamps_the_host_and_wraps_the_record() {
    let queue = Arc::new(CapturingQueue::default());
    let sink = FleetStateSink::new(queue.clone(), "robb-studio");
    let view = build_view(&input(), None, t0());
    sink.push(decide(&view, &stamps(), None, t0()).unwrap());
    let offered = queue.offered.lock().unwrap();
    assert_eq!(offered.len(), 1);
    assert_eq!(offered[0].host_id, "robb-studio");
    assert!(matches!(offered[0].record, TelemetryRecord::FleetState(_)));
}

#[test]
fn observed_sources_are_recorded_per_repo() {
    let view = build_view(&input(), None, t0());
    let review: &BTreeSet<String> = &view.observed[&Source::Review];
    assert!(review.contains(REPO) && !review.contains(OTHER));
    assert!(view.observed[&Source::Ready].contains(OTHER));
}

/// `ready_complete` is `true` for a repo whose ready listing the tick
/// completed, and `false` for one it could not (failed, or possibly cut at
/// one forge page and so left out of `listed`). A flip alone is a change.
#[test]
fn ready_complete_marks_whether_the_ready_listing_was_whole() {
    let view = build_view(&input(), None, t0());
    let anchor = decide(&view, &stamps(), None, t0()).unwrap();
    let entry = |record: &crate::telemetry::kinds::fleet_state::FleetStateRecord, repo: &str| {
        record
            .repos
            .iter()
            .find(|r| r.repo == repo)
            .cloned()
            .unwrap()
    };
    assert!(entry(&anchor, OTHER).ready_complete);
    assert!(entry(&anchor, REPO).ready_complete);

    // OTHER's listing may have been truncated: same rows, not complete.
    let mut inp = input();
    inp.ready.as_mut().unwrap().listed.remove(OTHER);
    let later = t0() + Duration::minutes(5);
    let second = build_view(&inp, Some(&view), later);
    assert!(!second.ready_complete(OTHER));
    let last = emitted(view, t0(), t0());
    let delta = decide(&second, &stamps(), Some(&last), later).expect("the flip is sent");
    let other = entry(&delta, OTHER);
    assert!(!other.ready_complete);
    assert!(other.rows.is_empty() && other.removed.is_empty());
    assert!(!delta.repos.iter().any(|r| r.repo == REPO), "REPO did not change");

    // No tick at all: nothing is complete.
    let mut none = input();
    none.ready = None;
    let view = build_view(&none, None, t0());
    let anchor = decide(&view, &stamps(), None, t0()).unwrap();
    assert!(anchor.repos.iter().all(|r| !r.ready_complete));
}

/// Rows an incomplete read could not see are never sent as `removed[]`: a
/// review walk that failed (no listing), or a ready listing possibly cut at
/// one page (not in `listed`) keeps the earlier rows.
#[test]
fn an_incomplete_read_never_removes_what_it_could_not_see() {
    let first = build_view(&input(), None, t0());
    let last = emitted(first.clone(), t0(), t0());
    let mut inp = input();
    inp.listings.clear();
    inp.listed_at = None;
    let ready = inp.ready.as_mut().unwrap();
    ready.listed.remove(OTHER);
    ready.items.retain(|i| i.issue != 6);
    let later = t0() + Duration::minutes(5);
    let second = build_view(&inp, Some(&first), later);
    let record = decide(&second, &stamps(), Some(&last), later).unwrap();
    assert_eq!(record.removed_count(), 0, "{record:?}");
    assert_eq!(second.repos[REPO].rows[&20], first.repos[REPO].rows[&20]);
    assert_eq!(second.repos[OTHER].rows[&6], first.repos[OTHER].rows[&6]);
}

/// `fleet.state` never reads the ETA subsystem, so it is emitted whatever
/// `autonomous.eta.enabled` says. A source-level guard keeps a later edit
/// from re-coupling it.
#[test]
fn fleet_state_does_not_depend_on_eta() {
    let sources = [
        include_str!("fleet_state.rs"),
        include_str!("fleet_state/sources.rs"),
        include_str!("../telemetry/kinds/fleet_state.rs"),
        include_str!("otlp/mapping/fleet_state.rs"),
    ];
    let needles = [
        concat!("crate::", "eta"),
        concat!("super::", "eta"),
        "eta.enabled",
    ];
    for (i, source) in sources.iter().enumerate() {
        for needle in needles {
            assert!(!source.contains(needle), "source #{i} names {needle}");
        }
    }
}
