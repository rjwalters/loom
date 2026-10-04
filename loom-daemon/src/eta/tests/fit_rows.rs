//! Training rows from fleet snapshots (#10245): the point-in-time edges, the
//! data horizon, and train/serve parity of the queue features.

use crate::eta::episodes::{EpisodeEnd, EpisodeNext, StageEpisode};
use crate::eta::fit::rows::{self, is_open_at, Assembled, RowKey};
use crate::eta::fit::{DwellEnd, FitStage, ModelInputs, TrainingRow};
use crate::eta::fleet::FleetSnapshot;
use crate::eta::queue_features::{
    queue_features, EventKind, EventLog, QueueSubject, RosterEntry, StageEvent,
};
use crate::eta::Stage;
use crate::pr_latency::history::fixtures::{labeled, t, unlabeled};
use crate::pr_latency::history::{PrEvent, PrHistory, PrState};
use crate::pr_latency::{APPROVED, CHANGES_REQUESTED, REVIEW_REQUESTED};
use chrono::{DateTime, Duration, Utc};

pub(crate) const REPO: &str = "rjwalters/loom";
pub(crate) const OTHER: &str = "rjwalters/other";
pub(crate) const OPERATOR: &str = "loom:operator";
pub(crate) const STAR: &str = "loom:operator-priority";
const LAG: i64 = 120;

/// The cutoff every test here fits at: 14 days after the fixture epoch, so
/// the window opens at `t(0)` and row instants are `t(k · 1800)`.
pub(crate) fn cutoff() -> DateTime<Utc> {
    t(14 * 86_400)
}

/// `x` hours after the fixture epoch.
pub(crate) fn h(x: f64) -> DateTime<Utc> {
    t((x * 3600.0).round() as i64)
}

/// Seconds since the fixture epoch, for the event helpers.
pub(crate) fn secs(at: DateTime<Utc>) -> i64 {
    (at - t(0)).num_seconds()
}

/// Review requested at `review`, approved at `approved` (one edit), merged at
/// `merged`.
pub(crate) fn landed(
    number: u32,
    review: DateTime<Utc>,
    approved: DateTime<Utc>,
    merged: DateTime<Utc>,
) -> PrHistory {
    let mut events = approve(review, approved);
    events.push(PrEvent::Merged { at: merged });
    PrHistory::new(number, t(0), PrState::Merged, Some(merged), Vec::new(), events, true)
}

/// Review requested at `review`, approved at `approved`.
pub(crate) fn approve(review: DateTime<Utc>, approved: DateTime<Utc>) -> Vec<PrEvent> {
    vec![
        labeled(REVIEW_REQUESTED, secs(review)),
        unlabeled(REVIEW_REQUESTED, secs(approved)),
        labeled(APPROVED, secs(approved)),
    ]
}

pub(crate) fn open(number: u32, events: Vec<PrEvent>) -> PrHistory {
    PrHistory::new(number, t(0), PrState::Open, None, Vec::new(), events, true)
}

pub(crate) fn snapshot(repo: &str, prs: &[PrHistory], as_of: DateTime<Utc>) -> FleetSnapshot {
    let mut snapshot = FleetSnapshot::empty(repo);
    snapshot.merge(prs, as_of);
    snapshot
}

/// The row of `pr` (in [`REPO`] unless named) at `at`, if one was built.
pub(crate) fn row<'a>(
    a: &'a Assembled,
    repo: &str,
    pr: u32,
    at: DateTime<Utc>,
) -> Option<&'a TrainingRow> {
    let key = RowKey {
        at,
        repo: repo.to_string(),
        pr,
    };
    a.row_keys
        .iter()
        .position(|k| *k == key)
        .map(|i| &a.rows[i])
}

/// A PR merged early, so `since_merge` is known for every later row.
fn early_merge() -> PrHistory {
    landed(90, h(1.0), h(2.0), h(3.0))
}

// -- knowability at the row instant ---------------------------------------

#[test]
fn an_event_at_t_minus_120s_is_not_used_at_t_and_one_at_t_minus_121s_is() {
    let t0 = h(10.0);
    let prs = vec![
        early_merge(),
        // Entries: 121 s before t0 is a subject at t0, 120 s is not.
        open(1, vec![labeled(REVIEW_REQUESTED, secs(t0) - 121)]),
        open(2, vec![labeled(REVIEW_REQUESTED, secs(t0) - 120)]),
        // Exits: approved 121 s before t0 is a departure at t0, 120 s is not.
        open(3, approve(h(5.0), t0 - Duration::seconds(121))),
        open(4, approve(h(5.0), t0 - Duration::seconds(120))),
    ];
    let a = rows::build(&[snapshot(REPO, &prs, cutoff() + Duration::hours(1))], cutoff());

    let one = row(&a, REPO, 1, t0).expect("entered 121 s before t0");
    assert!(row(&a, REPO, 2, t0).is_none(), "entered 120 s before t0: not knowable");
    assert!(row(&a, REPO, 2, t0 + Duration::minutes(30)).is_some());

    // PR 4's approval is not knowable at t0: still in review_wait, ahead of
    // PR 1. PR 3's is: it is a merge_wait row, and a review_wait exit.
    assert_eq!(row(&a, REPO, 4, t0).unwrap().stage, FitStage::ReviewWait);
    assert_eq!(row(&a, REPO, 3, t0).unwrap().stage, FitStage::MergeWait);
    assert_eq!(one.inputs.n_stage_repo, 1);
    assert_eq!(one.inputs.ahead, 1);
    assert_eq!(one.inputs.exits_repo_6h, 1, "PR 3's exit, not PR 4's");
    assert_eq!(one.inputs.exits_repo_24h, 2, "plus PR 90's");
    assert_eq!(one.inputs.age_h, 121.0 / 3600.0);
}

#[test]
fn an_exit_at_exactly_t_plus_30_minutes_counts() {
    let t1 = h(8.0);
    let prs = vec![
        early_merge(),
        open(5, approve(h(6.0), t1 + Duration::minutes(30))),
        open(6, approve(h(6.0), t1 + Duration::minutes(30) + Duration::seconds(1))),
    ];
    let a = rows::build(&[snapshot(REPO, &prs, cutoff() + Duration::hours(1))], cutoff());
    assert_eq!(row(&a, REPO, 5, t1).unwrap().exit, Some(true));
    assert_eq!(row(&a, REPO, 6, t1).unwrap().exit, Some(false));
}

// -- the data horizon ------------------------------------------------------

fn horizon_fleet() -> Vec<PrHistory> {
    let end = cutoff();
    vec![
        early_merge(),
        landed(91, h(20.0), h(21.0), h(22.0)),
        // Open across the cutoff.
        open(7, vec![labeled(REVIEW_REQUESTED, secs(end - Duration::hours(5)))]),
        // Merged 60 s before the cutoff: inside [T − 120 s, T), not knowable.
        landed(
            8,
            end - Duration::hours(4),
            end - Duration::hours(3),
            end - Duration::seconds(60),
        ),
    ]
}

#[test]
fn labels_censor_at_t_minus_120s_and_the_exit_label_needs_its_horizon() {
    let end = cutoff();
    let a = rows::build(&[snapshot(REPO, &horizon_fleet(), end + Duration::hours(1))], end);
    let horizon = end - Duration::seconds(LAG);
    assert_eq!(a.data_through, horizon);
    assert_eq!(a.row_keys.last().unwrap().at, end - Duration::minutes(30));

    let hour_before = row(&a, REPO, 7, end - Duration::hours(1)).unwrap();
    assert_eq!(hour_before.exit, Some(false), "t + 30 min < H");
    assert_eq!(hour_before.merge.dur_h, 58.0 / 60.0);
    assert!(!hour_before.merge.merged);
    let last = row(&a, REPO, 7, end - Duration::minutes(30)).unwrap();
    assert_eq!(last.exit, None, "t + 30 min ≥ H");
    assert_eq!(last.merge.dur_h, 28.0 / 60.0);

    // A merge in [T − 120 s, T) is censored at H, not an observed merge.
    let merged_late = row(&a, REPO, 8, end - Duration::hours(1)).unwrap();
    assert!(!merged_late.merge.merged);
    assert_eq!(merged_late.merge.dur_h, 58.0 / 60.0);
    assert_eq!(merged_late.exit, Some(false));

    // …whereas an earlier merge is an event, at its own instant.
    let early = row(&a, REPO, 91, h(21.5)).unwrap();
    assert!(early.merge.merged);
    assert_eq!(early.merge.dur_h, 0.5);
    assert_eq!(early.exit, Some(true), "merged at exactly t + 30 min");
}

#[test]
fn a_stale_snapshot_moves_the_horizon_to_its_own_as_of() {
    let end = cutoff();
    let stale = end - Duration::minutes(150);
    let fresh_other = snapshot(OTHER, &[early_merge()], end + Duration::hours(1));
    let a = rows::build(&[snapshot(REPO, &horizon_fleet(), stale), fresh_other], end);
    assert_eq!(a.data_through, stale, "H = min(T − 120 s, oldest as_of)");
    assert_eq!(a.row_keys.last().unwrap().at, end - Duration::hours(3));
    let last = row(&a, REPO, 7, end - Duration::hours(3)).unwrap();
    assert_eq!(last.merge.dur_h, 0.5);
    assert!(!last.merge.merged);
    assert_eq!(last.exit, None, "t + 30 min = H");
    // Dwells end at H too: PR 7's review_wait is censored there.
    assert!(a.dwells.iter().any(|d| d.stage == FitStage::ReviewWait
        && d.dwell_h == 2.5
        && d.end == DwellEnd::Censored));
}

// -- train/serve parity ------------------------------------------------------

/// The parity fleet. Repo A ([`REPO`]):
///
/// - 10: review 1 h, approved 2 h, merged 4 h;
/// - 11: review 15 h, approved 16 h, held 17–18 h, then `merge_wait` again;
/// - 12: review 14 h, approved 15 h, merged 19 h;
/// - 13: review 10 h, doctor 11 h, review 12 h, doctor 13 h (open);
/// - 14: review 5 h, doctor 6 h, review 7 h, doctor 60 s before `t*`;
/// - 15: review 12 h, approved 13 h (open).
///
/// Repo B ([`OTHER`]): 20 review 16 h, approved 17 h, merged 18.5 h; 21
/// review 19 h (open).
fn parity_fleet(at: DateTime<Utc>) -> Vec<FleetSnapshot> {
    let doctor = |x: f64| {
        vec![
            unlabeled(REVIEW_REQUESTED, secs(h(x))),
            labeled(CHANGES_REQUESTED, secs(h(x))),
        ]
    };
    let review_again = |x: f64| {
        vec![
            unlabeled(CHANGES_REQUESTED, secs(h(x))),
            labeled(REVIEW_REQUESTED, secs(h(x))),
        ]
    };
    let mut held = approve(h(15.0), h(16.0));
    held.push(labeled(OPERATOR, secs(h(17.0))));
    held.push(unlabeled(OPERATOR, secs(h(18.0))));
    let mut twice = vec![labeled(REVIEW_REQUESTED, secs(h(10.0)))];
    twice.extend(doctor(11.0));
    twice.extend(review_again(12.0));
    twice.extend(doctor(13.0));
    let mut late_doctor = vec![labeled(REVIEW_REQUESTED, secs(h(5.0)))];
    late_doctor.extend(doctor(6.0));
    late_doctor.extend(review_again(7.0));
    late_doctor.push(unlabeled(REVIEW_REQUESTED, secs(h(20.0)) - 60));
    late_doctor.push(labeled(CHANGES_REQUESTED, secs(h(20.0)) - 60));
    let a = vec![
        landed(10, h(1.0), h(2.0), h(4.0)),
        open(11, held),
        landed(12, h(14.0), h(15.0), h(19.0)),
        open(13, twice),
        open(14, late_doctor),
        open(15, approve(h(12.0), h(13.0))),
    ];
    let b = vec![
        landed(20, h(16.0), h(17.0), h(18.5)),
        open(21, vec![labeled(REVIEW_REQUESTED, secs(h(19.0)))]),
    ];
    vec![snapshot(REPO, &a, at), snapshot(OTHER, &b, at)]
}

/// The log of [`parity_fleet`], written out by hand.
fn parity_log() -> EventLog {
    let event =
        |repo: &str, pr: u32, stage: Stage, kind: EventKind, at: DateTime<Utc>| StageEvent {
            repo: repo.to_string(),
            pr: Some(pr),
            stage: Some(stage),
            kind,
            at,
            known_at: at + Duration::seconds(LAG),
        };
    use EventKind::{Exit, Merge};
    use Stage::{Doctor, MergeHold, MergeWait, ReviewWait};
    EventLog {
        // The later of the two repos' first entries.
        from: Some(h(16.0)),
        events: vec![
            event(REPO, 10, ReviewWait, Exit, h(2.0)),
            event(REPO, 10, MergeWait, Merge, h(4.0)),
            event(REPO, 11, ReviewWait, Exit, h(16.0)),
            event(REPO, 11, MergeWait, Exit, h(17.0)),
            event(REPO, 11, MergeHold, Exit, h(18.0)),
            event(REPO, 12, ReviewWait, Exit, h(15.0)),
            event(REPO, 12, MergeWait, Merge, h(19.0)),
            event(REPO, 13, ReviewWait, Exit, h(11.0)),
            event(REPO, 13, Doctor, Exit, h(12.0)),
            event(REPO, 13, ReviewWait, Exit, h(13.0)),
            event(REPO, 14, ReviewWait, Exit, h(6.0)),
            event(REPO, 14, Doctor, Exit, h(7.0)),
            event(REPO, 14, ReviewWait, Exit, h(20.0) - Duration::seconds(60)),
            event(REPO, 15, ReviewWait, Exit, h(12.0)),
            event(OTHER, 20, ReviewWait, Exit, h(17.0)),
            event(OTHER, 20, MergeWait, Merge, h(18.5)),
        ],
    }
}

/// The roster at `t* = 20 h`, by hand: every PR open in a stage at
/// `t* − 120 s`.
fn parity_roster() -> Vec<RosterEntry> {
    let entry = |repo: &str, pr: u32, stage: Stage, entered_at: DateTime<Utc>| RosterEntry {
        repo: repo.to_string(),
        pr,
        stage: Some(stage),
        entered_at,
        known_at: entered_at + Duration::seconds(LAG),
    };
    vec![
        entry(REPO, 11, Stage::MergeWait, h(18.0)),
        entry(REPO, 13, Stage::Doctor, h(13.0)),
        entry(REPO, 14, Stage::ReviewWait, h(7.0)),
        entry(REPO, 15, Stage::MergeWait, h(13.0)),
        entry(OTHER, 21, Stage::ReviewWait, h(19.0)),
    ]
}

/// The queue features a direct call computes, as [`ModelInputs`] fields.
fn direct(repo: &str, pr: u32, stage: Stage, entered_at: DateTime<Utc>) -> [Option<i64>; 9] {
    let subject = QueueSubject {
        repo: repo.to_string(),
        pr: Some(pr),
        current: Some((stage, entered_at)),
    };
    let scope = vec![REPO.to_string(), OTHER.to_string()];
    let q = queue_features(&subject, &parity_roster(), &parity_log(), &scope, h(20.0));
    [
        q.ahead.map(i64::from),
        q.n_stage_repo.map(i64::from),
        q.n_stage_fleet.map(i64::from),
        q.exits_repo_6h.map(i64::from),
        q.exits_repo_24h.map(i64::from),
        q.exits_fleet_6h.map(i64::from),
        q.merges_repo_24h.map(i64::from),
        q.merges_fleet_6h.map(i64::from),
        q.since_merge_sec,
    ]
}

fn trained(inputs: &ModelInputs) -> [Option<i64>; 9] {
    [
        Some(i64::from(inputs.ahead)),
        Some(i64::from(inputs.n_stage_repo)),
        Some(i64::from(inputs.n_stage_fleet)),
        Some(i64::from(inputs.exits_repo_6h)),
        Some(i64::from(inputs.exits_repo_24h)),
        Some(i64::from(inputs.exits_fleet_6h)),
        Some(i64::from(inputs.merges_repo_24h)),
        Some(i64::from(inputs.merges_fleet_6h)),
        Some((inputs.since_merge_h * 3600.0).round() as i64),
    ]
}

#[test]
fn a_rows_queue_features_equal_a_direct_call_and_the_hand_count() {
    let at = h(20.0);
    let a = rows::build(&parity_fleet(cutoff() + Duration::hours(1)), cutoff());

    // PR 11 in merge_wait after its hold, entered (split) at 18 h.
    let held = row(&a, REPO, 11, at).unwrap();
    assert_eq!(held.stage, FitStage::MergeWait);
    let by_hand = [
        Some(1),    // ahead: PR 15 (13 h)
        Some(1),    // n_stage_repo: PR 15
        Some(1),    // n_stage_fleet: PR 15
        Some(2),    // exits_repo_6h: 11's hold (17 h) and 12's merge (19 h), once
        Some(3),    // exits_repo_24h: plus 10's merge (4 h)
        Some(3),    // exits_fleet_6h: plus 20's merge (18.5 h)
        Some(2),    // merges_repo_24h: 10 and 12
        Some(2),    // merges_fleet_6h: 12 and 20
        Some(3600), // since_merge: 12 at 19 h
    ];
    assert_eq!(trained(&held.inputs), by_hand);
    assert_eq!(direct(REPO, 11, Stage::MergeWait, h(18.0)), by_hand);
    assert_eq!(held.inputs.age_h, 2.0, "the split entry, not the pooled 16 h");
    assert_eq!(held.inputs.rework, 0);
    assert!(!held.inputs.op_hold, "released at 18 h");

    // While held, the same PR is a merge_hold row with op_hold set.
    let holding = row(&a, REPO, 11, h(17.5)).unwrap();
    assert_eq!(holding.stage, FitStage::MergeHold);
    assert!(holding.inputs.op_hold);
    assert_eq!(holding.inputs.age_h, 0.5);

    // Every other row at t* agrees with the direct call too.
    for (repo, pr, stage, entered) in [
        (REPO, 13, Stage::Doctor, h(13.0)),
        (REPO, 14, Stage::ReviewWait, h(7.0)),
        (REPO, 15, Stage::MergeWait, h(13.0)),
        (OTHER, 21, Stage::ReviewWait, h(19.0)),
    ] {
        let r = row(&a, repo, pr, at).unwrap_or_else(|| panic!("{repo}#{pr} at t*"));
        assert_eq!(trained(&r.inputs), direct(repo, pr, stage, entered), "{repo}#{pr}");
        assert_eq!(r.inputs.age_h, (at - entered).num_seconds() as f64 / 3600.0);
    }

    // Rework: the doctor entries knowable at t*.
    assert_eq!(row(&a, REPO, 13, at).unwrap().inputs.rework, 2);
    assert_eq!(
        row(&a, REPO, 14, at).unwrap().inputs.rework,
        1,
        "the 60 s-old entry is not knowable"
    );
    assert_eq!(
        row(&a, REPO, 14, at + Duration::minutes(30))
            .unwrap()
            .inputs
            .rework,
        2
    );
    assert_eq!(row(&a, REPO, 13, at).unwrap().stage, FitStage::DoctorWait);
}

#[test]
fn build_ignores_input_order_and_dwells_follow_the_episodes() {
    let end = cutoff();
    let fleet = parity_fleet(end + Duration::hours(1));
    let a = rows::build(&fleet, end);
    let mut reversed = fleet.clone();
    reversed.reverse();
    for s in &mut reversed {
        s.episodes.reverse();
        s.flag_changes.reverse();
    }
    assert_eq!(rows::build(&reversed, end), a);
    assert_eq!(a.stats.rows_dropped_no_flags, 0);
    assert_eq!(a.rows.len(), a.row_keys.len());
    let mut sorted = a.row_keys.clone();
    sorted.sort();
    assert_eq!(sorted, a.row_keys, "canonical order");

    // PR 11's hold: merge_wait 16–17 h left for merge_hold, which left for
    // merge_wait; nothing before the window, so no delayed entry.
    let hold = a
        .dwells
        .iter()
        .find(|d| d.stage == FitStage::MergeHold)
        .unwrap();
    assert_eq!(hold.dwell_h, 1.0);
    assert_eq!(hold.entry_h, 0.0);
    assert_eq!(hold.end, DwellEnd::Next(FitStage::MergeWait));
}

// -- the shared definitions --------------------------------------------------

#[test]
fn from_stage_maps_exactly_the_four_pr_stages() {
    for stage in Stage::EVERY {
        let want = match stage {
            Stage::ReviewWait => Some(FitStage::ReviewWait),
            Stage::Doctor => Some(FitStage::DoctorWait),
            Stage::MergeWait => Some(FitStage::MergeWait),
            Stage::MergeHold => Some(FitStage::MergeHold),
            _ => None,
        };
        assert_eq!(FitStage::from_stage(stage), want, "{stage:?}");
    }
    let mapped = Stage::EVERY
        .iter()
        .filter_map(|s| FitStage::from_stage(*s))
        .collect::<Vec<_>>();
    assert_eq!(mapped.len(), 4);
}

#[test]
fn is_open_at_is_view_at_returning_open() {
    let episode = |end: EpisodeEnd| StageEpisode {
        repo: REPO.to_string(),
        pr_number: 1,
        stage: Stage::ReviewWait,
        entered_at: h(1.0),
        end,
    };
    let ends = [
        EpisodeEnd::Open { at: h(2.0) },
        EpisodeEnd::Unstaged { at: h(3.0) },
        EpisodeEnd::Left {
            at: h(3.0),
            next: EpisodeNext::Merged,
        },
    ];
    for end in ends {
        let e = episode(end);
        for x in [0.5, 1.0, 1.5, 2.0, 2.5, 3.0, 3.5] {
            let open = matches!(
                e.view_at(h(x)),
                Some(StageEpisode {
                    end: EpisodeEnd::Open { .. },
                    ..
                })
            );
            assert_eq!(is_open_at(&e, h(x)), open, "{end:?} at {x} h");
        }
    }
}

#[test]
fn the_row_builder_and_flag_timeline_are_pure() {
    const FORBIDDEN: &[&str] = &[
        "std::fs",
        "std::env",
        "Utc::now",
        "SystemTime",
        "tokio",
        "Mutex",
    ];
    for (name, source) in [
        ("rows.rs", include_str!("../fit/rows.rs")),
        ("flag_timeline.rs", include_str!("../flag_timeline.rs")),
    ] {
        for needle in FORBIDDEN {
            assert!(!source.contains(needle), "{name} mentions `{needle}`");
        }
    }
}
