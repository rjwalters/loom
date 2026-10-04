//! `fleet.state` building, anchor/delta decisions and routing (Issue #10196).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex as StdMutex};

use chrono::{DateTime, Duration, TimeZone, Utc};

use super::{build_view, decide, needs_anchor, Emitted, FleetInput, FleetStateSink};
use crate::eta::tracker::{LiveItem, RepoPrCensus};
use crate::eta::Stage;
use crate::observability::queue::QueueSink;
use crate::telemetry::kinds::fleet_state::{FleetSlot, ANCHOR_INTERVAL_SECS, MAX_ROWS};
use crate::telemetry::{TelemetryEnvelope, TelemetryRecord};

const REPO: &str = "rjwalters/loom";
const OTHER: &str = "rjwalters/anvil";

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 4, 12, 0, 0).unwrap()
}

fn item(repo: &str, issue: u32, stage: Stage, sweep: Option<&str>) -> LiveItem {
    LiveItem {
        repo: repo.to_string(),
        issue,
        stage,
        entered_at: t0() - Duration::minutes(30),
        entered_at_lower_bound: false,
        pr: (stage != Stage::ReadyWait).then_some(issue + 1),
        running_sweep_id: sweep.map(str::to_string),
    }
}

fn census(repo: &str, open: u32) -> RepoPrCensus {
    RepoPrCensus {
        repo: repo.to_string(),
        open,
        by_stage: [("review_wait".to_string(), open)].into_iter().collect(),
    }
}

fn input(items: Vec<LiveItem>, census_rows: Vec<RepoPrCensus>) -> FleetInput {
    FleetInput {
        host_id: "robb-studio".to_string(),
        items,
        census: Some((t0() - Duration::minutes(1), census_rows)),
        slots: Some((4, Some(1))),
    }
}

fn base() -> FleetInput {
    input(
        vec![
            item(REPO, 10, Stage::SweepBuilder, Some("sweep-issue-10-1")),
            item(REPO, 20, Stage::ReviewWait, None),
            item(OTHER, 5, Stage::ReadyWait, None),
        ],
        vec![census(REPO, 3)],
    )
}

fn emitted(input: &FleetInput, as_of: DateTime<Utc>, anchor_as_of: DateTime<Utc>) -> Emitted {
    Emitted {
        view: build_view(input, &BTreeSet::new()),
        as_of,
        anchor_as_of,
    }
}

#[test]
fn a_running_sweep_row_names_this_host_and_its_slot() {
    let view = build_view(&base(), &BTreeSet::new());
    let rows = &view.repos[REPO].rows;
    assert_eq!(rows[&10].host.as_deref(), Some("robb-studio"));
    assert_eq!(rows[&10].slot, Some(FleetSlot::Regular));
    assert_eq!(rows[&10].pr, Some(11));
    assert_eq!(rows[&10].stage, Stage::SweepBuilder);
    // Seen only through a review listing: no host, no slot.
    assert_eq!(rows[&20].host, None);
    assert_eq!(rows[&20].slot, None);
    assert_eq!(view.repos[REPO].census.as_ref().unwrap().open, 3);
    // A repo with rows but no completed listing has an unknown census.
    assert_eq!(view.repos[OTHER].census, None);
    assert_eq!(view.slots.unwrap().max_concurrent, 4);
}

#[test]
fn the_overflow_sweep_holds_the_overflow_slot() {
    let overflow: BTreeSet<String> = ["sweep-issue-10-1".to_string()].into_iter().collect();
    let view = build_view(&base(), &overflow);
    assert_eq!(view.repos[REPO].rows[&10].slot, Some(FleetSlot::Overflow));
}

#[test]
fn the_cap_keeps_host_held_rows_and_drops_ready_rows_first() {
    let mut items: Vec<LiveItem> = (0..MAX_ROWS as u32)
        .map(|n| item(OTHER, n, Stage::ReadyWait, None))
        .collect();
    items.push(item(REPO, 9_000, Stage::ReviewWait, None));
    items.push(item(REPO, 9_001, Stage::SweepCurator, Some("s")));
    let view = build_view(&input(items, Vec::new()), &BTreeSet::new());
    assert_eq!(view.rows_truncated, 2);
    assert!(view.repos[REPO].rows.contains_key(&9_000));
    assert!(view.repos[REPO].rows.contains_key(&9_001));
    assert_eq!(view.repos[OTHER].rows.len(), MAX_ROWS - 2);
}

#[test]
fn the_first_pass_is_a_full_anchor() {
    let view = build_view(&base(), &BTreeSet::new());
    let record = decide(&view, None, t0()).expect("first pass always sends");
    assert!(record.anchor);
    assert_eq!(record.anchor_as_of, t0());
    assert_eq!(record.prev_as_of, None);
    assert_eq!(record.row_count(), 3);
    assert_eq!(record.repos.len(), 2);
    assert_eq!(record.census_at, Some(t0() - Duration::minutes(1)));
}

#[test]
fn an_empty_tracker_still_anchors_so_silence_is_unambiguous() {
    let view = build_view(&input(Vec::new(), Vec::new()), &BTreeSet::new());
    let record = decide(&view, None, t0()).unwrap();
    assert!(record.anchor);
    assert!(record.repos.is_empty());
}

#[test]
fn an_unchanged_pass_inside_the_hour_sends_nothing() {
    let last = emitted(&base(), t0(), t0());
    // A new census instant alone is not a change.
    let mut later = base();
    later.census.as_mut().unwrap().0 = t0() + Duration::minutes(4);
    let view = build_view(&later, &BTreeSet::new());
    assert_eq!(decide(&view, Some(&last), t0() + Duration::minutes(5)), None);
}

#[test]
fn a_change_sends_a_delta_with_only_what_moved() {
    let last = emitted(&base(), t0(), t0());
    let mut next = base();
    // Issue 10 moves to review; issue 20 leaves; issue 30 arrives.
    next.items = vec![
        LiveItem {
            stage: Stage::ReviewWait,
            entered_at: t0() + Duration::minutes(3),
            running_sweep_id: None,
            ..item(REPO, 10, Stage::ReviewWait, None)
        },
        item(REPO, 30, Stage::SweepCurator, Some("sweep-issue-30-1")),
        item(OTHER, 5, Stage::ReadyWait, None),
    ];
    let now = t0() + Duration::minutes(5);
    let record = decide(&build_view(&next, &BTreeSet::new()), Some(&last), now).unwrap();
    assert!(!record.anchor);
    assert_eq!(record.anchor_as_of, t0());
    assert_eq!(record.prev_as_of, Some(t0()));
    assert_eq!(record.as_of, now);
    assert_eq!(record.repos.len(), 1, "the unchanged repo is not restated");
    let repo = &record.repos[0];
    assert_eq!(repo.repo, REPO);
    let issues: Vec<u32> = repo.rows.iter().map(|r| r.issue).collect();
    assert_eq!(issues, vec![10, 30]);
    assert_eq!(repo.removed, vec![20]);
    assert_eq!(repo.rows[0].host, None, "issue 10's sweep ended");
    // A delta restates the census of every repo it names.
    assert_eq!(repo.census.as_ref().unwrap().open, 3);
}

#[test]
fn a_census_change_alone_sends_a_delta() {
    let last = emitted(&base(), t0(), t0());
    let mut next = base();
    next.census = Some((t0(), vec![census(REPO, 4)]));
    let record =
        decide(&build_view(&next, &BTreeSet::new()), Some(&last), t0() + Duration::minutes(5))
            .unwrap();
    assert_eq!(record.repos.len(), 1);
    assert!(record.repos[0].rows.is_empty() && record.repos[0].removed.is_empty());
    assert_eq!(record.repos[0].census.as_ref().unwrap().open, 4);
}

#[test]
fn a_slot_change_alone_sends_a_delta() {
    let last = emitted(&base(), t0(), t0());
    let mut next = base();
    next.slots = Some((4, Some(2)));
    let record =
        decide(&build_view(&next, &BTreeSet::new()), Some(&last), t0() + Duration::minutes(5))
            .unwrap();
    assert!(!record.anchor && record.repos.is_empty());
    assert_eq!(record.slots.unwrap().occupancy, Some(2));
}

#[test]
fn a_repo_that_vanishes_is_removed_whole() {
    let last = emitted(&base(), t0(), t0());
    let mut next = base();
    next.items.retain(|i| i.repo != OTHER);
    let record =
        decide(&build_view(&next, &BTreeSet::new()), Some(&last), t0() + Duration::minutes(5))
            .unwrap();
    let gone = record.repos.iter().find(|r| r.repo == OTHER).unwrap();
    assert_eq!(gone.removed, vec![5]);
    assert_eq!(gone.census, None);
}

#[test]
fn the_hourly_anchor_fires_even_with_no_change() {
    let anchor_at = t0();
    // The last record was a delta 10 minutes ago; its chain's anchor is 1 h old.
    let last = emitted(&base(), anchor_at + Duration::minutes(50), anchor_at);
    let view = build_view(&base(), &BTreeSet::new());
    let just_before = anchor_at + Duration::seconds(ANCHOR_INTERVAL_SECS - 1);
    assert!(!needs_anchor(Some(&last), just_before));
    assert_eq!(decide(&view, Some(&last), just_before), None);
    let due = anchor_at + Duration::seconds(ANCHOR_INTERVAL_SECS);
    assert!(needs_anchor(Some(&last), due));
    let record = decide(&view, Some(&last), due).unwrap();
    assert!(record.anchor);
    assert_eq!(record.anchor_as_of, due);
    assert_eq!(record.prev_as_of, None);
    assert_eq!(record.row_count(), 3, "an anchor carries every row");
}

/// Applying anchor + deltas the way the replay doc says yields the live view.
#[test]
fn replaying_anchor_then_deltas_reconstructs_the_current_state() {
    type State = BTreeMap<(String, u32), (Stage, Option<String>)>;
    fn apply(state: &mut State, record: &crate::telemetry::kinds::fleet_state::FleetStateRecord) {
        if record.anchor {
            state.clear();
        }
        for repo in &record.repos {
            for issue in &repo.removed {
                state.remove(&(repo.repo.clone(), *issue));
            }
            for row in &repo.rows {
                state.insert((repo.repo.clone(), row.issue), (row.stage, row.host.clone()));
            }
        }
    }
    let passes = [
        base(),
        {
            let mut p = base();
            p.items.remove(1);
            p
        },
        {
            let mut p = base();
            p.items.remove(1);
            p.items.push(item(OTHER, 6, Stage::Doctor, None));
            p
        },
    ];
    let mut last: Option<Emitted> = None;
    let mut replayed = State::new();
    for (n, pass) in passes.iter().enumerate() {
        let now = t0() + Duration::minutes(5 * n as i64);
        let view = build_view(pass, &BTreeSet::new());
        if let Some(record) = decide(&view, last.as_ref(), now) {
            apply(&mut replayed, &record);
            last = Some(Emitted {
                anchor_as_of: record.anchor_as_of,
                view,
                as_of: now,
            });
        }
    }
    let live: State = passes[2]
        .items
        .iter()
        .map(|i| {
            let host = i
                .running_sweep_id
                .as_ref()
                .map(|_| "robb-studio".to_string());
            ((i.repo.clone(), i.issue), (i.stage, host))
        })
        .collect();
    assert_eq!(replayed, live);
}

#[derive(Default)]
struct Capture(StdMutex<Vec<TelemetryEnvelope>>);

impl QueueSink for Capture {
    fn offer(&self, envelope: TelemetryEnvelope) {
        self.0.lock().unwrap().push(envelope);
    }
    fn offer_durable(&self, envelope: TelemetryEnvelope) -> std::io::Result<()> {
        self.offer(envelope);
        Ok(())
    }
}

#[test]
fn the_sink_stamps_the_host_and_wraps_the_kind() {
    let capture = Arc::new(Capture::default());
    let sink = FleetStateSink::new(capture.clone(), "robb-studio");
    let view = build_view(&base(), &BTreeSet::new());
    sink.push(decide(&view, None, t0()).unwrap());
    let got = capture.0.lock().unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].host_id, "robb-studio");
    assert!(matches!(got[0].record, TelemetryRecord::FleetState(_)));
}
