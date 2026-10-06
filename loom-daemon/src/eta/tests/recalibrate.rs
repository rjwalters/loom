//! Online interval recalibration (#10207): the point-in-time fit, censoring,
//! the per-stage → pooled fallback, the identity on an empty table, the
//! transform over `land-v2` (the `amber-heron` shadow that applied it was
//! retired 2026-10-06, #10484; the machinery stays), and the backtest / outcome-log wiring.

use super::{as_of, history_a, history_a_envelopes, input_at, provenance};
use crate::eta::backtest::{self, Filter};
use crate::eta::calibration_log;
use crate::eta::heuristics::{LandV2, CALIBRATION_BASE, LAND_V2};
use crate::eta::recalibrate::{
    apply, fit_table, recalibrate, CalibrationObservation, CalibrationTable, Mode, Weighting,
    MIN_POOLED_EVENTS, MIN_STAGE_EVENTS, OBSERVATION_SCHEMA,
};
use crate::eta::score::{score, EstimateSummary, OutcomeKind};
use crate::eta::simulate::run_explanation;
use crate::eta::{Heuristic, Kind, Registry, Stage};
use chrono::{DateTime, Duration, Utc};

fn at(secs: i64) -> DateTime<Utc> {
    as_of() + Duration::seconds(secs)
}

/// A base estimate made at `made` (seconds from the fixture instant) with
/// median `p50`, landing `remaining` seconds later and known at once.
fn landed(id: &str, stage: Stage, made: i64, p50: i64, remaining: i64) -> CalibrationObservation {
    CalibrationObservation {
        schema: OBSERVATION_SCHEMA.to_string(),
        estimate_id: id.to_string(),
        heuristic: LAND_V2.to_string(),
        repo: "rjwalters/loom".to_string(),
        issue: 1,
        stage,
        as_of: at(made),
        p50_sec: p50,
        actual_at: Some(at(made + remaining)),
        resolved_at: Some(at(made + remaining)),
        age_sec: None,
        p25_sec: None,
        p75_sec: None,
        p90_sec: None,
    }
}

fn open(id: &str, stage: Stage, made: i64, p50: i64) -> CalibrationObservation {
    CalibrationObservation {
        actual_at: None,
        resolved_at: None,
        ..landed(id, stage, made, p50, 0)
    }
}

/// `n` landings at `stage` whose `ln(actual / p50)` is spread evenly over
/// `[-1, 1]`, all made and resolved well before the fixture instant.
fn log_uniform(stage: Stage, n: usize, tag: &str) -> Vec<CalibrationObservation> {
    (0..n)
        .map(|i| {
            let z = -1.0 + 2.0 * i as f64 / (n - 1) as f64;
            let p50 = 3_600;
            let remaining = (p50 as f64 * z.exp()).round() as i64;
            let made = -86_400 - 60 * i as i64;
            landed(&format!("{tag}-{i}"), stage, made, p50, remaining)
        })
        .collect()
}

fn bytes(table: &CalibrationTable) -> String {
    serde_json::to_string(table).unwrap()
}

// ------------------------------------------------- point-in-time discipline

#[test]
fn later_outcomes_and_later_estimates_never_move_the_table() {
    let t = at(0);
    // Knowable at t: landings resolved before it, plus estimates still open.
    let mut known = log_uniform(Stage::ReviewWait, 30, "rw");
    known.extend(log_uniform(Stage::MergeWait, 25, "mw"));
    known.push(open("open-a", Stage::ReviewWait, -7_200, 1_800));
    // Made before t, landed before t, but only *learned* after t: at t it
    // was still "not landed yet".
    let mut late_known = landed("late-known", Stage::ReviewWait, -7_200, 1_800, 3_600);
    late_known.resolved_at = Some(at(600));
    // Made before t, landing after t.
    let lands_later = landed("lands-later", Stage::ReviewWait, -3_600, 1_800, 9_000);
    let mut observations = known.clone();
    observations.push(late_known.clone());
    observations.push(lands_later.clone());
    let before = fit_table(&observations, LAND_V2, t, Weighting::default());
    assert!(before.per_stage.contains_key(&Stage::ReviewWait), "the fixture fits a stage");
    assert!(before.pooled.is_some(), "and the pool");

    // Perturb every later outcome (resolved at or after t) …
    let mut perturbed = known.clone();
    let mut late_known_b = late_known;
    late_known_b.actual_at = Some(at(-100));
    late_known_b.resolved_at = Some(at(50_000));
    perturbed.push(late_known_b);
    let mut lands_later_b = lands_later;
    lands_later_b.actual_at = Some(at(400_000));
    lands_later_b.resolved_at = Some(at(400_000));
    perturbed.push(lands_later_b);
    // … and add estimates made at or after t, scored and still open.
    perturbed.push(landed("after-a", Stage::ReviewWait, 0, 10, 1_000_000));
    perturbed.push(landed("after-b", Stage::MergeWait, 60, 10, 1_000_000));
    perturbed.push(open("after-c", Stage::ReviewWait, 120, 1));
    // A different heuristic's track record is not this table's evidence.
    let mut other = landed("other", Stage::ReviewWait, -500, 1, 1_000_000);
    other.heuristic = "land-v1".to_string();
    perturbed.push(other);
    let after = fit_table(&perturbed, LAND_V2, t, Weighting::default());
    assert_eq!(bytes(&before), bytes(&after), "byte-identical: nothing later leaks in");

    // Positive control: an outcome resolved *before* t does move it.
    let mut moved = observations.clone();
    for o in moved
        .iter_mut()
        .filter(|o| o.stage == Stage::ReviewWait)
        .take(10)
    {
        o.actual_at = Some(o.as_of + Duration::seconds(100_000));
        o.resolved_at = o.actual_at;
    }
    let moved_t = at(200_000);
    assert_ne!(
        bytes(&fit_table(&moved, LAND_V2, moved_t, Weighting::default())),
        bytes(&fit_table(&observations, LAND_V2, moved_t, Weighting::default())),
        "the control: a knowable outcome is evidence"
    );
}

// ---------------------------------------------------------------- censoring

#[test]
fn open_estimates_above_the_scored_ratios_raise_the_upper_quantiles_never_lower() {
    let scored = log_uniform(Stage::MergeWait, 40, "mw");
    // Ten estimates made a day before t with a one-hour p50, still open at t:
    // each has run at least ln(24) ≈ 3.18 > every scored ratio (≤ 1).
    let mut with_open = scored.clone();
    for i in 0..10 {
        with_open.push(open(&format!("open-{i}"), Stage::MergeWait, -86_400 - i, 3_600));
    }
    for weighting in [
        Weighting::default(),
        Weighting {
            half_life_sec: None,
        },
    ] {
        let ignored = fit_table(&scored, LAND_V2, at(0), weighting);
        let censored = fit_table(&with_open, LAND_V2, at(0), weighting);
        let a = &ignored.per_stage[&Stage::MergeWait];
        let b = &censored.per_stage[&Stage::MergeWait];
        assert_eq!(b.n_censored, 10);
        assert_eq!(a.n_events, b.n_events, "a censored point is never an event");
        for (lo, hi) in [
            (a.q25, b.q25),
            (a.q50, b.q50),
            (a.q75, b.q75),
            (a.q90, b.q90),
        ] {
            assert!(hi >= lo, "censoring never lowers a quantile: {lo} → {hi}");
        }
        assert!(b.q90 > a.q90, "the upper tail moves up: {} → {}", a.q90, b.q90);
    }
}

// --------------------------------------------------- fallback and identity

#[test]
fn a_thin_stage_falls_back_to_the_pool_and_a_thick_one_uses_its_own() {
    let mut observations = log_uniform(Stage::ReviewWait, MIN_STAGE_EVENTS, "rw");
    observations.extend(log_uniform(Stage::Doctor, MIN_STAGE_EVENTS - 1, "dr"));
    let table = fit_table(&observations, LAND_V2, at(0), Weighting::default());
    assert!(table.per_stage.contains_key(&Stage::ReviewWait));
    assert!(!table.per_stage.contains_key(&Stage::Doctor), "below the floor");
    assert_eq!(table.lookup(Stage::ReviewWait).map(|(l, _)| l), Some("stage"));
    assert_eq!(table.lookup(Stage::Doctor).map(|(l, _)| l), Some("pooled"));
    assert_eq!(table.lookup(Stage::MergeWait).map(|(l, _)| l), Some("pooled"));

    let history = history_a();
    let base = LandV2.estimate(&input_at(Stage::MergeWait, 0, 0), &history);
    assert!(base.result.is_some(), "the fixture estimates from merge_wait");
    let out = recalibrate(base, &table, LAND_V2, Mode::SpreadOnly);
    assert_eq!(out.recalibration.as_ref().map(|r| r.level.as_str()), Some("pooled"));

    // Too few landings even pooled: an empty table.
    let thin = log_uniform(Stage::ReviewWait, MIN_POOLED_EVENTS - 1, "thin");
    let empty = fit_table(&thin, LAND_V2, at(0), Weighting::default());
    assert!(empty.per_stage.is_empty() && empty.pooled.is_none());
}

#[test]
fn an_empty_table_returns_the_base_estimate_unchanged() {
    let history = history_a();
    let base = LandV2.estimate(&input_at(Stage::ReviewWait, 0, 0), &history);
    assert!(base.result.is_some(), "the fixture estimates");
    let out =
        recalibrate(base.clone(), &CalibrationTable::empty(as_of()), LAND_V2, Mode::SpreadOnly);
    assert_eq!(serde_json::to_string(&out).unwrap(), serde_json::to_string(&base).unwrap());
    assert!(out.recalibration.is_none());
    assert_eq!(run_explanation(&out), out.quantiles_with_p90());
}

// ------------------------------------------------------- the transform

#[test]
fn a_known_ratio_distribution_is_bracketed_at_its_nominal_coverage() {
    let observations = log_uniform(Stage::ReviewWait, 101, "rw");
    let table = fit_table(
        &observations,
        LAND_V2,
        at(0),
        Weighting {
            half_life_sec: None,
        },
    );
    let q = &table.per_stage[&Stage::ReviewWait];
    assert!((q.q25 + 0.5).abs() < 0.05, "Q(.25) of U[-1,1] ≈ -0.5: {}", q.q25);
    assert!(q.q50.abs() < 0.05, "Q(.5) ≈ 0: {}", q.q50);
    assert!((q.q75 - 0.5).abs() < 0.05, "Q(.75) ≈ 0.5: {}", q.q75);

    let (p25, p50, p75, p90) = apply(3_600, q, Mode::SpreadOnly);
    assert_eq!(p50, 3_600, "spread-only keeps the median");
    assert!(p25 <= p50 && p50 <= p75 && p75 <= p90);
    let covered = observations
        .iter()
        .filter(|o| {
            let remaining = (o.actual_at.unwrap() - o.as_of).num_seconds();
            p25 <= remaining && remaining <= p75
        })
        .count() as f64
        / observations.len() as f64;
    assert!((0.45..=0.56).contains(&covered), "≈50% coverage, got {covered}");

    // Full mode moves the median by Q(.5) as well.
    let shifted: Vec<_> = observations
        .iter()
        .map(|o| CalibrationObservation {
            p50_sec: o.p50_sec / 3,
            ..o.clone()
        })
        .collect();
    let shifted = fit_table(
        &shifted,
        LAND_V2,
        at(0),
        Weighting {
            half_life_sec: None,
        },
    );
    let q = &shifted.per_stage[&Stage::ReviewWait];
    let (_, full_p50, _, _) = apply(1_200, q, Mode::Full);
    assert!((full_p50 - 3_600).abs() < 200, "3x late median recovered: {full_p50}");
}

// ------------------------------------------- the transform over land-v2

#[test]
fn the_retired_shadow_is_unregistered_and_the_transform_still_recomputes_exactly() {
    let registry = Registry::builtin();
    assert!(registry.get("land-2026-10-04-amber-heron").is_none(), "retired, #10484");
    assert_eq!(Registry::default_current(Kind::Land), "land-v1");
    assert_eq!(CALIBRATION_BASE, LAND_V2);

    let history = history_a();
    let observations = log_uniform(Stage::ReviewWait, 60, "rw");
    let input = input_at(Stage::ReviewWait, 0, 0);
    let table = fit_table(&observations, LAND_V2, input.as_of, Weighting::default());
    let estimate =
        recalibrate(LandV2.estimate(&input, &history), &table, LAND_V2, Mode::SpreadOnly);
    let record = estimate.recalibration.as_ref().expect("recalibrated");
    assert_eq!(record.level, "stage");
    assert_eq!(record.base_heuristic, LAND_V2);
    let (p25, p50, p75) = estimate.quantiles().unwrap();
    assert_eq!(p50, record.base_p50_sec, "the median is kept");
    assert!(p25 <= p50 && p50 <= p75 && p75 <= record.p90_sec);
    assert_eq!(run_explanation(&estimate), estimate.quantiles_with_p90());
    assert_eq!(estimate.quantiles_with_p90().unwrap().3, record.p90_sec);
    let parsed: crate::eta::Explanation =
        serde_json::from_str(&serde_json::to_string(&estimate).unwrap()).unwrap();
    assert_eq!(run_explanation(&parsed), estimate.quantiles_with_p90());
}

// ------------------------------------------------------------- wiring

#[test]
fn backtest_replay_yields_one_calibration_observation_per_scored_case() {
    let envelopes = history_a_envelopes();
    let history = history_a();
    let cases = backtest::cases_from_envelopes(&envelopes);
    let replayed = backtest::calibration_from_replay(&LandV2, &history, &cases, &provenance());
    let land_v2 = backtest::run(&LandV2, &history, &cases, Filter::default(), &provenance());
    assert_eq!(replayed.len(), land_v2.overall.scored, "one observation per scored case");
    assert!(replayed
        .iter()
        .all(|o| o.actual_at.is_some() && o.as_of <= o.actual_at.unwrap()));
}

#[test]
fn only_landed_land_estimates_become_observations_and_the_log_round_trips() {
    let history = history_a();
    let estimate = LandV2.estimate(&input_at(Stage::ReviewWait, 0, 0), &history);
    let summary = EstimateSummary::of(&estimate);
    let landed_score = score(&summary, OutcomeKind::Landed, at(3_600), &[]);
    let abandoned = score(&summary, OutcomeKind::Abandoned, at(3_600), &[]);
    let row = CalibrationObservation::from_scored(&summary, &landed_score, at(3_000))
        .expect("a landing is evidence");
    assert_eq!(row.resolved_at, Some(at(3_600)), "never known before it happened");
    assert!(CalibrationObservation::from_scored(&summary, &abandoned, at(3_600)).is_none());

    let dir = tempfile::tempdir().unwrap();
    let path = calibration_log::path(dir.path());
    calibration_log::append(&path, std::slice::from_ref(&row)).unwrap();
    let read = calibration_log::read(&path);
    assert_eq!(read, vec![row.clone()]);

    // The pending twin of a scored estimate is dropped; other pending base
    // estimates join as open; other heuristics' pending ones do not.
    let mut other = summary.clone();
    other.estimate_id = "pending-other".to_string();
    let mut foreign = summary.clone();
    foreign.estimate_id = "pending-foreign".to_string();
    foreign.heuristic = "land-v1".to_string();
    let combined = calibration_log::combine(read, &[summary, other, foreign]);
    assert_eq!(combined.len(), 2);
    assert_eq!(combined[0], row);
    assert_eq!(combined[1].estimate_id, "pending-other");
    assert_eq!(combined[1].actual_at, None);
}
