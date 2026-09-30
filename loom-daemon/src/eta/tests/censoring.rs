//! Right-censoring: the Kaplan–Meier grid, where censored samples come from,
//! and what `land-v2` does with them that `land-v1` cannot (#9328).

use super::{as_of, history_a, input_at, provenance};
use crate::eta::grid;
use crate::eta::heuristics::{LandV1, LandV2};
use crate::eta::history::{Level, SampleSource, StageSample, StageSamples};
use crate::eta::journal::{censored_from_pr_history, JournalEntry};
use crate::eta::{Heuristic, Stage};
use chrono::Duration;

// ------------------------------------------------------- the KM primitive

#[test]
fn with_no_censored_samples_the_km_grid_is_exactly_the_nearest_rank_grid() {
    // The product-limit estimator degenerates to the empirical CDF, so
    // `land-v2` over a fully-observed history is `land-v1` over it.
    for n in [8_usize, 9, 20, 41] {
        let sorted: Vec<i64> = (1..=n as i64).map(|i| i * 60).collect();
        assert_eq!(grid::km_grid_of(&sorted, &[]), grid::grid_of(&sorted), "n = {n}");
    }
}

#[test]
fn km_curve_keeps_censored_samples_at_risk_without_counting_them_as_events() {
    // Four observed events, one censored between the second and third. The
    // censored sample is at risk for events at or below its bound, so it
    // dilutes those hazards and raises the survival of everything after.
    let observed = [10_i64, 20, 30, 40];
    let censored = [25_i64];
    let curve = grid::km_curve(&observed, &censored);
    assert_eq!(curve.len(), 4, "one step per distinct event time");
    // t=10: 5 at risk (4 observed + 1 censored), 1 event -> S = 0.8
    assert!((curve[0].survival - 0.8).abs() < 1e-12, "{:?}", curve[0]);
    // t=20: 4 at risk, 1 event -> S = 0.8 * 0.75 = 0.6
    assert!((curve[1].survival - 0.6).abs() < 1e-12, "{:?}", curve[1]);
    // t=30: the censored sample has dropped out, 2 at risk -> S = 0.3
    assert!((curve[2].survival - 0.3).abs() < 1e-12, "{:?}", curve[2]);
    // t=40: 1 at risk -> S = 0
    assert!(curve[3].survival.abs() < 1e-12, "{:?}", curve[3]);

    // A censored sample at exactly an event time is still at risk there.
    let tie = grid::km_curve(&[10_i64, 20], &[10]);
    assert!((tie[0].survival - (1.0 - 1.0 / 3.0)).abs() < 1e-12);
}

#[test]
fn km_quantiles_are_monotone_and_the_grid_reads_longer_under_censoring() {
    let observed: Vec<i64> = (1..=10).map(|i| i * 100).collect();
    // Ten more samples, all still open past the longest observed one: half
    // the population is known to outlive 1000s, and none of it is in
    // `land-v1`'s grid at all.
    let censored: Vec<i64> = vec![1000; 10];
    let plain = grid::grid_of(&observed);
    let km = grid::km_grid_of(&observed, &censored);
    assert!(km.windows(2).all(|w| w[0] <= w[1]), "a grid must be ascending: {km:?}");
    assert_eq!(km.len(), plain.len());
    assert_eq!(km[0], plain[0], "the minimum is an observed value either way");
    // Every quantile above the minimum reads at least as long, and the median
    // strictly longer: the dropped samples were the slow ones.
    assert!(km.iter().zip(&plain).all(|(k, p)| k >= p), "km {km:?} vs plain {plain:?}");
    assert!(km[10] > plain[10], "median: km {} vs plain {}", km[10], plain[10]);

    // The tail the data cannot resolve clamps to the horizon rather than
    // running off the end.
    assert_eq!(*km.last().unwrap(), 1000);
}

#[test]
fn km_quantile_clamps_to_the_horizon_when_survival_never_falls_far_enough() {
    let curve = grid::km_curve(&[10_i64], &[10, 10, 10]);
    // One event out of four at risk: S never falls below 0.75.
    assert_eq!(grid::km_quantile(&curve, 0.10, 99), 10);
    assert_eq!(grid::km_quantile(&curve, 0.90, 99), 99, "undefined -> the horizon");
}

// ------------------------------------------------ where censored samples come from

fn censored_sample(stage: Stage, duration_sec: i64, offset: i64) -> StageSample {
    StageSample {
        repo: "rjwalters/loom".to_string(),
        stage,
        duration_sec,
        observed_at: as_of() - Duration::seconds(offset),
        source: SampleSource::StageJournal,
        host: "host-test".to_string(),
    }
}

#[test]
fn censored_samples_are_kept_apart_from_observed_ones() {
    let mut history = history_a();
    let before = history.select(
        "rjwalters/loom",
        Stage::MergeWait,
        as_of(),
        &[SampleSource::SweepOutcome, SampleSource::StageJournal],
    );
    for i in 0..5 {
        history
            .censored
            .push(censored_sample(Stage::MergeWait, 9_999, 60 * (i + 1)));
    }
    let after = history.select(
        "rjwalters/loom",
        Stage::MergeWait,
        as_of(),
        &[SampleSource::SweepOutcome, SampleSource::StageJournal],
    );
    assert_eq!(
        before, after,
        "select() cannot see censored samples: every v1 grid is unchanged"
    );

    let censored = history.select_censored(
        "rjwalters/loom",
        Stage::MergeWait,
        as_of(),
        &[SampleSource::StageJournal],
        Level::Host,
    );
    assert_eq!(censored, vec![9_999; 5]);

    // The same leak-free rule: a sample observed at or after `as_of` is
    // refused, exactly as `select` refuses one.
    history
        .censored
        .push(censored_sample(Stage::MergeWait, 1, -60));
    let censored = history.select_censored(
        "rjwalters/loom",
        Stage::MergeWait,
        as_of(),
        &[SampleSource::StageJournal],
        Level::Host,
    );
    assert_eq!(censored.len(), 5, "the future sample is refused");

    // And the source filter is honoured.
    assert!(history
        .select_censored(
            "rjwalters/loom",
            Stage::MergeWait,
            as_of(),
            &[SampleSource::SweepOutcome],
            Level::Host,
        )
        .is_empty());
}

#[test]
fn a_journal_row_with_a_censored_bound_is_a_censored_sample_never_a_duration() {
    let mut row = JournalEntry::new("stage.open", "rjwalters/loom", as_of(), &provenance());
    row.stage = Some(Stage::MergeWait);
    row.censored_sec = Some(7_200);
    assert!(row.history_sample("host-test").is_none(), "never a completed duration");
    let sample = row.censored_sample("host-test").unwrap();
    assert_eq!(sample.duration_sec, 7_200);
    assert_eq!(sample.stage, Stage::MergeWait);

    // A completed row is never censored, even if a bound rode along.
    row.duration_sec = Some(100);
    assert!(row.censored_sample("host-test").is_none());
    assert!(row.history_sample("host-test").is_some());

    // In-sweep rows are skipped on both sides (the outcome journal has them).
    let mut in_sweep = JournalEntry::new("sweep.phase", "rjwalters/loom", as_of(), &provenance());
    in_sweep.stage = Some(Stage::MergeWait);
    in_sweep.censored_sec = Some(10);
    in_sweep.in_sweep = true;
    assert!(in_sweep.censored_sample("host-test").is_none());

    // `push_journal` routes each to its own side of the history.
    let mut open = JournalEntry::new("stage.open", "rjwalters/loom", as_of(), &provenance());
    open.stage = Some(Stage::ReviewWait);
    open.censored_sec = Some(500);
    let mut done = JournalEntry::new("label.transition", "rjwalters/loom", as_of(), &provenance());
    done.stage = Some(Stage::ReviewWait);
    done.duration_sec = Some(300);
    let mut history = StageSamples::default();
    history.push_journal(&[open.clone(), done], "host-test");
    assert_eq!(history.stages.len(), 1);
    assert_eq!(history.censored.len(), 1);
    assert_eq!(history.censored[0].duration_sec, 500);

    // A row written before #9328 simply has no bound, and still parses.
    let mut json = serde_json::to_value(&open).unwrap();
    json.as_object_mut().unwrap().remove("censored_sec");
    let legacy: JournalEntry = serde_json::from_value(json).unwrap();
    assert_eq!(legacy.censored_sec, None);
}

#[test]
fn open_pr_segments_become_censored_rows_and_settled_ones_do_not() {
    use crate::pr_latency::history::{PrEvent, PrHistory, PrState};
    use crate::pr_latency::{APPROVED, CHANGES_REQUESTED, REVIEW_REQUESTED};

    let t = |secs: i64| as_of() - Duration::seconds(secs);
    let labeled = |label: &str, secs: i64| PrEvent::Labeled {
        label: label.to_string(),
        at: t(secs),
    };

    // A PR approved two hours ago and still unmerged: `merge_wait`, open.
    let held = PrHistory::new(
        9328,
        t(86_400),
        PrState::Open,
        None,
        vec![APPROVED.to_string()],
        vec![labeled(REVIEW_REQUESTED, 10_800), labeled(APPROVED, 7_200)],
        true,
    );
    let rows = censored_from_pr_history(&held, "rjwalters/loom", as_of(), &provenance());
    let stages: Vec<Stage> = rows.iter().filter_map(|r| r.stage).collect();
    assert_eq!(stages, vec![Stage::MergeWait], "the review wait ended at the approval");
    assert_eq!(rows[0].censored_sec, Some(7_200));
    assert_eq!(rows[0].duration_sec, None, "a bound, never a duration");
    assert_eq!(rows[0].event, "stage.open");

    // A PR waiting on a first verdict: `review_wait`, open.
    let waiting = PrHistory::new(
        9328,
        t(86_400),
        PrState::Open,
        None,
        vec![REVIEW_REQUESTED.to_string()],
        vec![labeled(REVIEW_REQUESTED, 3_600)],
        true,
    );
    let rows = censored_from_pr_history(&waiting, "rjwalters/loom", as_of(), &provenance());
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].stage, Some(Stage::ReviewWait));
    assert_eq!(rows[0].censored_sec, Some(3_600));

    // A rejection nobody has answered: `doctor`, open.
    let rejected = PrHistory::new(
        9328,
        t(86_400),
        PrState::Open,
        None,
        vec![CHANGES_REQUESTED.to_string()],
        vec![
            labeled(REVIEW_REQUESTED, 7_200),
            labeled(CHANGES_REQUESTED, 1_800),
        ],
        true,
    );
    let rows = censored_from_pr_history(&rejected, "rjwalters/loom", as_of(), &provenance());
    let stages: Vec<Stage> = rows.iter().filter_map(|r| r.stage).collect();
    assert_eq!(stages, vec![Stage::Doctor]);
    assert_eq!(rows[0].censored_sec, Some(1_800));

    // A merged PR has no open segment: every one of its stages completed and
    // `entries_from_pr_history` already carries them.
    let merged = PrHistory::new(
        9328,
        t(86_400),
        PrState::Merged,
        Some(t(600)),
        Vec::new(),
        vec![
            labeled(REVIEW_REQUESTED, 7_200),
            labeled(APPROVED, 3_600),
            PrEvent::Merged { at: t(600) },
        ],
        true,
    );
    assert!(censored_from_pr_history(&merged, "rjwalters/loom", as_of(), &provenance()).is_empty());

    // Nor does a closed one.
    let closed = PrHistory::new(
        9328,
        t(86_400),
        PrState::Closed,
        None,
        Vec::new(),
        vec![labeled(REVIEW_REQUESTED, 7_200)],
        true,
    );
    assert!(censored_from_pr_history(&closed, "rjwalters/loom", as_of(), &provenance()).is_empty());
}

#[test]
fn a_stage_cut_short_is_journaled_as_a_bound_not_a_duration() {
    use crate::eta::tracker::{ItemKey, PrState, PrView, Tracker};
    const REPO: &str = "rjwalters/loom";
    let mut tracker = Tracker::new(provenance());
    let at = |secs: i64| as_of() + Duration::seconds(secs);

    // An exactly-observed entry into `review_wait` (a bus phase event), then
    // the PR is closed unmerged 900s later.
    tracker.on_dispatch(REPO, 77, "sweep-issue-77-1", at(0));
    tracker.on_phase(REPO, 77, "curator", None, at(100));
    tracker.on_phase(REPO, 77, "builder", Some(771), at(200));
    tracker.on_listing(REPO, &[], at(300), 300);
    let closed = tracker.on_pr_resolved(&ItemKey::new(REPO, 77), PrState::Closed, at(1_100));
    let row = closed
        .journal
        .iter()
        .find(|r| r.stage == Some(Stage::ReviewWait))
        .expect("the truncated stage is journaled");
    assert_eq!(row.duration_sec, None, "it never completed");
    assert_eq!(row.censored_sec, Some(900), "but it provably lasted at least this long");
    assert!(row.history_sample("host-test").is_none(), "no v1 distribution sees it");
    assert!(row.censored_sample("host-test").is_some(), "land-v2 does");

    // A first sight whose entry is only a lower bound yields no bound at all:
    // a bound on a bound is not evidence.
    let mut inexact = Tracker::new(provenance());
    inexact.on_listing(
        REPO,
        &[PrView {
            number: 881,
            issue: 88,
            labels: vec!["loom:review-requested".to_string()],
            created_at: Some(at(-7_200)),
            updated_at: Some(at(-600)),
        }],
        at(0),
        300,
    );
    inexact.on_listing(REPO, &[], at(300), 300);
    let closed = inexact.on_pr_resolved(&ItemKey::new(REPO, 88), PrState::Closed, at(300));
    let row = closed
        .journal
        .iter()
        .find(|r| r.stage == Some(Stage::ReviewWait))
        .unwrap();
    assert_eq!(row.censored_sec, None);
}

// ------------------------------------------------- the positive control

/// [`history_a`] plus `n` `merge_wait` samples censored at `bound`.
fn history_with_censored_merge_wait(n: usize, bound: i64) -> StageSamples {
    let mut history = history_a();
    for i in 0..n {
        history
            .censored
            .push(censored_sample(Stage::MergeWait, bound, 60 * (i as i64 + 1)));
    }
    history
}

#[test]
fn land_v2_equals_land_v1_until_a_censored_sample_exists_then_diverges() {
    let input = input_at(Stage::ReviewWait, 0, 0);
    let clean = history_a();

    // Positive control, part 1: with nothing censored, the two heuristics draw
    // from byte-identical stage grids — `land-v2` changes the estimator, not
    // the path. Their top-level quantiles are only *close*, not bit-equal:
    // `seed_for` deliberately keys the Monte Carlo seed on the heuristic id
    // (`estimate_id` embeds it), so `land-v1` and `land-v2` sample the
    // identical grids with two independent RNG streams — the same
    // independence the live paired gate relies on to score them as two real
    // observations, not one estimate copied twice.
    let v1 = LandV1.estimate(&input, &clean);
    let v2 = LandV2.estimate(&input, &clean);
    assert!(v1.quantiles().is_some(), "the fixture estimates");
    let (v1_p25, v1_p50, v1_p75) = v1.quantiles().unwrap();
    let (v2_p25, v2_p50, v2_p75) = v2.quantiles().unwrap();
    let close = |a: i64, b: i64| ((a - b).abs() as f64) <= 0.05 * a.max(b) as f64;
    assert!(
        close(v1_p25, v2_p25) && close(v1_p50, v2_p50) && close(v1_p75, v2_p75),
        "no censoring, only independent-seed sampling noise: {:?} vs {:?}",
        v2.quantiles(),
        v1.quantiles()
    );
    // The invariant that actually matters: identical stage grids. Both
    // heuristics resample the same distributions, so this is what "land-v2
    // changes the estimator, not the path" provably means.
    for (a, b) in v1.stages.iter().zip(&v2.stages) {
        assert_eq!(a.distribution.grid_sec, b.distribution.grid_sec, "{:?}", a.stage);
    }
    assert!(
        v2.stages
            .iter()
            .all(|s| s.distribution.censored_n == Some(0)),
        "the censoring heuristic records that it found none"
    );
    assert!(
        v1.stages
            .iter()
            .all(|s| s.distribution.censored_n.is_none()),
        "a v1 explanation is byte-identical to before"
    );

    // Positive control, part 2: add merge_wait samples still open well past
    // every observed one. `land-v1` cannot see them; `land-v2` must, and the
    // estimate must get LONGER — the dropped samples were the slow ones.
    let censored = history_with_censored_merge_wait(12, 4 * 86_400);
    let v1_after = LandV1.estimate(&input, &censored);
    let v2_after = LandV2.estimate(&input, &censored);
    assert_eq!(
        v1_after.quantiles(),
        v1.quantiles(),
        "land-v1 is provably blind to censoring: its answer did not move"
    );
    let (_, v2_p50, _) = v2_after.quantiles().expect("land-v2 still estimates");
    let (_, v1_p50, _) = v1_after.quantiles().unwrap();
    assert!(
        v2_p50 > v1_p50,
        "censoring must lengthen the estimate: land-v2 p50 {v2_p50}s vs land-v1 {v1_p50}s"
    );

    // The explanation says so, and the grid it drew from is the KM one.
    let merge_wait = v2_after
        .stages
        .iter()
        .find(|s| s.stage == Stage::MergeWait)
        .expect("merge_wait is on an always-merge path");
    assert_eq!(merge_wait.distribution.censored_n, Some(12));
    let v1_merge_wait = v1_after
        .stages
        .iter()
        .find(|s| s.stage == Stage::MergeWait)
        .unwrap();
    assert_ne!(
        merge_wait.distribution.grid_sec, v1_merge_wait.distribution.grid_sec,
        "the KM grid is not the nearest-rank one"
    );
    assert!(
        merge_wait.distribution.p50 >= v1_merge_wait.distribution.p50,
        "the summary quartiles come off the same grid the simulation draws from"
    );

    // Recomputable: the simulation reads only what the explanation holds, so
    // a KM-gridded explanation replays to the same numbers.
    let replayed = crate::eta::simulate::run_explanation(&v2_after).unwrap();
    assert_eq!(replayed, v2_after.quantiles().unwrap());
}

#[test]
fn land_v2_keeps_its_own_id_and_never_rewrites_land_v1s() {
    let input = input_at(Stage::ReviewWait, 0, 0);
    let history = history_with_censored_merge_wait(12, 4 * 86_400);
    let v1 = LandV1.estimate(&input, &history);
    let v2 = LandV2.estimate(&input, &history);
    assert_eq!(v1.heuristic, "land-v1");
    assert_eq!(v2.heuristic, "land-v2");
    assert_ne!(v1.estimate_id, v2.estimate_id, "a different id is a different series");
    assert_eq!(v1.kind, v2.kind);
}
