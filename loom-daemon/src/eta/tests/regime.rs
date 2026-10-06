//! Latent-regime adjustment and drift check (#10528): the adaptation-time
//! harness. A deterministic synthetic stream of scored `review_wait`
//! outcomes, 10 an hour, with log-normal-ish noise; the shift fixture doubles
//! every review time at `T` with no covariate change. Measures
//! `t_p50` (<= 6 h), `t_cov` (<= 12 h) and `t_alarm` (<= 3 h).

use crate::eta::explanation::RegimeAdjustment;
use crate::eta::recency::{
    resolve_half_life, resolve_half_life_adaptive, DEFAULT_HALF_LIFE_SEC, DRIFTED_DIVISOR,
    STABLE_MULTIPLIER,
};
use crate::eta::regime::{adjust, drift, residuals, DriftState, Residual};
use crate::eta::{Stage, MIN_SAMPLES};
use chrono::{DateTime, Duration, TimeZone, Utc};

const PER_HOUR: i64 = 10;
const SIGMA: f64 = 0.3;
const BASE_P50: f64 = 7_200.0;
const STAGE: Stage = Stage::ReviewWait;
/// Hours of pre-shift history before `T`.
const PRE_HOURS: i64 = 72;

fn epoch() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).unwrap()
}

/// Deterministic standard-normal-ish draw (sum of 12 uniforms, minus 6).
fn normal(state: &mut u64) -> f64 {
    let mut s = 0.0;
    for _ in 0..12 {
        *state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        s += (*state >> 11) as f64 / (1u64 << 53) as f64;
    }
    s - 6.0
}

/// One outcome: `(known_at, log_ratio)` against the unadjusted p50.
struct Stream {
    rows: Vec<Residual>,
    /// `T`, in hours after the epoch; `None` for the no-shift fixture.
    shift_at_h: Option<i64>,
    total_hours: i64,
}

fn stream(shift_at_h: Option<i64>, post_hours: i64, seed: u64) -> Stream {
    let mut rng = seed;
    let total_hours = PRE_HOURS + post_hours;
    let mut rows = Vec::new();
    for h in 0..total_hours {
        for k in 0..PER_HOUR {
            let known_at = epoch() + Duration::minutes(h * 60 + k * (60 / PER_HOUR) + 1);
            let shifted = shift_at_h.is_some_and(|t| h >= t);
            let log_ratio = SIGMA * normal(&mut rng) + if shifted { 2.0_f64.ln() } else { 0.0 };
            rows.push(Residual {
                stage: STAGE,
                known_at,
                log_ratio,
            });
        }
    }
    Stream {
        rows,
        shift_at_h,
        total_hours,
    }
}

#[derive(Debug, Default)]
struct Times {
    t_p50: Option<i64>,
    t_cov: Option<i64>,
    t_alarm: Option<i64>,
}

/// Replay the stream hour by hour, prequentially (each decision sees only
/// outcomes known before it), and report the adaptation times in hours after
/// `T`.
fn measure(s: &Stream) -> Times {
    let mut out = Times::default();
    let Some(t) = s.shift_at_h else { return out };
    let mut in_band_since: Option<i64> = None;
    for h in t..s.total_hours {
        let as_of = epoch() + Duration::hours(h + 1);
        let after = h + 1 - t;
        let a = adjust(&s.rows, STAGE, as_of);
        // Served p50 within 25% of the new truth (2x the old p50).
        if out.t_p50.is_none()
            && (BASE_P50 * a.factor - 2.0 * BASE_P50).abs() <= 0.25 * 2.0 * BASE_P50
        {
            out.t_p50 = Some(after);
        }
        if out.t_alarm.is_none() && drift(&s.rows, STAGE, as_of).drifted {
            out.t_alarm = Some(after);
        }
        // Coverage of the adjusted p25-p75 over the trailing 6 h of
        // outcomes, each scored against the factor known just before it.
        let from = as_of - Duration::hours(6);
        let (mut hit, mut n) = (0, 0);
        for r in s
            .rows
            .iter()
            .filter(|r| r.known_at >= from && r.known_at < as_of)
        {
            let f = adjust(&s.rows, STAGE, r.known_at).factor.ln();
            let z = (r.log_ratio - f) / SIGMA;
            n += 1;
            if z.abs() <= 0.6745 {
                hit += 1;
            }
        }
        let cov = f64::from(hit) / f64::from(n);
        if (0.4..=0.6).contains(&cov) {
            in_band_since.get_or_insert(after);
        } else {
            in_band_since = None;
        }
    }
    out.t_cov = in_band_since;
    out
}

#[test]
fn shift_fixture_adapts_within_the_budgets() {
    for seed in [1_u64, 7, 42] {
        let s = stream(Some(PRE_HOURS), 24, seed);
        let t = measure(&s);
        assert!(t.t_p50.is_some_and(|h| h <= 6), "seed {seed}: {t:?}");
        assert!(t.t_alarm.is_some_and(|h| h <= 3), "seed {seed}: {t:?}");
        assert!(t.t_cov.is_some_and(|h| h <= 12), "seed {seed}: {t:?}");
    }
}

#[test]
fn no_shift_fixture_never_alarms_and_the_adjustment_is_identity() {
    for seed in [1_u64, 7, 42] {
        let s = stream(None, 96, seed);
        // Every hour after the first day, over the whole stream.
        for h in 24..s.total_hours {
            let as_of = epoch() + Duration::hours(h + 1);
            let d = drift(&s.rows, STAGE, as_of);
            assert!(!d.drifted, "seed {seed} hour {h}: {d:?}");
            let a = adjust(&s.rows, STAGE, as_of);
            assert!(a.is_identity(), "seed {seed} hour {h}: {a:?}");
        }
    }
}

#[test]
fn adjustment_and_drift_ignore_everything_at_or_after_as_of() {
    let s = stream(Some(PRE_HOURS), 12, 3);
    let as_of = epoch() + Duration::hours(PRE_HOURS + 4);
    let reference = (adjust(&s.rows, STAGE, as_of), drift(&s.rows, STAGE, as_of));
    let mut perturbed = s.rows.clone();
    for r in &mut perturbed {
        if r.known_at >= as_of {
            r.log_ratio = 9.0;
        }
    }
    assert_eq!(reference, (adjust(&perturbed, STAGE, as_of), drift(&perturbed, STAGE, as_of)));
}

#[test]
fn explanation_record_is_absent_for_identity_and_present_for_a_shift() {
    let s = stream(Some(PRE_HOURS), 8, 5);
    let calm = adjust(&s.rows, STAGE, epoch() + Duration::hours(PRE_HOURS - 1));
    assert!(RegimeAdjustment::of(&calm).is_none());
    let moved = adjust(&s.rows, STAGE, epoch() + Duration::hours(PRE_HOURS + 6));
    let rec = RegimeAdjustment::of(&moved).expect("adjusted");
    let json = serde_json::to_value(&rec).unwrap();
    for key in ["stage", "factor", "n_recent", "half_life"] {
        assert!(json.get(key).is_some(), "{key}");
    }
    assert_eq!(json["stage"], "review_wait");
}

#[test]
fn residuals_come_from_resolved_calibration_rows_only() {
    use crate::eta::recalibrate::{CalibrationObservation, OBSERVATION_SCHEMA};
    let row = |remaining: Option<i64>| CalibrationObservation {
        schema: OBSERVATION_SCHEMA.to_string(),
        estimate_id: "e".into(),
        heuristic: "land-v2".into(),
        repo: "r/r".into(),
        issue: 1,
        stage: STAGE,
        as_of: epoch(),
        p50_sec: 3_600,
        actual_at: remaining.map(|r| epoch() + Duration::seconds(r)),
        resolved_at: remaining.map(|r| epoch() + Duration::seconds(r)),
        age_sec: None,
        p25_sec: None,
        p75_sec: None,
        p90_sec: None,
    };
    let rs = residuals(&[row(Some(7_200)), row(None), row(Some(0))]);
    assert_eq!(rs.len(), 1);
    assert!((rs[0].log_ratio - 2.0_f64.ln()).abs() < 1e-12);
}

#[test]
fn adaptive_half_life_shortens_on_drift_lengthens_when_stable_and_keeps_the_floor() {
    // Plenty of recent samples: the floor does not bind.
    let ages: Vec<i64> = (0..200).map(|i| i * 600).collect();
    let base = DEFAULT_HALF_LIFE_SEC;
    let unknown = resolve_half_life_adaptive(&ages, base, MIN_SAMPLES, DriftState::Unknown);
    assert_eq!(unknown, resolve_half_life(&ages, base, MIN_SAMPLES));
    let drifted =
        resolve_half_life_adaptive(&ages, base, MIN_SAMPLES, DriftState::Drifted).unwrap();
    let stable = resolve_half_life_adaptive(&ages, base, MIN_SAMPLES, DriftState::Stable).unwrap();
    assert_eq!(drifted, base / DRIFTED_DIVISOR);
    assert_eq!(stable, base * STABLE_MULTIPLIER);
    assert!(drifted < unknown.unwrap() && unknown.unwrap() < stable);

    // Few, old samples: a drifted half-life is widened back to the floor.
    let old: Vec<i64> = (0..MIN_SAMPLES as i64)
        .map(|i| 3 * 86_400 + i * 3_600)
        .collect();
    let h = resolve_half_life_adaptive(&old, base, MIN_SAMPLES, DriftState::Drifted);
    match h {
        Some(h) => {
            let ess = crate::eta::recency::effective_n(
                old.iter().map(|&a| crate::eta::recency::weight(a, Some(h))),
            );
            assert!(ess >= MIN_SAMPLES as f64, "{h} -> {ess}");
        }
        None => {}
    }
}
