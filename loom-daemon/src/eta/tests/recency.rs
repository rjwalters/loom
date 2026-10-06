//! Recency-weighted stage samples and the engine's weighted path (#10209;
//! `land-2026-10-04-fresh-tide`, its heuristic, retired in #10549).

use super::{as_of, history_a, input_at};
use crate::eta::grid;
use crate::eta::heuristics::{estimate_path, LandV2, PathRules};
use crate::eta::history::{SampleSource, StageSample, StageSamples};
use crate::eta::recency::{
    effective_n, resolve_half_life, weight, DEFAULT_HALF_LIFE_SEC, MAX_DOUBLINGS,
};
use crate::eta::{
    EstimateInput, Explanation, Heuristic, Kind, NoEstimateReason, Stage, MIN_SAMPLES,
};
use chrono::Duration;

const DAY: i64 = 86_400;
const SOURCES: &[SampleSource] = &[SampleSource::SweepOutcome, SampleSource::StageJournal];

fn sample(stage: Stage, duration_sec: i64, age_sec: i64) -> StageSample {
    StageSample {
        repo: "rjwalters/loom".to_string(),
        stage,
        duration_sec,
        observed_at: as_of() - Duration::seconds(age_sec),
        source: SampleSource::StageJournal,
        host: "host-test".to_string(),
        worked: None,
    }
}

/// `(duration, age)` pairs as `merge_wait` samples.
fn history_of(samples: &[(i64, i64)]) -> StageSamples {
    let mut history = StageSamples::default();
    for &(duration, age) in samples {
        history.stages.push(sample(Stage::MergeWait, duration, age));
    }
    history
}

/// A deterministic spread of durations with ties (a tiny LCG), so the
/// identity tests cover duplicates and non-round ranks.
fn durations(n: usize, seed: u64) -> Vec<i64> {
    let mut x = seed;
    let mut out: Vec<i64> = (0..n)
        .map(|_| {
            x = x
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((x >> 33) % 50) as i64 * 60
        })
        .collect();
    out.sort_unstable();
    out
}

fn unit(sorted: &[i64]) -> Vec<(i64, f64)> {
    sorted.iter().map(|&d| (d, 1.0)).collect()
}

// --------------------------------------------------------- the weighting

#[test]
fn weights_are_exp_of_minus_age_over_half_life() {
    let h = 2 * DAY;
    assert!((weight(0, Some(h)) - 1.0).abs() < 1e-15);
    assert!((weight(h, Some(h)) - (-1.0_f64).exp()).abs() < 1e-15);
    assert!((weight(2 * h, Some(h)) - (-2.0_f64).exp()).abs() < 1e-15);
    // Flat: no half-life, or a non-positive one.
    assert_eq!(weight(30 * DAY, None), 1.0);
    assert_eq!(weight(30 * DAY, Some(0)), 1.0);
    assert_eq!(DEFAULT_HALF_LIFE_SEC, 2 * DAY);
}

#[test]
fn effective_n_is_sum_squared_over_sum_of_squares() {
    assert!((effective_n([1.0; 8]) - 8.0).abs() < 1e-12);
    assert!((effective_n([1.0, 0.0, 0.0]) - 1.0).abs() < 1e-12);
    // (1 + 0.5)² / (1 + 0.25) = 1.8
    assert!((effective_n([1.0, 0.5]) - 1.8).abs() < 1e-12);
    assert_eq!(effective_n(std::iter::empty()), 0.0);
}

#[test]
fn weighted_quantiles_lean_toward_recent_samples() {
    // Ages 0, 1 and 2 half-lives: weights 1, e^-1, e^-2 (total ≈ 1.503).
    let w = |k: f64| (-k).exp();
    let sorted = [(10_i64, 1.0), (20, w(1.0)), (30, w(2.0))];
    // p50: target ≈ 0.752, reached by the first (newest) sample alone.
    assert_eq!(grid::weighted_rank(&sorted, 50), 10);
    // p75: target ≈ 1.127 → cumulative 1.368 at 20.
    assert_eq!(grid::weighted_rank(&sorted, 75), 20);
    // p95: target ≈ 1.428 → only the last sample reaches it.
    assert_eq!(grid::weighted_rank(&sorted, 95), 30);
    assert_eq!(grid::weighted_rank(&sorted, 0), 10);
    assert_eq!(grid::weighted_rank(&sorted, 100), 30);
    // Flat, the median is the middle sample.
    assert_eq!(grid::quantile(&[10, 20, 30], 50), 20);
}

#[test]
fn weighted_km_counts_weight_in_deaths_and_at_risk() {
    // t=10: at risk 1 + 1 + 0.5 = 2.5, one unit death → S = 0.6.
    // t=20: the censored sample (15) has dropped out, at risk 1 → S = 0.
    let curve = grid::weighted_km_curve(&[(10, 1.0), (20, 1.0)], &[(15, 0.5)]);
    assert_eq!(curve.len(), 2);
    assert!((curve[0].survival - 0.6).abs() < 1e-12, "{:?}", curve[0]);
    assert!(curve[1].survival.abs() < 1e-12, "{:?}", curve[1]);
    // A lighter death removes less survival: half a death of 2.5 at risk.
    let light = grid::weighted_km_curve(&[(10, 0.5), (20, 1.0)], &[(15, 1.0)]);
    assert!((light[0].survival - 0.8).abs() < 1e-12, "{:?}", light[0]);
}

#[test]
fn with_equal_weights_the_weighted_grid_is_exactly_todays_grid() {
    for (n, seed) in [(1_usize, 1_u64), (8, 2), (9, 3), (20, 4), (41, 5), (200, 6)] {
        let observed = durations(n, seed);
        assert_eq!(
            grid::weighted_grid_of(&unit(&observed), &[]),
            grid::grid_of(&observed),
            "no censoring, n = {n}"
        );
        let censored = durations(n / 2 + 1, seed + 100);
        assert_eq!(
            grid::weighted_grid_of(&unit(&observed), &unit(&censored)),
            grid::km_grid_of(&observed, &censored),
            "with censoring, n = {n}"
        );
        // The curve itself, not just the grid read off it.
        assert_eq!(
            grid::weighted_km_curve(&unit(&observed), &unit(&censored)),
            grid::km_curve(&observed, &censored),
            "curve, n = {n}"
        );
    }
}

// --------------------------------------------- the effective-N fallback

#[test]
fn the_half_life_is_kept_when_the_effective_n_already_clears_the_floor() {
    let ages: Vec<i64> = (0..20).map(|i| 3_600 * (i + 1)).collect();
    assert_eq!(resolve_half_life(&ages, 2 * DAY, MIN_SAMPLES), Some(2 * DAY));
}

#[test]
fn a_short_effective_n_widens_the_half_life_and_records_the_one_used() {
    // Two fresh samples and eighteen ten days old. At 2d the old ones weigh
    // e^-5 and the effective N is ≈ 2.2; at 4d ≈ 5.7; at 8d ≈ 14.7 ≥ 8.
    let mut samples = vec![(600, 3_600), (660, 7_200)];
    samples.extend((0..18).map(|i| (6_000 + i * 60, 10 * DAY)));
    let history = history_of(&samples);
    let picked = history
        .select_weighted("rjwalters/loom", Stage::MergeWait, as_of(), SOURCES, 2 * DAY, true)
        .expect("twenty samples clear the raw floor");
    assert_eq!(picked.half_life_sec, Some(8 * DAY), "doubled twice");
    assert!(picked.effective_n >= MIN_SAMPLES as f64, "{}", picked.effective_n);
    assert_eq!(picked.selection.sorted.len(), 20, "n stays the raw count");
    assert_eq!(picked.observed.len(), 20);

    // The explanation records the widened half-life and the effective N.
    let explanation = weighted(DEFAULT_HALF_LIFE_SEC, &input_at(Stage::MergeWait, 0, 0), &history);
    assert!(explanation.no_estimate_reason.is_none(), "{:?}", explanation.no_estimate_reason);
    let stage = &explanation.stages[0].distribution;
    assert_eq!(stage.n, 20);
    assert_eq!(stage.half_life_sec, Some(8 * DAY));
    assert_eq!(stage.effective_n, Some(picked.effective_n));
}

#[test]
fn when_no_half_life_clears_the_floor_the_weights_fall_back_to_flat() {
    // One fresh sample and seven fifty days old: even at 2d · 2^6 = 128d the
    // effective N is ≈ 7.8 < 8, so the weights go flat, where it is n = 8.
    let mut samples = vec![(600, 60)];
    samples.extend((0..7).map(|i| (6_000 + i * 60, 50 * DAY)));
    let ages: Vec<i64> = samples.iter().map(|s| s.1).collect();
    assert_eq!(MAX_DOUBLINGS, 6);
    assert_eq!(resolve_half_life(&ages, 2 * DAY, MIN_SAMPLES), None);

    let history = history_of(&samples);
    let picked = history
        .select_weighted("rjwalters/loom", Stage::MergeWait, as_of(), SOURCES, 2 * DAY, true)
        .unwrap();
    assert_eq!(picked.half_life_sec, None);
    assert!((picked.effective_n - 8.0).abs() < 1e-9);
    assert!(picked.observed.iter().all(|o| o.1 == 1.0));
    let explanation = weighted(DEFAULT_HALF_LIFE_SEC, &input_at(Stage::MergeWait, 0, 0), &history);
    let stage = &explanation.stages[0].distribution;
    assert_eq!(stage.half_life_sec, None, "flat: no half-life on the wire");
    assert_eq!(stage.effective_n, Some(8.0), "…but the effective N still is");
}

#[test]
fn below_the_raw_floor_the_stage_is_still_refused() {
    let samples: Vec<(i64, i64)> = (0..(MIN_SAMPLES as i64 - 1))
        .map(|i| (600 + i, 3_600))
        .collect();
    let history = history_of(&samples);
    assert!(history
        .select_weighted("rjwalters/loom", Stage::MergeWait, as_of(), SOURCES, 2 * DAY, true)
        .is_none());
    let explanation = weighted(DEFAULT_HALF_LIFE_SEC, &input_at(Stage::MergeWait, 0, 0), &history);
    assert_eq!(explanation.no_estimate_reason, Some(NoEstimateReason::InsufficientSamples));
}

#[test]
fn the_widening_search_terminates_for_any_input() {
    // Pathological inputs: no samples, a single one, a floor no weighting can
    // reach, a base that would overflow when doubled.
    assert_eq!(resolve_half_life(&[], 2 * DAY, MIN_SAMPLES), None);
    assert_eq!(resolve_half_life(&[DAY], 2 * DAY, 1), Some(2 * DAY));
    assert_eq!(resolve_half_life(&[DAY; 3], 2 * DAY, 4), None);
    assert_eq!(resolve_half_life(&[DAY; 3], i64::MAX, 4), None);
    assert_eq!(resolve_half_life(&[DAY; 3], -1, 1), None, "non-positive is flat");
}

// ------------------------------------------------------------ leak-freedom

#[test]
fn samples_at_or_after_as_of_never_change_the_weighted_result() {
    let mut samples = vec![(600, 3_600), (660, 7_200)];
    samples.extend((0..18).map(|i| (6_000 + i * 60, 10 * DAY)));
    let clean = history_of(&samples);
    let mut leaky = clean.clone();
    // At as_of, and after it — observed and censored alike. Had any of them
    // been admitted, its age-0 weight would dominate.
    for (duration, age) in [(1, 0), (2, -60), (3, -DAY)] {
        leaky.stages.push(sample(Stage::MergeWait, duration, age));
        leaky.censored.push(sample(Stage::MergeWait, duration, age));
    }
    let pick = |h: &StageSamples| {
        h.select_weighted("rjwalters/loom", Stage::MergeWait, as_of(), SOURCES, 2 * DAY, true)
    };
    assert_eq!(pick(&clean), pick(&leaky));
    let input = input_at(Stage::MergeWait, 0, 0);
    assert_eq!(
        serde_json::to_string(&weighted(DEFAULT_HALF_LIFE_SEC, &input, &clean)).unwrap(),
        serde_json::to_string(&weighted(DEFAULT_HALF_LIFE_SEC, &input, &leaky)).unwrap()
    );
}

#[test]
fn censored_samples_are_weighted_at_the_same_half_life() {
    let mut samples = vec![(600, 3_600), (660, 7_200)];
    samples.extend((0..18).map(|i| (6_000 + i * 60, 10 * DAY)));
    let mut history = history_of(&samples);
    history
        .censored
        .push(sample(Stage::MergeWait, 9_000, 10 * DAY));
    history
        .censored
        .push(sample(Stage::MergeWait, 9_000, 3_600));
    let picked = history
        .select_weighted("rjwalters/loom", Stage::MergeWait, as_of(), SOURCES, 2 * DAY, true)
        .unwrap();
    let h = picked.half_life_sec;
    assert_eq!(h, Some(8 * DAY), "the censored side does not move the half-life");
    let mut expected = vec![(9_000, weight(10 * DAY, h)), (9_000, weight(3_600, h))];
    expected.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)));
    assert_eq!(picked.censored, expected);
    // Without censoring asked for, none are read.
    let plain = history
        .select_weighted("rjwalters/loom", Stage::MergeWait, as_of(), SOURCES, 2 * DAY, false)
        .unwrap();
    assert!(plain.censored.is_empty());
}

// ------------------------------------------------------------ the engine

/// The engine's recency-weighted path (`PathRules::half_life_sec`, #10209):
/// `land-v2`'s rules at half-life `half_life_sec`. Its one registered
/// heuristic, `land-2026-10-04-fresh-tide`, was retired and removed (#10549);
/// the weighting stays in the engine and in `recalibrate`, so it stays
/// tested here.
fn weighted(half_life_sec: i64, input: &EstimateInput, history: &StageSamples) -> Explanation {
    estimate_path(
        PathRules {
            id: WEIGHTED,
            kind: Kind::Land,
            sources: SOURCES,
            always_merge: true,
            censoring: true,
            adjust: None,
            models_hold: false,
            half_life_sec: Some(half_life_sec),
            stall_term: false,
            residual_tail: false,
        },
        input,
        history,
    )
}

const WEIGHTED: &str = "test-weighted-path";

#[test]
fn a_weighted_path_records_half_life_and_effective_n_on_every_stage() {
    let history = history_a();
    let input = input_at(Stage::ReviewWait, 0, 0);
    let explanation = weighted(DEFAULT_HALF_LIFE_SEC, &input, &history);
    assert_eq!(explanation.heuristic, WEIGHTED);
    assert!(explanation.result.is_some(), "{:?}", explanation.no_estimate_reason);
    assert!(!explanation.stages.is_empty());
    for entry in &explanation.stages {
        let d = &entry.distribution;
        let ess = d
            .effective_n
            .expect("every weighted stage records its effective N");
        assert!(ess >= MIN_SAMPLES as f64 - 1e-9, "{:?}: {ess}", entry.stage);
        assert!(ess <= d.n as f64 + 1e-9, "{:?}: {ess} > n = {}", entry.stage, d.n);
        if let Some(h) = d.half_life_sec {
            assert!(h >= DEFAULT_HALF_LIFE_SEC, "{:?}: only ever widened", entry.stage);
        }
    }
    // It round-trips, and recomputes from its own explanation.
    let json = serde_json::to_string(&explanation).unwrap();
    assert!(json.contains("\"effective_n\""));
    let back: crate::eta::Explanation = serde_json::from_str(&json).unwrap();
    assert_eq!(back, explanation);
    assert_eq!(
        crate::eta::simulate::run_explanation(&explanation),
        explanation.quantiles_with_p90()
    );
}

#[test]
fn earlier_heuristics_carry_neither_field_on_the_wire() {
    let history = history_a();
    let input = input_at(Stage::ReviewWait, 0, 0);
    let v2 = LandV2.estimate(&input, &history);
    assert!(v2.result.is_some());
    let json = serde_json::to_string(&v2).unwrap();
    assert!(!json.contains("half_life_sec"), "{json}");
    assert!(!json.contains("effective_n"), "{json}");
}

#[test]
fn a_flat_weighted_path_draws_from_exactly_land_v2s_grids() {
    // A non-positive half-life weighs flat: the weighted grid must then be
    // land-v2's Kaplan–Meier grid, stage for stage.
    let history = history_a();
    let input = input_at(Stage::ReviewWait, 0, 0);
    let flat = weighted(0, &input, &history);
    let v2 = LandV2.estimate(&input, &history);
    assert_eq!(flat.stages.len(), v2.stages.len());
    for (a, b) in flat.stages.iter().zip(&v2.stages) {
        assert_eq!(a.stage, b.stage);
        assert_eq!(a.distribution.grid_sec, b.distribution.grid_sec, "{:?}", a.stage);
        assert_eq!(a.distribution.n, b.distribution.n);
        assert_eq!(a.distribution.censored_n, b.distribution.censored_n);
        assert_eq!(a.distribution.half_life_sec, None);
        assert_eq!(a.distribution.effective_n, Some(a.distribution.n as f64));
    }
}

#[test]
fn fresh_tide_is_retired_and_unregistered() {
    let id = "land-2026-10-04-fresh-tide";
    assert!(crate::eta::Registry::builtin().get(id).is_none(), "#10549");
    assert_eq!(crate::eta::shadow_fleet::builtin_tier(id), Some(crate::eta::Tier::Retired));
}
