//! Train/serve parity of the whole twin-otter input for a PR the tracker
//! first sees mid-stage (#10500).
//!
//! One scenario is built as fleet snapshots (training, `fit::rows::build`)
//! and as a **first-seen** tracker: one listing pass, no history and no
//! journal, the same snapshots loaded as the label timeline and event log.
//! At the row instant, the input the tracker hands
//! `land-2026-10-04-twin-otter` must equal the training row field for field
//! (stage, age, every count, rework, flags, clock), and so must the 20 model
//! features, bit for bit, for `review_wait`, `doctor_wait`, `merge_wait` and
//! `merge_hold` alike. Every PR's listing `updated_at` is later than its stage
//! entry, so the old lower bound is wrong for each of them.
//!
//! The scenario's repo also has a merge of a PR that never carried a loom
//! review label: both sides count it.

use super::fit_rows::{cutoff, h, row, snapshot, REPO};
use super::hold_parity::{fitted, listings_at, served, snapshots, views_at, Spec, AT, LAST_PASS};
use super::provenance;
use crate::eta::fit::{clock, model_features, rows, ModelInputs};
use crate::eta::fleet::FleetSnapshot;
use crate::eta::fleet_log::{one_per_repo, SnapshotLog};
use crate::eta::queue_features::{EventKind, EventLog, StageEvent};
use crate::eta::tracker::{events_from_journal, Tracker};
use crate::eta::twin_otter::TwinOtterInput;
use crate::eta::Stage;
use crate::pr_latency::history::fixtures::t;
use crate::pr_latency::history::{PrHistory, PrState};
use crate::pr_latency::{APPROVED, CHANGES_REQUESTED, REVIEW_REQUESTED};
use chrono::Duration;

const RR: &str = REVIEW_REQUESTED;
const CR: &str = CHANGES_REQUESTED;
const PR: &str = APPROVED;
const OP: &str = "loom:operator";

/// The unlabelled PR: merged at 18 h, never under a loom review label.
const UNLABELLED: u32 = 56;

/// [`REPO`], every open PR touched at 19 h (after each stage entry):
///
/// - 51: review 10 h, changes requested 12 h, review again 14 h, changes
///   requested again 16 h: `doctor` since 16 h, two rounds;
/// - 52: review 11 h: `review_wait` since 11 h;
/// - 53: review 13 h, approved 15 h: `merge_wait` since 15 h;
/// - 54: review 12 h, approved 13 h, held 14 h, released 17 h:
///   `merge_wait` since 13 h pooled, 17 h as its split episode;
/// - 55: review 9 h, approved 10 h, merged 17 h;
/// - 57: review 12 h, approved 13 h, held 15 h: `merge_hold` since 15 h;
/// - 56 ([`UNLABELLED`]): merged 18 h, no label ever (snapshot only).
fn specs() -> Vec<Spec> {
    let spec = |pr, steps| Spec {
        repo: REPO,
        pr,
        steps,
        merged: None,
        touched: Some(19.0),
    };
    let merged = Spec {
        merged: Some(17.0),
        touched: None,
        ..spec(55, &[(9.0, &[RR]), (10.0, &[PR])])
    };
    vec![
        spec(51, &[(10.0, &[RR]), (12.0, &[CR]), (14.0, &[RR]), (16.0, &[CR])]),
        spec(52, &[(11.0, &[RR])]),
        spec(53, &[(13.0, &[RR]), (15.0, &[PR])]),
        spec(
            54,
            &[
                (12.0, &[RR]),
                (13.0, &[PR]),
                (14.0, &[PR, OP]),
                (17.0, &[PR]),
            ],
        ),
        merged,
        spec(57, &[(12.0, &[RR]), (13.0, &[PR]), (15.0, &[PR, OP])]),
    ]
}

/// The unlabelled merge, as the refresh reads it.
fn unlabelled() -> PrHistory {
    PrHistory::new(UNLABELLED, t(0), PrState::Merged, Some(h(18.0)), Vec::new(), Vec::new(), true)
}

/// The training snapshots: the specs' plus the unlabelled merge in [`REPO`].
fn fleet(specs: &[Spec]) -> Vec<FleetSnapshot> {
    let mut snapshots = snapshots(specs);
    for s in snapshots.iter_mut().filter(|s| s.repo == REPO) {
        s.merge(&[unlabelled()], cutoff() + Duration::hours(1));
    }
    snapshots
}

/// A tracker that sees the scenario once, at [`LAST_PASS`], with the
/// snapshots `snapshots` (none: the pre-#10500 serving path).
fn first_seen(specs: &[Spec], snapshots: Option<&[FleetSnapshot]>) -> Tracker {
    let mut tracker = Tracker::new(provenance());
    if let Some(snapshots) = snapshots {
        tracker.on_fleet_snapshots(snapshots, h(LAST_PASS));
    }
    tracker.on_listing(REPO, &views_at(specs, REPO, LAST_PASS), h(LAST_PASS), 300);
    let events = events_from_journal(&[], h(LAST_PASS));
    tracker.on_fleet_context(&listings_at(specs, LAST_PASS), events, h(LAST_PASS));
    tracker
}

/// The model inputs a served record feeds the shared transform at its
/// `as_of`, as `twin_otter::eval` builds them; `None` when a count is
/// missing (it would be imputed).
fn served_inputs(i: &TwinOtterInput) -> Option<ModelInputs> {
    let (hour_utc, weekend) = clock(i.as_of);
    Some(ModelInputs {
        age_h: i.age_h,
        ahead: i.ahead?,
        n_stage_repo: i.n_stage_repo?,
        exits_repo_6h: i.exits_repo_6h?,
        exits_repo_24h: i.exits_repo_24h?,
        exits_fleet_6h: i.exits_fleet_6h?,
        merges_repo_24h: i.merges_repo_24h?,
        merges_fleet_6h: i.merges_fleet_6h?,
        since_merge_h: i.since_merge_h?,
        n_stage_fleet: i.n_stage_fleet?,
        hour_utc,
        weekend,
        rework: i.rework,
        op_hold: i.op_hold != 0,
        sequenced: i.sequenced != 0,
        starred: i.starred != 0,
        conflict: i.conflict != 0,
        ci_fail: i.ci_fail != 0,
        blocked: i.blocked != 0,
    })
}

#[test]
fn a_first_seen_prs_whole_input_is_its_training_row_bit_for_bit() {
    let specs = specs();
    let snapshots = fleet(&specs);
    let trained = rows::build(&snapshots, cutoff());
    let registry = fitted();
    let mut tracker = first_seen(&specs, Some(&snapshots));

    // By hand at 20 h: (pr, stage, age_h, rework, ahead, op_hold).
    let by_hand = [
        (51, "doctor_wait", 4.0, 2, 0, false),
        (52, "review_wait", 9.0, 0, 0, false),
        (53, "merge_wait", 5.0, 0, 0, false),
        (54, "merge_wait", 3.0, 0, 1, false),
        (57, "merge_hold", 5.0, 0, 0, true),
    ];
    for (pr, stage, age_h, rework, ahead, op_hold) in by_hand {
        let spec = specs.iter().find(|s| s.pr == pr).unwrap();
        let training = row(&trained, REPO, pr, h(AT)).unwrap_or_else(|| panic!("row {pr}"));
        let want = &training.inputs;
        assert_eq!(
            (training.stage.as_str(), want.age_h, want.rework, want.ahead, want.op_hold),
            (stage, age_h, rework, ahead, op_hold),
            "PR {pr} trained"
        );
        // Merges in REPO by 20 h: 55 (17 h) and the unlabelled 56 (18 h).
        assert_eq!(
            (want.merges_repo_24h, want.merges_fleet_6h, want.since_merge_h),
            (2, 2, 2.0),
            "PR {pr} trained merges"
        );

        let (input, imputed) = served(&mut tracker, &registry, spec);
        assert!(imputed.is_empty(), "PR {pr} imputed {imputed:?}");
        assert_eq!(input.stage, stage, "PR {pr} stage");
        assert_eq!(input.as_of, h(AT), "PR {pr} instant");
        let got = served_inputs(&input).expect("no missing count");
        assert_eq!(got, *want, "PR {pr}: serving differs from training");
        let bits = |m: &ModelInputs| model_features(m).map(f64::to_bits);
        assert_eq!(bits(&got), bits(want), "PR {pr}: model features differ");
    }
}

/// The skew itself: without the snapshots the same listing is dated from
/// `updated_at` (19 h), so every PR reads an age of an hour, and the
/// unlabelled merge is not counted.
#[test]
fn without_the_snapshots_age_is_the_updated_at_bound_and_merges_are_missed() {
    let specs = specs();
    let registry = fitted();
    let mut tracker = first_seen(&specs, None);
    for pr in [51, 52, 53] {
        let spec = specs.iter().find(|s| s.pr == pr).unwrap();
        let (input, _) = served(&mut tracker, &registry, spec);
        assert_eq!(input.age_h, 1.0, "PR {pr} age from updated_at");
        assert_ne!(input.merges_repo_24h, Some(2), "PR {pr} merges");
    }
}

/// Training counts a merge of a PR with no loom review label (#10500): one
/// more merge than a snapshot that records only the labelled PRs' episodes
/// (a pre-#10500 file), and a later last merge.
#[test]
fn training_counts_a_merge_of_a_pr_that_never_carried_a_review_label() {
    let specs = specs();
    let with = fleet(&specs);
    let mut without = with.clone();
    for s in &mut without {
        s.merges.clear();
    }
    let (a, b) = (rows::build(&with, cutoff()), rows::build(&without, cutoff()));
    let merges = |a: &rows::Assembled| {
        let r = &row(a, REPO, 52, h(AT)).expect("row").inputs;
        (r.merges_repo_24h, r.merges_fleet_6h, r.since_merge_h)
    };
    assert_eq!(merges(&a), (2, 2, 2.0));
    assert_eq!(merges(&b), (1, 1, 3.0));
    // A labelled merge is still one event, never doubled by its record.
    let snapshot = with.iter().find(|s| s.repo == REPO).unwrap();
    assert_eq!(
        snapshot
            .merges
            .iter()
            .map(|m| m.pr_number)
            .collect::<Vec<_>>(),
        vec![55, UNLABELLED]
    );
}

/// A snapshot records every merged PR it reads, labelled or not, and a
/// re-read replaces the record.
#[test]
fn a_snapshot_records_every_merge_and_a_reread_replaces_it() {
    let mut s = snapshot(REPO, &[unlabelled()], h(30.0));
    assert_eq!(s.merges.len(), 1);
    assert_eq!((s.merges[0].pr_number, s.merges[0].at), (UNLABELLED, h(18.0)));
    let id = s.snapshot_id.clone();
    let reopened =
        PrHistory::new(UNLABELLED, t(0), PrState::Open, None, Vec::new(), Vec::new(), true);
    s.merge(&[reopened], h(31.0));
    assert!(s.merges.is_empty());
    assert_ne!(s.snapshot_id, id, "the merge is part of the id");
}

/// Serving's log: the snapshots' events before their horizon in the repos
/// they cover, the journal's after it and in every other repo.
#[test]
fn serving_splices_the_journal_after_the_snapshot_horizon() {
    let specs = specs();
    let snapshots = fleet(&specs);
    let chosen = one_per_repo(&snapshots);
    let horizon = h(19.0);
    let log = SnapshotLog::new(&chosen, horizon);
    let event = |repo: &str, pr, at| StageEvent {
        repo: repo.to_string(),
        pr: Some(pr),
        stage: Some(Stage::MergeWait),
        kind: EventKind::Merge,
        at: h(at),
        known_at: h(19.5),
    };
    let journal = EventLog {
        from: Some(h(0.5)),
        events: vec![
            event(REPO, 55, 17.0),     // before H, covered: the snapshot's
            event(REPO, 58, 19.2),     // after H: kept
            event("x/other", 7, 12.0), // a repo no snapshot covers: kept
        ],
    };
    let served = log.serve(journal, h(19.5));
    // Each merge once: the journal's 55 (before H) gives way to the
    // snapshot's, which also brings the unlabelled 56.
    let mut merges: Vec<(String, u32, i64)> = served
        .events
        .iter()
        .filter(|e| e.kind == EventKind::Merge)
        .map(|e| (e.repo.clone(), e.pr.unwrap(), (e.known_at - e.at).num_seconds()))
        .collect();
    merges.sort();
    let lagged = |pr| (REPO.to_string(), pr, 120);
    assert_eq!(
        merges,
        vec![
            lagged(55),
            lagged(UNLABELLED),
            (REPO.to_string(), 58, (h(19.5) - h(19.2)).num_seconds()),
            ("x/other".to_string(), 7, (h(19.5) - h(12.0)).num_seconds()),
        ]
    );
    assert_eq!(served.from, log.at(horizon).from, "training's `from`");
    // No snapshot: the journal as is.
    let journal = EventLog {
        from: Some(h(0.5)),
        events: vec![event(REPO, 55, 17.0)],
    };
    assert_eq!(SnapshotLog::default().serve(journal.clone(), h(19.5)), journal);
}
