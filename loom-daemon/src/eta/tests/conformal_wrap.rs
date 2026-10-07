//! The IPCW conformal wrapper over any `land` base (#10524, slice 5): parity
//! with the shipped quick-tern / swift-tern over twin-otter-b, a different
//! base calibrated on its own rows only, the point-in-time leak test, the
//! recompute through a simulator base (held-heron), the refusals, and the
//! offline backtest wiring. Nothing new is registered.

use super::conformal_ipcw::{at, bytes, fixture, obs, DAY};
use super::held_heron::{held, heron, hold_history};
use super::land_twin_otter::{fit_as_of, fixture_fit};
use super::{history_a, history_a_envelopes, input_at, provenance};
use crate::eta::backtest::{self, Filter};
use crate::eta::conformal;
use crate::eta::conformal_ipcw::{METHOD, METHOD_DRIFT};
use crate::eta::conformal_wrap::{Calibrator, IpcwWrap, CALIBRATED};
use crate::eta::explanation::RegimeAdjustment;
use crate::eta::heuristics::{
    LandQuickTern, LandSwiftTern, LandTwinOtterB, LandV2, StartV1, LAND_CALM_PLOVER,
    LAND_HELD_HERON, LAND_QUICK_TERN, LAND_SWIFT_TERN, LAND_TWIN_OTTER_B, LAND_V2,
};
use crate::eta::recalibrate::CalibrationObservation;
use crate::eta::regime;
use crate::eta::simulate::run_explanation;
use crate::eta::{Explanation, Heuristic, Kind, Registry, Stage, StageSamples};
use std::sync::Arc;

const V2_IPCW: &str = "land-v2+ipcw";
const HERON_IPCW: &str = "land-2026-10-06-held-heron+ipcw";

/// Evidence for `base` in `stage`: the calibrated fixture's estimates made
/// over the two days before the fixture instant, each landing `factor`
/// times later than the fixture says.
fn evidence(base: &str, stage: Stage, factor: f64) -> Vec<CalibrationObservation> {
    fixture(1.0, 300)
        .into_iter()
        .filter(|o| o.as_of >= at(-2 * DAY) && o.as_of < at(0))
        .map(|mut o| {
            o.heuristic = base.to_string();
            o.stage = stage;
            let late = (o.actual_at.unwrap() - o.as_of).num_seconds() as f64 * factor;
            o.actual_at = Some(o.as_of + chrono::Duration::seconds(late.round() as i64));
            o.resolved_at = o.actual_at;
            o
        })
        .collect()
}

fn with_evidence(mut history: StageSamples, rows: Vec<CalibrationObservation>) -> StageSamples {
    history.calibration = rows;
    history
}

fn v2_ipcw() -> IpcwWrap<LandV2> {
    IpcwWrap::new(V2_IPCW, LandV2, Calibrator::Ipcw).expect("land-v2 is a land base")
}

#[test]
fn over_twin_otter_b_the_wrapper_is_quick_tern_and_swift_tern_byte_for_byte() {
    for fit in [None, Some(Arc::new(fixture_fit(fit_as_of())))] {
        // A pre-PR stage: twin-otter-b answers with or without a fit.
        let input = input_at(Stage::SweepBuilder, 0, 0);
        let history = with_evidence(
            super::ready::history_ready(),
            evidence(LAND_TWIN_OTTER_B, Stage::SweepBuilder, 3.0),
        );

        let quick = LandQuickTern::new(fit.clone()).estimate(&input, &history);
        assert_eq!(quick.calibration.as_ref().expect("calibrated").method, METHOD);
        let wrapped =
            IpcwWrap::new(LAND_QUICK_TERN, LandTwinOtterB::new(fit.clone()), Calibrator::Ipcw)
                .unwrap();
        assert_eq!(wrapped.base_id(), LAND_TWIN_OTTER_B);
        assert_eq!(bytes(&wrapped.estimate(&input, &history)), bytes(&quick));
        assert_eq!(wrapped.models_hold(), LandQuickTern::new(fit.clone()).models_hold());

        let swift = LandSwiftTern::new(fit.clone()).estimate(&input, &history);
        assert_eq!(swift.calibration.as_ref().unwrap().method, METHOD_DRIFT);
        let wrapped =
            IpcwWrap::new(LAND_SWIFT_TERN, LandTwinOtterB::new(fit.clone()), Calibrator::IpcwDrift)
                .unwrap();
        assert_eq!(bytes(&wrapped.estimate(&input, &history)), bytes(&swift));
    }
}

#[test]
fn another_base_is_calibrated_on_its_own_rows_only_and_recomputes() {
    let input = input_at(Stage::ReviewWait, 0, 0);
    let bare = LandV2.estimate(&input, &history_a());
    let base_q = bare.quantiles_with_p90().expect("land-v2 answers");

    // Its own rows, 3x slower than estimated: every quantile moves up.
    let history = with_evidence(history_a(), evidence(LAND_V2, Stage::ReviewWait, 3.0));
    let e = v2_ipcw().estimate(&input, &history);
    assert_eq!(e.heuristic, V2_IPCW);
    assert_eq!(
        e.estimate_id,
        crate::eta::estimate_id(&input.subject, Kind::Land, V2_IPCW, input.as_of)
    );
    let record = e.calibration.as_ref().expect("calibrated");
    assert_eq!(record.base, LAND_V2);
    assert_eq!(record.method, METHOD);
    let q = e.quantiles_with_p90().unwrap();
    assert!(q.1 > base_q.1 && q.2 > base_q.2, "{q:?} vs {base_q:?}");
    assert!(q.0 <= q.1 && q.1 <= q.2 && q.2 <= q.3);
    assert_eq!(run_explanation(&e), Some(q));
    let parsed: Explanation = serde_json::from_str(&bytes(&e)).unwrap();
    assert_eq!(run_explanation(&parsed), Some(q));

    // twin-otter-b's rows are not land-v2's evidence: the base, re-identified.
    let foreign = with_evidence(history_a(), evidence(LAND_TWIN_OTTER_B, Stage::ReviewWait, 3.0));
    let e = v2_ipcw().estimate(&input, &foreign);
    assert!(e.calibration.is_none());
    assert_eq!(e.quantiles_with_p90(), Some(base_q));
    assert_eq!(e.heuristic, V2_IPCW);
}

#[test]
fn wrap_leak_post_as_of_outcomes_and_estimates_are_bit_identical() {
    let input = input_at(Stage::ReviewWait, 0, 0);
    let mut rows: Vec<CalibrationObservation> = fixture(1.0, 600)
        .into_iter()
        .filter(|o| o.as_of < at(0))
        .map(|mut o| {
            o.heuristic = LAND_V2.to_string();
            o
        })
        .collect();
    let estimate = |rows: &[CalibrationObservation]| {
        v2_ipcw().estimate(&input, &with_evidence(history_a(), rows.to_vec()))
    };
    let reference = estimate(&rows);
    assert!(reference.calibration.is_some(), "calibrated");

    // Every outcome known at or after `as_of`, moved wildly.
    let mut perturbed = rows.clone();
    let mut moved = 0;
    for o in &mut perturbed {
        if o.resolved_at.is_some_and(|known| known >= at(0)) {
            o.actual_at = Some(at(1_000_000));
            o.resolved_at = Some(at(1_000_000));
            moved += 1;
        }
    }
    assert!(moved > 0, "the fixture has post-as_of outcomes to perturb");
    assert_eq!(bytes(&estimate(&perturbed)), bytes(&reference));

    // A landing before `as_of` known only afterwards: its time is unknowable.
    let mut late = obs("late", Stage::ReviewWait, -3_600, Some(600));
    late.heuristic = LAND_V2.to_string();
    late.resolved_at = Some(at(500));
    let mut late_b = late.clone();
    late_b.actual_at = Some(at(-60));
    let (mut a, mut b) = (rows.clone(), rows.clone());
    a.push(late);
    b.push(late_b);
    assert_eq!(bytes(&estimate(&a)), bytes(&estimate(&b)));

    // Estimates made at or after `as_of` are never evidence.
    rows.extend((0..60).map(|i| {
        let mut o = obs(&format!("f{i}"), Stage::ReviewWait, i * 60, Some(60));
        o.heuristic = LAND_V2.to_string();
        o
    }));
    assert_eq!(bytes(&estimate(&rows)), bytes(&reference));
}

#[test]
fn a_wrapped_simulator_base_recomputes_its_calibrated_answer() {
    // held-heron answers a held PR from its hazard simulator record; the
    // recompute must replay that record *and* apply the calibration shift.
    let wrap = IpcwWrap::new(HERON_IPCW, heron(), Calibrator::Ipcw).unwrap();
    assert!(wrap.models_hold(), "as held-heron");
    let input = held();
    let bare = heron().estimate(&input, &hold_history("rjwalters/loom"));
    let base_q = bare.quantiles_with_p90().expect("the simulator answers");
    let history = with_evidence(
        hold_history("rjwalters/loom"),
        evidence(LAND_HELD_HERON, Stage::MergeHold, 3.0),
    );
    let e = wrap.estimate(&input, &history);
    assert!(e.held_heron.is_some(), "still the simulator's answer");
    let record = e.calibration.as_ref().expect("calibrated");
    assert_eq!(record.base, LAND_HELD_HERON);
    let q = e.quantiles_with_p90().unwrap();
    assert_ne!(q, base_q, "the shift moved it");
    assert_eq!(run_explanation(&e), Some(q));
    let parsed: Explanation = serde_json::from_str(&bytes(&e)).unwrap();
    assert_eq!(run_explanation(&parsed), Some(q));
    // The unwrapped simulator answer still recomputes as before.
    assert_eq!(run_explanation(&bare), Some(base_q));
}

const REGIMED: &str = "land-v2-regimed";

/// land-v2 served brisk-petrel style (#10528): scaled by a fixed regime
/// factor, with the `regime_adjustment` record the recompute applies last.
#[derive(Debug, Clone)]
struct Regimed;

impl Heuristic for Regimed {
    fn id(&self) -> &'static str {
        REGIMED
    }
    fn kind(&self) -> Kind {
        Kind::Land
    }
    fn tier(&self) -> crate::eta::Tier {
        crate::eta::Tier::Candidate
    }
    fn models_hold(&self) -> bool {
        LandV2.models_hold()
    }
    fn estimate(&self, input: &crate::eta::EstimateInput, history: &StageSamples) -> Explanation {
        let mut e = LandV2.estimate(input, history);
        e.heuristic = REGIMED.to_string();
        let (p25, p50, p75, p90) = regime::scale(e.quantiles_with_p90().unwrap(), 1.2);
        let r = e.result.as_mut().unwrap();
        (r.p25_sec, r.p50_sec, r.p75_sec, r.p90_sec) = (p25, p50, p75, Some(p90));
        r.eta_p50_at = e.as_of + chrono::Duration::seconds(p50);
        e.regime_adjustment = Some(RegimeAdjustment {
            stage: Stage::ReviewWait.as_str().to_string(),
            factor: 1.2,
            n_recent: 50,
            half_life: 3_600,
        });
        e
    }
}

#[test]
fn a_wrapped_regime_adjusted_base_keeps_the_recompute_order() {
    // Calibration then regime factor, as `run_explanation` replays it: both
    // round, so calibrating the already-scaled quantiles would not recompute.
    let input = input_at(Stage::ReviewWait, 0, 0);
    let bare = Regimed.estimate(&input, &history_a());
    assert_eq!(run_explanation(&bare), bare.quantiles_with_p90(), "the base replays");
    let raw = LandV2
        .estimate(&input, &history_a())
        .quantiles_with_p90()
        .unwrap();

    let history = with_evidence(history_a(), evidence(REGIMED, Stage::ReviewWait, 3.0));
    for calibrator in Calibrator::ALL {
        let wrap = IpcwWrap::new("land-v2-regimed+ipcw", Regimed, calibrator).unwrap();
        let e = wrap.estimate(&input, &history);
        let record = e.calibration.as_ref().expect("calibrated");
        assert_eq!(record.base, REGIMED);
        let regime = e
            .regime_adjustment
            .as_ref()
            .expect("the regime record survives");
        assert!((regime.factor - 1.2).abs() < f64::EPSILON);
        let q = e.quantiles_with_p90().unwrap();
        assert_ne!(Some(q), bare.quantiles_with_p90(), "the shift moved it");
        assert_eq!(q, regime::scale(conformal::apply(raw, &record.shift), 1.2));
        assert_eq!(run_explanation(&e), Some(q));
        let parsed: Explanation = serde_json::from_str(&bytes(&e)).unwrap();
        assert_eq!(run_explanation(&parsed), parsed.quantiles_with_p90());
        assert_eq!(parsed.quantiles_with_p90(), Some(q));
    }
}

#[test]
fn refusals_never_twice_and_nothing_registered() {
    // Only a land base.
    assert!(IpcwWrap::new("start-v1+ipcw", StartV1, Calibrator::Ipcw).is_none());

    // An already-calibrated base is left as it answered, only re-identified.
    let input = input_at(Stage::SweepBuilder, 0, 0);
    let history = with_evidence(
        super::ready::history_ready(),
        evidence(LAND_TWIN_OTTER_B, Stage::SweepBuilder, 3.0),
    );
    let quick = LandQuickTern::default().estimate(&input, &history);
    let twice = IpcwWrap::new("quick+ipcw", LandQuickTern::default(), Calibrator::Ipcw)
        .unwrap()
        .estimate(&input, &history);
    assert_eq!(twice.calibration, quick.calibration);
    assert_eq!(twice.quantiles_with_p90(), quick.quantiles_with_p90());
    assert_eq!(twice.heuristic, "quick+ipcw");
    assert_eq!(CALIBRATED, [LAND_CALM_PLOVER, LAND_QUICK_TERN, LAND_SWIFT_TERN]);

    // Names round-trip; a wrapped id is never a registered one.
    for c in Calibrator::ALL {
        assert_eq!(Calibrator::parse(c.name()), Some(c));
    }
    assert_eq!(Calibrator::parse("conformal"), None);
    assert_eq!(Calibrator::Ipcw.wrapped_id(LAND_V2), V2_IPCW);
    assert_eq!(Calibrator::IpcwDrift.wrapped_id(LAND_V2), "land-v2+ipcw-drift");
    let registry = Registry::builtin();
    assert!(registry.ids().iter().all(|id| !id.contains('+')));
    assert!(registry.get(V2_IPCW).is_none());
}

#[test]
fn the_offline_backtest_pairs_a_wrapped_base_against_itself() {
    let history = history_a();
    let cases = backtest::cases_from_envelopes(&history_a_envelopes());
    let mut calibrated = history.clone();
    calibrated.calibration =
        backtest::calibration_from_replay(&LandV2, &history, &cases, &provenance());
    assert!(!calibrated.calibration.is_empty());
    let comparison = backtest::compare(
        &v2_ipcw(),
        &LandV2,
        &calibrated,
        &cases,
        Filter::default(),
        &provenance(),
    )
    .expect("both land");
    assert_eq!(comparison.a.heuristic, V2_IPCW);
    assert_eq!(comparison.b.heuristic, LAND_V2);
    assert_eq!(comparison.a.overall.n, comparison.b.overall.n, "the identical replay set");
    assert!(comparison.a.overall.n > 0);
    // The wrapper never refuses what its base answers.
    assert_eq!(comparison.a.overall.scored, comparison.b.overall.scored);
}
