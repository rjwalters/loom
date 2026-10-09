//! `fleet.state` building, anchor/delta decisions, stamps and routing (Issue
//! #10196). Everything here is pure: no ETA state, no forge.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex as StdMutex};

use chrono::{DateTime, Duration, TimeZone, Utc};

use super::{
    build_view, decide, held_stage, hold_kind, needs_anchor, planner_config_hash, pr_stage,
    Emitted, FleetInput, FleetStateSink, FleetView, HeldSweep, ListedPr, ReadyItem, ReadyQueue,
    RepoListing, Source,
};
use crate::observability::queue::QueueSink;
use crate::telemetry::kinds::fleet_state::{
    split_into_chunks, FleetCapacity, FleetHoldKind, FleetSlot, FleetSlots, FleetStage,
    FleetStateRecord, FleetStateRepo, FleetStateRow, MainCi, PlannerStamps, ANCHOR_INTERVAL_SECS,
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
            // Forced whole so the diffing path stays covered; the work
            // finder never reports this until #11139.
            complete: [REPO, OTHER].iter().map(|s| (*s).to_string()).collect(),
            slots: Some(FleetSlots {
                max_concurrent: 4,
                occupancy: Some(1),
            }),
        }),
        capacity: None,
        main_ci: BTreeMap::new(),
    }
}

/// `repo`'s entry in `record`.
fn entry(record: &FleetStateRecord, repo: &str) -> FleetStateRepo {
    record
        .repos
        .iter()
        .find(|r| r.repo == repo)
        .cloned()
        .unwrap()
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
    inp.ready.as_mut().unwrap().complete.remove(OTHER);
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

/// `ready_complete` is `true` only for a repo whose ready listing is known
/// whole (forced here; never so before #11139). A repo read but not known
/// whole is `false` and `ready_replace`; a flip alone is a change, and the
/// delta carries the repo's whole ready set.
#[test]
fn ready_complete_marks_whether_the_ready_listing_was_whole() {
    let view = build_view(&input(), None, t0());
    let anchor = decide(&view, &stamps(), None, t0()).unwrap();
    assert!(entry(&anchor, OTHER).ready_complete);
    assert!(!entry(&anchor, OTHER).ready_replace);
    assert!(entry(&anchor, REPO).ready_complete);

    let mut inp = input();
    inp.ready.as_mut().unwrap().complete.remove(OTHER);
    let later = t0() + Duration::minutes(5);
    let second = build_view(&inp, Some(&view), later);
    assert!(!second.ready_complete(OTHER));
    let last = emitted(view, t0(), t0());
    let delta = decide(&second, &stamps(), Some(&last), later).expect("the flip is sent");
    let other = entry(&delta, OTHER);
    assert!(!other.ready_complete && other.ready_replace);
    let issues: Vec<u32> = other.rows.iter().map(|r| r.issue).collect();
    assert_eq!(issues, vec![5, 6], "the whole ready set");
    assert!(other.removed.is_empty());
    assert!(!delta.repos.iter().any(|r| r.repo == REPO), "REPO did not change");

    // No tick at all: nothing is complete.
    let mut none = input();
    none.ready = None;
    let view = build_view(&none, None, t0());
    let anchor = decide(&view, &stamps(), None, t0()).unwrap();
    assert!(anchor
        .repos
        .iter()
        .all(|r| !r.ready_complete && r.ready_replace));
}

/// Rows a failed read could not see are never sent as `removed[]`: a review
/// walk that failed (no listing), or a ready listing that failed (not in
/// `listed`), keeps the earlier rows.
#[test]
fn an_incomplete_read_never_removes_what_it_could_not_see() {
    let first = build_view(&input(), None, t0());
    let last = emitted(first.clone(), t0(), t0());
    let mut inp = input();
    inp.listings.clear();
    inp.listed_at = None;
    let ready = inp.ready.as_mut().unwrap();
    ready.listed.remove(OTHER);
    ready.complete.remove(OTHER);
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

/// A host's state as a reader holds it: rows by repo, then issue.
type Replayed = BTreeMap<String, BTreeMap<u32, FleetStateRow>>;

/// The documented reader rule (`telemetry-replay.md`, step 3) over one
/// assembled record: an anchor resets the state; per repo entry, a
/// `ready_replace` entry first drops every `ready_wait` row of the repo
/// (once per record), then `removed` is dropped and `rows` upserted. A repo
/// left with no rows is dropped (the census is not tracked here).
fn replay(state: &mut Replayed, record: &FleetStateRecord) {
    if record.anchor {
        state.clear();
    }
    let mut replaced = BTreeSet::new();
    for entry in &record.repos {
        let rows = state.entry(entry.repo.clone()).or_default();
        if entry.ready_replace && replaced.insert(entry.repo.clone()) {
            rows.retain(|_, row| row.stage != FleetStage::ReadyWait);
        }
        for issue in &entry.removed {
            rows.remove(issue);
        }
        for row in &entry.rows {
            rows.insert(row.issue, row.clone());
        }
    }
    state.retain(|_, rows| !rows.is_empty());
}

/// The rows of `view`, as [`replay`] holds them.
fn rows_of(view: &FleetView) -> Replayed {
    view.repos
        .iter()
        .filter(|(_, state)| !state.rows.is_empty())
        .map(|(repo, state)| (repo.clone(), state.rows.clone()))
        .collect()
}

/// One pass of the replay scenario: OTHER's ready items and held sweeps,
/// whether OTHER's ready listing failed, and whether it is forced whole.
fn pass(ready_items: &[(u32, u32)], held_other: &[u32], failed: bool, whole: bool) -> FleetInput {
    let mut inp = input();
    inp.held.extend(
        held_other
            .iter()
            .map(|&i| held(OTHER, i, FleetStage::SweepCurator, false)),
    );
    let queue = inp.ready.as_mut().unwrap();
    queue.items = ready_items
        .iter()
        .map(|&(issue, rank)| ready(OTHER, issue, rank))
        .collect();
    if failed {
        queue.listed.remove(OTHER);
    }
    if failed || !whole {
        queue.complete.clear();
    }
    inp
}

/// Drive `passes` through `build_view` / `decide` / the chunker and the
/// reader rule. After every pass the reader's state equals the emitter's
/// view, and an incomplete repo's records never name a `ready_wait` row in
/// `removed`. Returns each pass's record (if any) and the reader's state.
fn drive(passes: &[FleetInput]) -> Vec<(Option<FleetStateRecord>, Replayed)> {
    let mut prev: Option<FleetView> = None;
    let mut last: Option<Emitted> = None;
    let mut state = Replayed::new();
    let mut out = Vec::new();
    for (i, inp) in passes.iter().enumerate() {
        let now = t0() + Duration::minutes(5 * i64::try_from(i).unwrap());
        let view = build_view(inp, prev.as_ref(), now);
        let record = decide(&view, &stamps(), last.as_ref(), now);
        if let Some(record) = &record {
            for entry in record.repos.iter().filter(|e| e.ready_replace) {
                let held = state.get(&entry.repo).cloned().unwrap_or_default();
                for issue in &entry.removed {
                    assert_ne!(
                        held.get(issue).map(|r| r.stage),
                        Some(FleetStage::ReadyWait),
                        "pass {i}: removed[] names ready_wait #{issue}"
                    );
                }
            }
            // Tiny chunks: the replace rule must hold across a split record.
            let chunks = split_into_chunks(record.clone(), 600);
            let mut assembled = chunks[0].clone();
            assembled.repos = chunks.into_iter().flat_map(|c| c.repos).collect();
            replay(&mut state, &assembled);
            last = Some(emitted(view.clone(), now, record.anchor_as_of));
        }
        assert_eq!(state, rows_of(&view), "pass {i}: reader diverged");
        prev = Some(view);
        out.push((record, state.clone()));
    }
    out
}

/// (a) An issue leaves an incomplete repo's ready queue: the next delta
/// names the repo with its whole remaining ready set and no `removed[]`
/// entry, and the reader drops the issue.
#[test]
fn an_issue_leaving_an_incomplete_ready_queue_is_replaced_away_not_removed() {
    let out = drive(&[
        pass(&[(5, 1), (6, 2)], &[], false, false),
        pass(&[(5, 1)], &[], false, false),
    ]);
    let delta = out[1].0.as_ref().expect("a changed ready set always sends");
    assert!(!delta.anchor);
    let other = entry(delta, OTHER);
    assert!(other.ready_replace && !other.ready_complete);
    assert_eq!(other.rows.iter().map(|r| r.issue).collect::<Vec<_>>(), vec![5]);
    assert!(other.removed.is_empty(), "{other:?}");
    assert!(!out[1].1[OTHER].contains_key(&6));
}

/// (b) Replayed by the reader rule, no `ready_wait` row of an incomplete
/// repo survives its absence from the observed set, through departures, a
/// handoff to a held sweep and back out, rank shifts and an emptied queue. A
/// failed listing (not an observation) keeps the earlier rows. The same
/// passes with the repo forced whole reconstruct exactly by plain diffing.
#[test]
fn no_ready_row_outlives_its_absence_from_the_observed_set() {
    type Step = (&'static [(u32, u32)], &'static [u32], bool);
    let steps: [Step; 8] = [
        (&[(5, 1), (6, 2), (7, 3)], &[], false),
        (&[(6, 1), (7, 2)], &[5], false),
        (&[(7, 1), (6, 2)], &[5], false),
        (&[(7, 1)], &[], false),
        (&[(7, 1)], &[], true),
        (&[], &[], false),
        (&[(8, 1), (9, 2)], &[], false),
        (&[(9, 1)], &[8], false),
    ];
    for whole in [false, true] {
        let passes: Vec<FleetInput> = steps
            .iter()
            .map(|(items, held, failed)| pass(items, held, *failed, whole))
            .collect();
        let out = drive(&passes);
        for (i, ((items, _, failed), (_, state))) in steps.iter().zip(&out).enumerate() {
            if *failed {
                continue;
            }
            let observed: BTreeSet<u32> = items.iter().map(|(issue, _)| *issue).collect();
            let replayed: BTreeSet<u32> = state
                .get(OTHER)
                .map(|rows| {
                    rows.values()
                        .filter(|r| r.stage == FleetStage::ReadyWait)
                        .map(|r| r.issue)
                        .collect()
                })
                .unwrap_or_default();
            assert_eq!(replayed, observed, "whole={whole} pass {i}");
        }
        // The failed pass kept #7; the next observation replaced it away.
        assert!(out[4].1[OTHER].contains_key(&7));
        assert!(!out[5]
            .1
            .get(OTHER)
            .is_some_and(|rows| rows.contains_key(&7)));
    }
}

fn labels(list: &[&str]) -> Vec<String> {
    list.iter().map(|l| (*l).to_string()).collect()
}

/// An input whose only PR (21, issue 20) carries `pr_labels`.
fn with_pr_labels(pr_labels: &[&str]) -> FleetInput {
    let mut i = input();
    i.held.clear();
    i.listings[0].prs = vec![ListedPr {
        number: 21,
        labels: labels(pr_labels),
        issue: Some(20),
    }];
    i
}

#[test]
fn hold_kind_wire_strings_are_pinned() {
    let wire: Vec<&str> = FleetHoldKind::ALL.iter().map(|k| k.as_str()).collect();
    assert_eq!(
        wire,
        [
            "operator",
            "operator_only",
            "operator_decision",
            "merge_risk",
            "critical_file",
            "ac_hold",
            "blocked",
            "other"
        ]
    );
    for kind in FleetHoldKind::ALL {
        assert_eq!(serde_json::to_value(kind).unwrap(), kind.as_str());
    }
}

#[test]
fn hold_kind_comes_from_the_labels() {
    assert_eq!(hold_kind(&labels(&["loom:pr"])), None);
    assert_eq!(hold_kind(&labels(&["loom:pr", "loom:operator"])), Some(FleetHoldKind::Operator));
    assert_eq!(
        hold_kind(&labels(&["loom:operator", "loom:operator-only"])),
        Some(FleetHoldKind::OperatorOnly)
    );
    assert_eq!(
        hold_kind(&labels(&["loom:operator-decision", "loom:operator"])),
        Some(FleetHoldKind::OperatorDecision)
    );
    assert_eq!(hold_kind(&labels(&["loom:blocked"])), Some(FleetHoldKind::Blocked));
}

#[test]
fn hold_appears_then_releases_across_two_deltas() {
    let stamps = stamps();
    let t1 = t0() + Duration::minutes(5);
    let t2 = t0() + Duration::minutes(10);
    let t3 = t0() + Duration::minutes(15);
    let v0 = build_view(&with_pr_labels(&["loom:pr"]), None, t0());
    let anchor = decide(&v0, &stamps, None, t0()).unwrap();
    assert!(anchor.anchor);
    let e0 = emitted(v0.clone(), t0(), t0());

    // The hold appears.
    let v1 = build_view(&with_pr_labels(&["loom:pr", "loom:operator"]), Some(&v0), t1);
    let d1 = decide(&v1, &stamps, Some(&e0), t1).unwrap();
    let row = &entry(&d1, REPO).rows[0];
    assert_eq!(row.hold_kind, Some(FleetHoldKind::Operator));
    assert_eq!(row.held_since, Some(t1));
    assert!(!row.held_since_lower_bound);
    assert_eq!(row.hold_released_at, None);
    let e1 = emitted(v1.clone(), t1, t0());

    // Still held: no change, no delta (and the start is kept).
    let v2 = build_view(&with_pr_labels(&["loom:pr", "loom:operator"]), Some(&v1), t2);
    assert_eq!(v2.repos[REPO].rows[&20].held_since, Some(t1));
    assert!(decide(&v2, &stamps, Some(&e1), t2).is_none_or(|d| d.repos.is_empty()));

    // It clears: the clearing delta says when, and the hold fields are gone.
    let v3 = build_view(&with_pr_labels(&["loom:pr"]), Some(&v2), t3);
    let d3 = decide(&v3, &stamps, Some(&emitted(v2, t2, t0())), t3).unwrap();
    let row = &entry(&d3, REPO).rows[0];
    assert_eq!(row.hold_kind, None);
    assert_eq!(row.held_since, None);
    assert_eq!(row.hold_released_at, Some(t3));
    // The stamp is kept, so the next pass is quiet.
    let v4 = build_view(&with_pr_labels(&["loom:pr"]), Some(&v3), t3 + Duration::minutes(5));
    assert_eq!(v4.repos[REPO].rows[&20].hold_released_at, Some(t3));
    let e3 = emitted(v3, t3, t0());
    assert!(decide(&v4, &stamps, Some(&e3), t3 + Duration::minutes(5)).is_none());
}

#[test]
fn a_hold_first_seen_without_a_prior_listing_is_a_lower_bound() {
    let v = build_view(&with_pr_labels(&["loom:pr", "loom:blocked"]), None, t0());
    let row = &v.repos[REPO].rows[&20];
    assert_eq!(row.hold_kind, Some(FleetHoldKind::Blocked));
    assert!(row.held_since_lower_bound);
}

fn capacity(live: u32) -> FleetCapacity {
    FleetCapacity {
        live_workers: live,
        accounts_usable: Some(3),
        accounts_exhausted: Some(1),
        host_breaker: Some("closed".to_string()),
        rate_limit_breaker: Some("closed".to_string()),
        admission_brake_held: Some(false),
    }
}

#[test]
fn capacity_change_produces_delta() {
    let stamps = stamps();
    let t1 = t0() + Duration::minutes(5);
    let mut i = input();
    i.capacity = Some(capacity(2));
    let v0 = build_view(&i, None, t0());
    let e0 = emitted(v0.clone(), t0(), t0());
    assert!(decide(&v0, &stamps, None, t0()).unwrap().capacity == Some(capacity(2)));

    // Otherwise unchanged, capacity unchanged: nothing to send.
    let v1 = build_view(&i, Some(&v0), t1);
    assert!(decide(&v1, &stamps, Some(&e0), t1).is_none());

    // Only `live_workers` changed: a delta carrying the capacity.
    i.capacity = Some(capacity(3));
    let v2 = build_view(&i, Some(&v0), t1);
    let delta = decide(&v2, &stamps, Some(&e0), t1).unwrap();
    assert!(!delta.anchor);
    assert!(delta.repos.is_empty());
    assert_eq!(delta.capacity, Some(capacity(3)));
}

#[test]
fn main_ci_rides_each_repo_entry_and_a_change_is_a_delta() {
    let stamps = stamps();
    let t1 = t0() + Duration::minutes(5);
    let mut i = input();
    i.main_ci.insert(REPO.to_string(), MainCi::Green);
    let v0 = build_view(&i, None, t0());
    let anchor = decide(&v0, &stamps, None, t0()).unwrap();
    assert_eq!(entry(&anchor, REPO).main_ci, Some(MainCi::Green));
    assert_eq!(entry(&anchor, OTHER).main_ci, None);
    let e0 = emitted(v0.clone(), t0(), t0());
    i.main_ci.insert(REPO.to_string(), MainCi::Red);
    let v1 = build_view(&i, Some(&v0), t1);
    let delta = decide(&v1, &stamps, Some(&e0), t1).unwrap();
    assert_eq!(delta.repos.len(), 1);
    assert_eq!(delta.repos[0].main_ci, Some(MainCi::Red));
}

#[test]
fn fleet_state_v1_old_record_decodes() {
    let old = serde_json::json!({
        "schema": "fleet-state/v1",
        "as_of": "2026-10-04T12:00:00Z",
        "anchor": true,
        "anchor_as_of": "2026-10-04T12:00:00Z",
        "repos": [{"repo": "a/b", "rows": [{
            "issue": 1, "stage": "review_wait", "entered_at": "2026-10-04T11:00:00Z"
        }]}]
    });
    let record: FleetStateRecord = serde_json::from_value(old).unwrap();
    assert_eq!(record.capacity, None);
    assert_eq!(record.repos[0].main_ci, None);
    let row = &record.repos[0].rows[0];
    assert_eq!((row.hold_kind, row.held_since, row.hold_released_at), (None, None, None));
    // And the new fields stay off the wire when unset.
    let text = serde_json::to_string(&record).unwrap();
    assert!(!text.contains("hold_") && !text.contains("capacity") && !text.contains("main_ci"));
}
