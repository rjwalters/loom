//! `land-v3` (#9970, Slice 1): registered-not-current, pure, and — on a
//! deterministic correlated-stage fixture — better calibrated than `land-v2`.
//!
//! Nothing here touches live data. The fixture proves the *mechanism* (an
//! independent-draw sum under-covers when an item's stages are correlated,
//! and widening the stage grids moves coverage toward 50% while lowering
//! pinball loss); the live coverage/pinball result is the operator's
//! post-merge backtest (Slice 3), not a claim of this suite.

use super::{history_a, input_at, provenance, LEAKAGE};
use crate::eta::backtest::{self, cases_from_record, Filter, ReplayCase};
use crate::eta::explanation::Features;
use crate::eta::heuristics::{
    complexity_scale, LandV2, LandV3, FRICTION_SEC_PER_RUNNING_SWEEP, INPUT_MISSING, LAND_V3,
    REVIEW_FLOOR_SEC,
};
use crate::eta::history::StageSamples;
use crate::eta::simulate::run_explanation;
use crate::eta::{Explanation, Heuristic, Kind, NoEstimateReason, Registry, Stage};
use crate::telemetry::{TelemetryEnvelope, TelemetryRecord};
use chrono::{Duration, TimeZone, Utc};

// ------------------------------------------------------------- the fixture

/// Per-item slowness regimes, cycled: most items near nominal, a heavy right
/// tail (×2.2, ×4.0). One factor multiplies **every** stage of an item — the
/// within-item correlation an independent Monte Carlo cannot represent.
const REGIMES: [f64; 12] = [0.6, 0.8, 1.0, 0.7, 1.3, 0.9, 2.2, 1.1, 0.75, 4.0, 1.0, 0.85];

/// Small per-stage jitter, so no two samples tie by construction.
const JITTER: [f64; 5] = [0.9, 1.1, 1.0, 1.05, 0.95];

/// Nominal stage seconds: curator, builder, judge (always passes), merge.
const NOMINAL: [(&str, f64); 4] = [
    ("curator", 300.0),
    ("builder", 1800.0),
    ("judge", 600.0),
    ("merge", 900.0),
];

/// Items in the fixture, one every 18 hours (45 days: inside `WINDOW_DAYS`).
const ITEMS: usize = 60;

/// The correlated-stage fixture, as `sweep.outcome` envelopes — built in
/// code rather than checked in, so the generating rule is the fixture.
fn correlated_envelopes() -> Vec<TelemetryEnvelope> {
    let base = Utc.with_ymd_and_hms(2026, 7, 25, 0, 0, 0).unwrap();
    (0..ITEMS)
        .map(|i| {
            let regime = REGIMES[i % REGIMES.len()];
            let phases: Vec<serde_json::Value> = NOMINAL
                .iter()
                .enumerate()
                .map(|(k, (phase, nominal))| {
                    let jitter = JITTER[(i + k) % JITTER.len()];
                    serde_json::json!({
                        "phase": phase,
                        "duration_sec": (nominal * regime * jitter).round() as i64,
                    })
                })
                .collect();
            let total: i64 = phases
                .iter()
                .map(|p| p["duration_sec"].as_i64().unwrap())
                .sum();
            let issue = 20_000 + i;
            let emitted_at = base + Duration::hours(18 * i as i64);
            let line = serde_json::json!({
                "schema_version": 1,
                "emitted_at": emitted_at,
                "host_id": "host-fixture-v3",
                "record": {
                    "kind": "sweep.outcome",
                    "repo": "rjwalters/loom",
                    "visibility": "public",
                    "issue": issue,
                    "sweep_id": format!("sweep-issue-{issue}-1"),
                    "phase_durations": phases,
                    "total_duration_sec": total,
                    "result": "success",
                    "pr_number": issue + 1,
                    "judge_verdicts": [{"attempt": 1, "verdict": "pass"}],
                },
            });
            serde_json::from_value(line).expect("fixture envelope parses")
        })
        .collect()
}

fn samples_of(envelopes: &[TelemetryEnvelope]) -> StageSamples {
    let mut samples = StageSamples::default();
    samples.push_envelopes(envelopes);
    samples
}

fn grid(e: &Explanation, stage: Stage) -> Vec<i64> {
    e.stages
        .iter()
        .find(|s| s.stage == stage)
        .unwrap_or_else(|| panic!("{stage} is on the path"))
        .distribution
        .grid_sec
        .clone()
}

fn width(e: &Explanation) -> i64 {
    let (p25, _, p75) = e.quantiles().expect("an estimate");
    p75 - p25
}

// ------------------------------------------------- registration and purity

#[test]
fn land_v3_is_registered_but_not_current() {
    let registry = Registry::builtin();
    let v3 = registry.get(LAND_V3).expect("land-v3 is registered");
    assert_eq!(v3.kind(), Kind::Land);
    assert_eq!(registry.current(Kind::Land, None).id(), "land-v1");
    assert!(registry.for_kind(Kind::Land).any(|h| h.id() == LAND_V3));
}

#[test]
fn land_v3_is_pure_and_recomputes_from_its_own_explanation() {
    let history = history_a();
    let input = input_at(Stage::SweepCurator, 0, 0);
    let first = LandV3.estimate(&input, &history);
    let second = LandV3.estimate(&input, &history);
    assert_eq!(first, second);
    assert_eq!(first.heuristic, LAND_V3);
    // The adjusted grids are what the explanation carries, so the simulation
    // rebuilt from the explanation alone reproduces the number exactly.
    assert!(first.quantiles().is_some());
    assert_eq!(run_explanation(&first), first.quantiles());
    // Every post-dispatch stage records its adjustment.
    assert!(first
        .stages
        .iter()
        .all(|s| s.distribution.adjustment.is_some()));
}

#[test]
fn earlier_heuristics_record_no_adjustment() {
    let history = history_a();
    let v2 = LandV2.estimate(&input_at(Stage::SweepCurator, 0, 0), &history);
    assert!(v2
        .stages
        .iter()
        .all(|s| s.distribution.adjustment.is_none()));
    let text = serde_json::to_string(&v2).unwrap();
    assert!(!text.contains("adjustment"), "the field is skipped when absent");
}

// ------------------------------------------- calibration on the fixture

#[test]
fn land_v3_interval_is_strictly_wider_than_land_v2s_on_a_heavy_tail() {
    let envelopes = correlated_envelopes();
    let history = samples_of(&envelopes);
    for stage in [Stage::SweepCurator, Stage::SweepBuilder, Stage::ReviewWait] {
        let mut input = input_at(stage, 0, 0);
        input.as_of = Utc.with_ymd_and_hms(2026, 9, 20, 0, 0, 0).unwrap();
        let v2 = LandV2.estimate(&input, &history);
        let v3 = LandV3.estimate(&input, &history);
        assert!(
            width(&v3) > width(&v2),
            "{stage}: land-v3 p25–p75 width {}s must exceed land-v2's {}s",
            width(&v3),
            width(&v2)
        );
    }
}

/// The calibration claim, on the fixture: replayed leak-free over the
/// identical case set, `land-v2`'s p25–p75 coverage is under 50% (44.2%:
/// stages drawn independently, though they move together), `land-v3`'s is
/// closer to 50% (46.6%), and `land-v3`'s mean pinball loss is strictly
/// lower (1816.0s vs 1835.2s). Modest on purpose: these are the numbers of
/// a stationary synthetic fixture, not a forecast of the live gain.
#[test]
fn land_v3_backtests_better_calibrated_than_land_v2_on_the_fixture() {
    let envelopes = correlated_envelopes();
    let history = samples_of(&envelopes);
    let cases = backtest::cases_from_envelopes(&envelopes);
    let comparison =
        backtest::compare(&LandV2, &LandV3, &history, &cases, Filter::default(), &provenance())
            .expect("same kind");
    let (v2, v3) = (&comparison.a.overall, &comparison.b.overall);
    // Identical replay set, identical refusals: land-v3 refuses exactly
    // where land-v2 does (it reads the same selections).
    assert_eq!(v2.n, v3.n);
    assert_eq!(v2.scored, v3.scored);
    assert_eq!(v2.scored, 208, "the fixture's scored case count");

    let c2 = v2.coverage.unwrap();
    let c3 = v3.coverage.unwrap();
    let l2 = v2.mean_pinball_loss_sec.unwrap();
    let l3 = v3.mean_pinball_loss_sec.unwrap();
    eprintln!(
        "fixture: scored {} | land-v2 coverage {c2:.3} pinball {l2:.1}s | \
         land-v3 coverage {c3:.3} pinball {l3:.1}s",
        v2.scored
    );
    // The documented fixture numbers (deterministic: fixed fixture, fixed
    // seeds). A change here is a behaviour change of an immutable id.
    assert!((c2 - FIXTURE_V2_COVERAGE).abs() < 1e-3, "land-v2 coverage {c2}");
    assert!((c3 - FIXTURE_V3_COVERAGE).abs() < 1e-3, "land-v3 coverage {c3}");
    assert!((l2 - FIXTURE_V2_PINBALL).abs() < 0.5, "land-v2 pinball {l2}");
    assert!((l3 - FIXTURE_V3_PINBALL).abs() < 0.5, "land-v3 pinball {l3}");

    assert!(c2 < 0.5, "the fixture must reproduce under-coverage");
    assert!(
        (c3 - 0.5).abs() < (c2 - 0.5).abs(),
        "land-v3 coverage {c3} must be closer to 50% than land-v2's {c2}"
    );
    assert!(l3 < l2, "land-v3 pinball {l3} must beat land-v2's {l2}");
    assert_eq!(comparison.better.as_deref(), Some(LAND_V3));
}

/// `land-v2` on the fixture: 92 of 208 scored cases inside p25–p75.
const FIXTURE_V2_COVERAGE: f64 = 92.0 / 208.0;
/// `land-v3` on the fixture: 97 of 208 — closer to the 50% target.
const FIXTURE_V3_COVERAGE: f64 = 97.0 / 208.0;
/// `land-v2`'s mean pinball loss on the fixture, seconds.
const FIXTURE_V2_PINBALL: f64 = 1835.2;
/// `land-v3`'s mean pinball loss on the fixture, seconds — strictly lower.
const FIXTURE_V3_PINBALL: f64 = 1816.0;

/// The leakage fixture's future record cannot reach a `land-v3` replay it
/// postdates — the same property `land-v1`'s backtest pins, for the new id.
#[test]
fn land_v3_replay_does_not_leak_the_future() {
    let all: Vec<TelemetryEnvelope> = LEAKAGE
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("fixture line parses"))
        .collect();
    let (future, past) = all.split_last().expect("fixture is non-empty");
    let cases: Vec<ReplayCase> = past
        .iter()
        .find_map(|e| match &e.record {
            TelemetryRecord::SweepOutcome(r) if r.issue == 9325 => {
                Some(cases_from_record(r, e.emitted_at))
            }
            _ => None,
        })
        .expect("fixture carries the subject record");
    assert!(cases.iter().all(|c| future.emitted_at > c.as_of));
    let report = |envelopes: &[TelemetryEnvelope]| {
        backtest::run(&LandV3, &samples_of(envelopes), &cases, Filter::default(), &provenance())
    };
    let without = report(past);
    assert!(without.overall.scored > 0, "the fixture must be enough to estimate");
    assert_eq!(report(&all), without, "the future record leaked into the replay");
}

// --------------------------------------------- complexity and friction

#[test]
fn only_the_builder_stage_scales_with_complexity() {
    let history = history_a();
    let sized = |labels: Option<Vec<&str>>| {
        let mut input = input_at(Stage::SweepCurator, 0, 0);
        input.features = Features {
            labels: labels.map(|l| l.into_iter().map(str::to_string).collect()),
            ..Features::default()
        };
        LandV3.estimate(&input, &history)
    };
    let small = sized(Some(vec!["points:1"]));
    let unsized_ = sized(None);
    let large = sized(Some(vec!["loom:issue", "points:13"]));

    let builder = |e: &Explanation| grid(e, Stage::SweepBuilder);
    assert!(builder(&small)[10] < builder(&unsized_)[10]);
    assert!(builder(&unsized_)[10] < builder(&large)[10]);
    let record = |e: &Explanation| {
        e.stages
            .iter()
            .find(|s| s.stage == Stage::SweepBuilder)
            .and_then(|s| s.distribution.adjustment.clone())
            .unwrap()
    };
    assert_eq!(record(&large).scale_basis, "points:13");
    assert!((record(&large).scale - complexity_scale(13)).abs() < 1e-9);
    assert_eq!(record(&unsized_).scale_basis, "none");
    assert!((record(&unsized_).scale - 1.0).abs() < 1e-9);

    // Downstream (Curator before, review/doctor/merge after) is size-blind.
    for stage in [
        Stage::SweepCurator,
        Stage::ReviewWait,
        Stage::Doctor,
        Stage::MergeWait,
    ] {
        assert_eq!(grid(&small, stage), grid(&large, stage), "{stage}");
        assert_eq!(grid(&unsized_, stage), grid(&large, stage), "{stage}");
    }
    // And the end-to-end estimate moves with size.
    assert!(large.quantiles().unwrap().1 > small.quantiles().unwrap().1);
}

#[test]
fn queue_friction_shifts_review_and_merge_only() {
    let history = history_a();
    let with_queue = |running: Option<u32>| {
        let mut input = input_at(Stage::SweepCurator, 0, 0);
        input.features.queue_running = running;
        LandV3.estimate(&input, &history)
    };
    let idle = with_queue(None);
    let busy = with_queue(Some(4));
    let shift = 4 * FRICTION_SEC_PER_RUNNING_SWEEP;
    for stage in [Stage::ReviewWait, Stage::MergeWait] {
        let (a, b) = (grid(&idle, stage), grid(&busy, stage));
        for (x, y) in a.iter().zip(&b) {
            assert_eq!(*y, (*x + shift).max(*x), "{stage}");
        }
    }
    for stage in [Stage::SweepCurator, Stage::SweepBuilder, Stage::Doctor] {
        assert_eq!(grid(&idle, stage), grid(&busy, stage), "{stage}");
    }
    assert!(busy.quantiles().unwrap().1 > idle.quantiles().unwrap().1);
}

// ----------------------------------------------- degrade, never fabricate

#[test]
fn missing_features_degrade_to_land_v2s_medians_and_are_named() {
    let history = history_a();
    let mut input = input_at(Stage::SweepCurator, 0, 0);
    input.features = Features::default();
    let v2 = LandV2.estimate(&input, &history);
    let v3 = LandV3.estimate(&input, &history);

    for name in ["points_marker", "queue_running"] {
        let entry = v3
            .features_omitted
            .iter()
            .find(|o| o.name == name)
            .unwrap_or_else(|| panic!("{name} is named as omitted"));
        assert_eq!(entry.reason, INPUT_MISSING);
    }
    // Same path, same selections; with no feature input each stage keeps
    // land-v2's median — only the calibrated tails (and the review floor)
    // differ.
    assert_eq!(v2.stages.len(), v3.stages.len());
    for (a, b) in v2.stages.iter().zip(&v3.stages) {
        assert_eq!(a.stage, b.stage);
        assert_eq!(a.distribution.n, b.distribution.n);
        let adjustment = b.distribution.adjustment.as_ref().unwrap();
        assert!((adjustment.scale - 1.0).abs() < 1e-9);
        assert_eq!(adjustment.offset_sec, 0);
        assert_eq!(adjustment.raw_p50, a.distribution.p50);
        let floor = if a.stage == Stage::ReviewWait {
            REVIEW_FLOOR_SEC
        } else {
            0
        };
        assert_eq!(b.distribution.p50, a.distribution.p50.max(floor), "{}", a.stage);
    }
}

#[test]
fn insufficient_history_refuses_instead_of_estimating() {
    let v3 = LandV3.estimate(&input_at(Stage::SweepCurator, 0, 0), &StageSamples::default());
    assert_eq!(v3.no_estimate_reason, Some(NoEstimateReason::InsufficientSamples));
    assert!(v3.result.is_none());
    assert!(v3.quantiles().is_none());
}
