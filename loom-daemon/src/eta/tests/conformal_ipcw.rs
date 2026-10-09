//! IPCW conformal calibration wrapper (#10524): the point-in-time leak test,
//! the censoring correction, the regime-shift and no-shift fixtures
//! (`t_cov ≤ 12 h`, never widening), the half-life fallback, and the
//! retired quick-tern / bold-lark shadows (#10949), rebuilt offline through
//! [`IpcwWrap`].

use super::{as_of, history_a, input_at};
use crate::eta::conformal::{apply, Calibration};
use crate::eta::conformal_ipcw::{self, HALF_LIFE_SEC, METHOD};
use crate::eta::conformal_wrap::{Calibrator, IpcwWrap};
use crate::eta::heuristics::{
    LandKeenWren, LandTwinOtterB, LandV2, CALIBRATION_BASES, LAND_BOLD_LARK, LAND_KEEN_WREN,
    LAND_QUICK_TERN, LAND_TWIN_OTTER_B, LAND_V2,
};
use crate::eta::recalibrate::{CalibrationObservation, OBSERVATION_SCHEMA};
use crate::eta::simulate::run_explanation;
use crate::eta::{Explanation, Heuristic, Kind, Registry, Stage};
use chrono::{DateTime, Duration, Utc};
use std::f64::consts::LN_2;

/// The retired `land-2026-10-06-quick-tern` (#10949), rebuilt offline:
/// twin-otter-b wrapped by [`conformal_ipcw::calibrate`].
fn quick_tern() -> IpcwWrap<LandTwinOtterB> {
    IpcwWrap::new(LAND_QUICK_TERN, LandTwinOtterB::default(), Calibrator::Ipcw).unwrap()
}

/// The retired `land-2026-10-06-bold-lark` (#10949), rebuilt offline:
/// keen-wren wrapped by [`conformal_ipcw::calibrate`].
fn bold_lark() -> IpcwWrap<LandKeenWren> {
    IpcwWrap::new(LAND_BOLD_LARK, LandKeenWren::new(None), Calibrator::Ipcw).unwrap()
}

pub(super) const HOUR: i64 = 3_600;
pub(super) const DAY: i64 = 86_400;

/// The base quantiles every fixture row carries.
pub(super) const BASE: (i64, i64, i64, i64) = (1_800, 3_600, 7_200, 14_400);

/// Logistic scale for which [`BASE`] is exactly calibrated: an outcome
/// `3600 · exp(S · L)`, `L` standard logistic, has quantiles 1800 / 3600 /
/// 7200 / 14400 at 25 / 50 / 75 / 90%.
pub(super) fn scale() -> f64 {
    LN_2 / 3f64.ln()
}

pub(super) fn at(secs: i64) -> DateTime<Utc> {
    as_of() + Duration::seconds(secs)
}

pub(super) fn obs(
    id: &str,
    stage: Stage,
    made: i64,
    remaining: Option<i64>,
) -> CalibrationObservation {
    CalibrationObservation {
        schema: OBSERVATION_SCHEMA.to_string(),
        estimate_id: id.to_string(),
        heuristic: LAND_TWIN_OTTER_B.to_string(),
        repo: "rjwalters/loom".to_string(),
        issue: 1,
        stage,
        as_of: at(made),
        p50_sec: BASE.1,
        actual_at: remaining.map(|r| at(made + r)),
        resolved_at: remaining.map(|r| at(made + r)),
        age_sec: Some(0),
        p25_sec: Some(BASE.0),
        p75_sec: Some(BASE.2),
        p90_sec: Some(BASE.3),
    }
}

/// One `review_wait` estimate every `step` seconds from five days before the
/// fixture instant to 30 h after it. Outcomes follow the calibrated
/// logistic, multiplied by `factor` for estimates made at or after the
/// fixture instant (the regime shift).
pub(super) fn fixture(factor: f64, step: i64) -> Vec<CalibrationObservation> {
    rows_between(factor, -5 * DAY, 30 * HOUR, step)
}

/// [`fixture`]'s rows made in `[from, to)`.
fn rows_between(factor: f64, from: i64, to: i64, step: i64) -> Vec<CalibrationObservation> {
    let mut rows = Vec::new();
    let mut made = from;
    let mut i = 0_u32;
    while made < to {
        let u = (0.5 + f64::from(i) * 0.618_033_988_7).fract();
        let f = if made >= 0 { factor } else { 1.0 };
        let remaining = (3_600.0 * f * (scale() * (u / (1.0 - u)).ln()).exp()).round();
        rows.push(obs(&format!("r{i}"), Stage::ReviewWait, made, Some((remaining as i64).max(1))));
        made += step;
        i += 1;
    }
    rows
}

/// A `review_wait` base estimate (`land-v2`'s, any answered one will do:
/// the record's shift is what is checked).
pub(super) fn base_explanation() -> Explanation {
    let e = LandV2.estimate(&input_at(Stage::ReviewWait, 0, 0), &history_a());
    assert!(e.quantiles_with_p90().is_some(), "the fixture base answers");
    e
}

/// The calibration record of `base` moved to `secs` after the fixture
/// instant, against `observations`.
fn record_at(
    base: &Explanation,
    observations: &[CalibrationObservation],
    secs: i64,
) -> Option<Calibration> {
    let mut e = base.clone();
    e.as_of = at(secs);
    conformal_ipcw::calibrate(e, observations, LAND_TWIN_OTTER_B).calibration
}

pub(super) fn logistic(x: f64) -> f64 {
    1.0 / (1.0 + (-x).exp())
}

/// For an estimate with base quantiles [`BASE`] calibrated by `record`:
/// the share of outcomes from the `factor` regime inside `[p25, p75]`, and
/// beyond `p90` (late surprise).
pub(super) fn coverage(record: &Calibration, factor: f64) -> (f64, f64) {
    let (p25, _, p75, p90) = apply(BASE, &record.shift);
    let z = |sec: i64| (sec as f64 / (3_600.0 * factor)).ln() / scale();
    (logistic(z(p75)) - logistic(z(p25)), 1.0 - logistic(z(p90)))
}

pub(super) fn bytes(e: &Explanation) -> String {
    serde_json::to_string(e).unwrap()
}

#[test]
fn quick_tern_leak_perturbing_post_as_of_outcomes_is_bit_identical() {
    let base = base_explanation();
    let mut rows = fixture(1.0, 600);
    rows.retain(|o| o.as_of < at(0));
    let calibrate = |rows: Vec<CalibrationObservation>| {
        conformal_ipcw::calibrate(base.clone(), &rows, LAND_TWIN_OTTER_B)
    };
    let reference = calibrate(rows.clone());
    assert!(reference.calibration.is_some(), "calibrated");

    // Every outcome known at or after `as_of`, moved wildly.
    let mut perturbed = rows.clone();
    for o in &mut perturbed {
        if o.resolved_at.is_some_and(|known| known >= at(0)) {
            o.actual_at = Some(at(1_000_000));
            o.resolved_at = Some(at(1_000_000));
        }
    }
    assert_eq!(bytes(&calibrate(perturbed)), bytes(&reference));

    // A landing before `as_of` known only afterwards is a censored row: its
    // true landing time must not matter.
    let mut late = obs("late", Stage::ReviewWait, -HOUR, Some(600));
    late.resolved_at = Some(at(500));
    let mut late_b = late.clone();
    late_b.actual_at = Some(at(-60));
    let mut with_a = rows.clone();
    with_a.push(late);
    let mut with_b = rows.clone();
    with_b.push(late_b);
    assert_eq!(bytes(&calibrate(with_a)), bytes(&calibrate(with_b)));

    // Estimates made at or after `as_of` are never evidence.
    let mut future = rows.clone();
    future.extend((0..60).map(|i| obs(&format!("f{i}"), Stage::ReviewWait, i * 60, Some(60))));
    assert_eq!(bytes(&calibrate(future)), bytes(&reference));

    // The same through the (offline) wrapper.
    let input = input_at(Stage::ReviewWait, 0, 0);
    let mut history = history_a();
    history.calibration = rows.clone();
    let a = quick_tern().estimate(&input, &history);
    history.calibration.extend(
        (0..60).map(|i| obs(&format!("g{i}"), Stage::ReviewWait, i * 60 + 1, Some(10 * DAY))),
    );
    let b = quick_tern().estimate(&input, &history);
    assert_eq!(bytes(&a), bytes(&b));
}

#[test]
fn ipcw_removes_the_bias_that_dropping_censored_rows_has() {
    // Stationary and calibrated: the true shift is zero at every quantile.
    let rows = fixture(1.0, 300);
    let record = record_at(&base_explanation(), &rows, 0).expect("calibrated");
    assert!(record.n_censored > 0, "{record:?}");
    for raw in record.raw_shift.to_array() {
        assert!(raw.abs() < 0.1, "IPCW is unbiased: {record:?}");
    }
    // The naive estimate, recency-weighted landings only, at the same half-life:
    // the recent slow rows are missing, so it is optimistic.
    let t = at(0);
    let mut landed: Vec<(f64, f64)> = rows
        .iter()
        .filter(|o| o.as_of < t && o.resolved_at.is_some_and(|k| k < t))
        .map(|o| {
            let age = (t - o.as_of).num_seconds() as f64;
            let w = (-LN_2 * age / HALF_LIFE_SEC as f64).exp();
            let remaining = (o.actual_at.unwrap() - o.as_of).num_seconds() as f64;
            ((remaining / BASE.3 as f64).ln(), w)
        })
        .collect();
    landed.sort_by(|a, b| a.0.total_cmp(&b.0));
    let total: f64 = landed.iter().map(|p| p.1).sum();
    let mut mass = 0.0;
    let naive_p90 = landed
        .iter()
        .find(|p| {
            mass += p.1 / total;
            mass >= 0.9
        })
        .unwrap()
        .0;
    assert!(naive_p90 < -0.2, "dropping censored rows is optimistic: {naive_p90}");
    assert!(record.raw_shift.p90 > naive_p90 + 0.15, "{record:?} vs {naive_p90}");
}

#[test]
fn the_no_shift_fixture_never_widens() {
    let base = base_explanation();
    let rows = fixture(1.0, 300);
    for half_hour in -24..=48 {
        let record = record_at(&base, &rows, half_hour * 1_800).expect("calibrated");
        let (p25, p50, p75, p90) = apply(BASE, &record.shift);
        assert!(p75 - p25 <= BASE.2 - BASE.0, "{half_hour}: widened {record:?}");
        assert!(p90 <= BASE.3, "{half_hour}: p90 widened {record:?}");
        assert!(p50 > 0);
        let (inside, late) = coverage(&record, 1.0);
        assert!((0.45..=0.55).contains(&inside), "{half_hour}: {inside}");
        assert!(late <= 0.12, "{half_hour}: late {late}");
        let ipcw = record.ipcw.as_ref().unwrap();
        assert_eq!(ipcw.half_life_sec, HALF_LIFE_SEC, "dense evidence: the short window");
    }
}

#[test]
fn regime_shift_fixtures_recover_coverage_within_twelve_hours() {
    let base = base_explanation();
    for factor in [3.0, 2.0, 0.5, 1.0 / 3.0] {
        let rows = fixture(factor, 300);
        // Every half hour from the shift to 24 h after it.
        let series: Vec<(i64, f64, f64)> = (0..=48)
            .map(|half_hour| {
                let record = record_at(&base, &rows, half_hour * 1_800).expect("calibrated");
                let (inside, late) = coverage(&record, factor);
                (half_hour * 1_800, inside, late)
            })
            .collect();
        // t_cov: from when on p25–p75 coverage stays in [40%, 60%].
        let t_cov = (0..series.len())
            .find(|&i| series[i..].iter().all(|s| (0.40..=0.60).contains(&s.1)))
            .map(|i| series[i].0);
        assert!(
            t_cov.is_some_and(|t| t <= 12 * HOUR),
            "factor {factor}: t_cov {t_cov:?}, {series:?}"
        );
        // And once recovered, the p90 is not left behind either.
        for (t, _, late) in series.iter().filter(|s| s.0 >= 12 * HOUR) {
            assert!(*late <= 0.15, "factor {factor} at {t}: late {late}");
        }
    }
}

#[test]
fn the_unshifted_base_is_not_calibrated_after_a_shift() {
    // Control for the fixture above: with no wrapper, a x3 slow-down leaves
    // the base's range far off target, so the recovery is the wrapper's doing.
    let identity = Calibration {
        shift: crate::eta::conformal::Q4 {
            p25: 0.0,
            p50: 0.0,
            p75: 0.0,
            p90: 0.0,
        },
        ..record_at(&base_explanation(), &fixture(1.0, 300), 0).unwrap()
    };
    let (inside, late) = coverage(&identity, 3.0);
    assert!(inside < 0.35 && late > 0.3, "{inside} {late}");
}

#[test]
fn sparse_evidence_doubles_the_half_life_and_thin_evidence_is_the_identity() {
    let base = base_explanation();
    // One estimate an hour: too few effective landings at 6 h.
    let sparse = fixture(1.0, HOUR);
    let record = record_at(&base, &sparse, 0).expect("a longer window answers");
    let ipcw = record.ipcw.as_ref().unwrap();
    assert!(ipcw.half_life_sec > HALF_LIFE_SEC, "{ipcw:?}");
    assert_eq!(ipcw.window_sec, (8 * ipcw.half_life_sec).min(7 * DAY));
    // Sparse but calibrated: the noise floor keeps the base as it is.
    assert_eq!(apply(BASE, &record.shift), BASE, "{record:?}");

    // A handful of landings: no cell anywhere, the base unchanged.
    let thin: Vec<CalibrationObservation> = (0..10)
        .map(|i| obs(&format!("t{i}"), Stage::ReviewWait, -HOUR * (i + 2), Some(HOUR)))
        .collect();
    let mut e = base.clone();
    e.as_of = at(0);
    let out = conformal_ipcw::calibrate(e.clone(), &thin, LAND_TWIN_OTTER_B);
    assert!(out.calibration.is_none());
    assert_eq!(bytes(&out), bytes(&e));
    // Another base's rows are not this base's evidence.
    let foreign: Vec<CalibrationObservation> = fixture(3.0, 300)
        .into_iter()
        .map(|mut o| {
            o.heuristic = LAND_V2.to_string();
            o
        })
        .collect();
    assert!(conformal_ipcw::calibrate(e, &foreign, LAND_TWIN_OTTER_B)
        .calibration
        .is_none());
}

#[test]
fn another_stages_evidence_is_used_pooled() {
    let base = base_explanation();
    let rows: Vec<CalibrationObservation> = fixture(1.0, 300)
        .into_iter()
        .map(|mut o| {
            o.stage = Stage::MergeWait;
            o
        })
        .collect();
    let record = record_at(&base, &rows, 0).expect("pooled");
    assert_eq!(record.level, "pooled");
    assert_eq!(record.stage, Stage::ReviewWait);
    let mut mixed = rows;
    mixed.extend(fixture(1.0, 300));
    assert_eq!(record_at(&base, &mixed, 0).unwrap().level, "stage");
}

#[test]
fn quick_tern_is_retired_and_offline_records_its_method_and_recomputes() {
    let registry = Registry::builtin();
    assert!(!registry.ids().contains(&LAND_QUICK_TERN), "retired (#10949)");
    assert!(!registry
        .for_kind(Kind::Land)
        .any(|h| h.id() == LAND_QUICK_TERN));
    assert_eq!(registry.current(Kind::Land, None).id(), "land-v1");
    let heuristic = quick_tern();
    assert!(heuristic.models_hold(), "as twin-otter-b");
    assert!(CALIBRATION_BASES.contains(&LAND_TWIN_OTTER_B));

    // A pre-PR stage: twin-otter-b answers from land-v2's path.
    let input = input_at(Stage::SweepBuilder, 0, 0);
    let mut history = super::ready::history_ready();
    history.calibration.clear();
    let bare = heuristic.estimate(&input, &history);
    assert_eq!(bare.heuristic, LAND_QUICK_TERN);
    let base_q = bare
        .quantiles_with_p90()
        .expect("twin-otter-b answers pre-PR");
    assert!(bare.calibration.is_none(), "no evidence, no record");

    // Evidence made over the last two days, all 3x slower than its base:
    // every quantile moves up.
    history.calibration = rows_between(1.0, -2 * DAY, 0, 300)
        .into_iter()
        .map(|mut o| {
            o.stage = Stage::SweepBuilder;
            let slow = (o.actual_at.unwrap() - o.as_of) * 3;
            o.actual_at = Some(o.as_of + slow);
            o.resolved_at = o.actual_at;
            o
        })
        .collect();
    let e = heuristic.estimate(&input, &history);
    let record = e.calibration.as_ref().expect("calibrated");
    assert_eq!(record.method, METHOD);
    assert_eq!(record.base, LAND_TWIN_OTTER_B);
    assert_eq!(record.max_daily_step, None);
    assert_eq!(record.replay_from, None);
    assert_eq!(record.ipcw.as_ref().unwrap().censoring, conformal_ipcw::CENSORING_MODEL);
    let q = e.quantiles_with_p90().unwrap();
    assert!(q.1 > base_q.1 && q.2 > base_q.2, "{q:?} vs {base_q:?}");
    assert!(q.0 <= q.1 && q.1 <= q.2 && q.2 <= q.3);
    // The explanation recomputes from itself, also after a JSON round trip.
    assert_eq!(run_explanation(&e), e.quantiles_with_p90());
    let parsed: Explanation = serde_json::from_str(&bytes(&e)).unwrap();
    assert_eq!(run_explanation(&parsed), e.quantiles_with_p90());
    assert!(bytes(&e).contains("\"ipcw\""));
    assert!(!bytes(&e).contains("max_daily_step"));
}

/// #10541 review: a heavily censored cell. 500 estimates made 100 h ago; 25
/// landed after 10 minutes, 475 are still open. Only 5% of the mass
/// resolves, so every level is unidentified, and the open rows prove that
/// 95% run past 100 h. The calibrator must not shrink the range to the
/// fast landings (it once returned 600 s for every quantile); it moves each
/// unidentified quantile up to at least the open rows' elapsed time.
#[test]
fn an_unidentified_tail_is_moved_conservatively_never_down() {
    let made = -100 * HOUR;
    let mut rows: Vec<CalibrationObservation> = (0..25)
        .map(|i| obs(&format!("fast{i}"), Stage::ReviewWait, made, Some(600)))
        .collect();
    rows.extend((0..475).map(|i| obs(&format!("open{i}"), Stage::ReviewWait, made, None)));
    let base = base_explanation();
    let out = conformal_ipcw::calibrate(base.clone(), &rows, LAND_TWIN_OTTER_B);
    let record = out.calibration.as_ref().expect("enough landings to fit");
    let ipcw = record.ipcw.as_ref().unwrap();
    assert_eq!(ipcw.unresolved, ["p25", "p50", "p75", "p90"], "{record:?}");
    assert_eq!((record.n_events, record.n_censored), (25, 475));
    for (k, shift) in record.shift.to_array().into_iter().enumerate() {
        assert!(shift >= 0.0, "quantile {k} moved down: {record:?}");
    }
    // On the rows' own base: every quantile at least the 100 h already run.
    let (p25, p50, p75, p90) = apply(BASE, &record.shift);
    for q in [p25, p50, p75, p90] {
        assert!(q >= 100 * HOUR, "{q} < 100 h: {record:?}");
    }
    // And the production estimate never drops below its base.
    let (b, o) = (base.quantiles_with_p90().unwrap(), out.quantiles_with_p90().unwrap());
    assert!(o.0 >= b.0 && o.1 >= b.1 && o.2 >= b.2 && o.3 >= b.3, "{o:?} vs {b:?}");
    assert_eq!(run_explanation(&out), out.quantiles_with_p90());
}

/// `fixture` rows re-attributed to `heuristic`.
fn rows_of(heuristic: &str, rows: Vec<CalibrationObservation>) -> Vec<CalibrationObservation> {
    rows.into_iter()
        .map(|mut o| {
            o.heuristic = heuristic.to_string();
            o
        })
        .collect()
}

/// #10524 slice 4: the wrapper over keen-wren wraps keen-wren's answer under
/// its own id, and only ever reads keen-wren's logged rows. Retired as a
/// registered shadow (#10949); kept offline.
#[test]
fn bold_lark_wraps_keen_wren_and_reads_only_its_rows() {
    let registry = Registry::builtin();
    assert!(!registry
        .for_kind(Kind::Land)
        .any(|h| h.id() == LAND_BOLD_LARK));
    assert!(CALIBRATION_BASES.contains(&LAND_KEEN_WREN));
    let heuristic = bold_lark();
    assert!(heuristic.models_hold(), "as keen-wren");

    // Pre-PR stage: keen-wren answers from the dispatch-plan path.
    let input = input_at(Stage::SweepBuilder, 0, 0);
    let mut history = super::ready::history_ready();
    history.calibration.clear();
    let bare = heuristic.estimate(&input, &history);
    assert_eq!(bare.heuristic, LAND_BOLD_LARK);
    let base_q = bare.quantiles_with_p90().expect("keen-wren answers pre-PR");
    assert!(bare.calibration.is_none(), "no evidence, no record");

    let slow = |heuristic: &str| -> Vec<CalibrationObservation> {
        rows_of(
            heuristic,
            rows_between(1.0, -2 * DAY, 0, 300)
                .into_iter()
                .map(|mut o| {
                    o.stage = Stage::SweepBuilder;
                    let slow = (o.actual_at.unwrap() - o.as_of) * 3;
                    o.actual_at = Some(o.as_of + slow);
                    o.resolved_at = o.actual_at;
                    o
                })
                .collect(),
        )
    };

    // Another base's rows are not evidence.
    history.calibration = slow(LAND_TWIN_OTTER_B);
    assert!(heuristic.estimate(&input, &history).calibration.is_none());

    history.calibration = slow(LAND_KEEN_WREN);
    let e = heuristic.estimate(&input, &history);
    let record = e.calibration.as_ref().expect("calibrated");
    assert_eq!(record.method, METHOD);
    assert_eq!(record.base, LAND_KEEN_WREN);
    assert_eq!(record.ipcw.as_ref().unwrap().censoring, conformal_ipcw::CENSORING_MODEL);
    let q = e.quantiles_with_p90().unwrap();
    assert!(q.1 > base_q.1 && q.2 > base_q.2, "{q:?} vs {base_q:?}");
    assert_eq!(run_explanation(&e), e.quantiles_with_p90());
}

/// The leak test, through bold-lark: outcomes known at or after `as_of`, and
/// estimates made at or after it, cannot enter the calibration set.
#[test]
fn bold_lark_leak_post_as_of_outcomes_cannot_enter_the_calibration_set() {
    let mut rows = rows_of(LAND_KEEN_WREN, fixture(1.0, 600));
    rows.retain(|o| o.as_of < at(0));
    // A pre-PR stage: keen-wren answers without an `eta-fit/v2` file, and
    // the review_wait evidence is pooled.
    let input = input_at(Stage::SweepBuilder, 0, 0);
    let mut history = super::ready::history_ready();
    history.calibration = rows.clone();
    let a = bold_lark().estimate(&input, &history);
    assert!(a.calibration.is_some(), "the leak test exercises calibration");
    // Post-as_of estimates with extreme outcomes, and a pre-as_of estimate
    // whose landing is only known afterwards.
    history.calibration.extend(rows_of(
        LAND_KEEN_WREN,
        (0..60)
            .map(|i| obs(&format!("g{i}"), Stage::ReviewWait, i * 60 + 1, Some(10 * DAY)))
            .collect(),
    ));
    let mut late = obs("late", Stage::ReviewWait, -HOUR, Some(600));
    late.heuristic = LAND_KEEN_WREN.to_string();
    late.resolved_at = Some(at(500));
    let mut late_b = late.clone();
    late_b.actual_at = Some(at(-60));
    let mut with_a = history.clone();
    with_a.calibration.push(late);
    let mut with_b = history.clone();
    with_b.calibration.push(late_b);
    assert_eq!(
        bytes(&bold_lark().estimate(&input, &with_a)),
        bytes(&bold_lark().estimate(&input, &with_b)),
    );
    let b = bold_lark().estimate(&input, &history);
    assert_eq!(bytes(&a), bytes(&b));
}
