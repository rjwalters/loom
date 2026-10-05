//! `eta::fit` unit tests (#10221): the hand-ported numerics, the feature
//! transform, Kaplan–Meier and its downsampling, gating, the fixture shapes,
//! and the coefficient file's writer, reader and id.

use super::TWIN_OTTER_PARITY;
use crate::eta::fit::math::{cholesky_solve, erfc, norm_cdf, norm_logpdf, norm_logsf, probit};
use crate::eta::fit::paths::{kaplan_meier, next_steps};
use crate::eta::fit::{
    self, clock, downsample_km, model_features, AftFit, CoefficientFile, DwellEnd, DwellRow,
    FitMeta, FitStage, FitWindow, Fitter, HazardFit, MergeLabel, ModelInputs, NextStep, PathStats,
    SkipReason, TrainingRow, FEATURES,
};
use chrono::{TimeZone, Utc};
use serde_json::Value;

fn rel_close(got: f64, want: f64, tol: f64) -> bool {
    (got - want).abs() <= tol * want.abs()
}

#[test]
fn math_matches_scipy_reference_values() {
    let cases: [(&str, f64, f64, f64); 8] = [
        ("probit(1e-12)", probit(1e-12), -7.034483825301131, 1e-9),
        ("probit(0.975)", probit(0.975), 1.959963984540054, 1e-12),
        ("probit(0.25)", probit(0.25), -0.6744897501960817, 1e-12),
        ("norm_logsf(-3)", norm_logsf(-3.0), -0.0013508099647481923, 1e-12),
        ("norm_logsf(2.71)", norm_logsf(2.71), -5.694576854693982, 1e-12),
        ("norm_logsf(10)", norm_logsf(10.0), -53.23128515051248, 1e-12),
        ("norm_logsf(30)", norm_logsf(30.0), -454.3212439563432, 1e-12),
        ("norm_cdf(0.3)", norm_cdf(0.3), 0.6179114221889526, 1e-12),
    ];
    for (name, got, want, tol) in cases {
        assert!(rel_close(got, want, tol), "{name} = {got:e}, want {want:e}");
    }
}

#[test]
fn erfc_matches_libm_in_every_range() {
    // Reference values: CPython `math.erfc` (platform libm), one per fdlibm
    // branch and both signs.
    let cases = [
        (-1.5, 1.9661051464753108),
        (-0.5, 1.5204998778130465),
        (0.1, 0.8875370839817152),
        (0.3, 0.6713732405408727),
        (1.0, 0.15729920705028516),
        (1.1, 0.11979493042591825),
        (2.0, 0.0046777349810472645),
        (3.5, 7.43098372341413e-07),
        (5.0, 1.537459794428035e-12),
        (10.0, 2.0884875837625446e-45),
        (20.0, 5.395865611607901e-176),
    ];
    for (x, want) in cases {
        let got = erfc(x);
        assert!(rel_close(got, want, 1e-14), "erfc({x}) = {got:e}, want {want:e}");
    }
    assert_eq!(erfc(0.0), 1.0);
    assert_eq!(erfc(f64::INFINITY), 0.0);
    assert_eq!(erfc(f64::NEG_INFINITY), 2.0);
    assert_eq!(erfc(-30.0), 2.0);
    assert!(erfc(f64::NAN).is_nan());
}

#[test]
fn probit_edges_and_round_trip() {
    assert_eq!(probit(0.5), 0.0);
    assert_eq!(probit(0.0), f64::NEG_INFINITY);
    assert_eq!(probit(1.0), f64::INFINITY);
    assert!(probit(f64::NAN).is_nan());
    for x in [-6.0, -2.5, -0.4, 0.1, 1.3, 3.0, 5.5] {
        assert!((probit(norm_cdf(x)) - x).abs() < 1e-9, "probit(Φ({x}))");
    }
    // Symmetric to the bit about 0.5, in the central and the tail branch
    // (dyadic p, so 1 − p is exact).
    assert_eq!(probit(0.25), -probit(0.75));
    assert_eq!(probit(0.0625), -probit(0.9375));
}

#[test]
fn norm_logsf_is_finite_and_monotone_on_minus_40_to_40() {
    let mut prev = f64::INFINITY;
    for i in 0..=8000 {
        let r = -40.0 + f64::from(i) * 0.01;
        let v = norm_logsf(r);
        assert!(v.is_finite(), "norm_logsf({r}) = {v}");
        assert!(v <= prev, "norm_logsf not monotone at {r}: {v} > {prev}");
        prev = v;
    }
    // The asymptotic branch above 37 meets the erfc branch.
    let below = norm_logsf(37.0);
    let above = norm_logsf(37.0 + 1e-9);
    assert!((below - above).abs() < 1e-6, "{below} vs {above}");
    assert!((norm_logpdf(0.0) + 0.918_938_533_204_672_7).abs() < 1e-15);
}

#[test]
fn cholesky_solves_and_refuses_indefinite() {
    let a = [4.0, 2.0, 0.6, 2.0, 5.0, 1.5, 0.6, 1.5, 3.0];
    let x = [1.0, -2.0, 0.5];
    let b: Vec<f64> = (0..3)
        .map(|i| (0..3).map(|j| a[i * 3 + j] * x[j]).sum())
        .collect();
    let got = cholesky_solve(&a, &b).unwrap();
    for (g, w) in got.iter().zip(x) {
        assert!((g - w).abs() < 1e-12);
    }
    assert!(cholesky_solve(&[1.0, 2.0, 2.0, 1.0], &[1.0, 1.0]).is_none());
}

#[test]
fn model_features_follow_the_pinned_transform() {
    let m = ModelInputs {
        age_h: 3.0,
        ahead: 2,
        since_merge_h: 500.0,
        hour_utc: 6.0,
        weekend: true,
        rework: 2,
        starred: true,
        ..ModelInputs::default()
    };
    let x = model_features(&m);
    assert_eq!(x.len(), FEATURES.len());
    assert_eq!(x[0], 3.0_f64.ln_1p());
    assert_eq!(x[1], 2.0_f64.ln_1p());
    assert_eq!(x[2], 0.0);
    // since_merge_h = 500 maps to ln_1p(168).
    assert_eq!(x[8], 168.0_f64.ln_1p());
    assert!((x[10] - 1.0).abs() < 1e-15, "hour_sin at 06:00");
    assert!(x[11].abs() < 1e-15, "hour_cos at 06:00");
    assert_eq!(x[12], 1.0);
    // rework is raw, not logged.
    assert_eq!(x[13], 2.0);
    assert_eq!(&x[14..], &[0.0, 0.0, 1.0, 0.0, 0.0, 0.0]);
}

#[test]
fn clock_reads_utc_hour_and_weekend() {
    // 2026-10-03 is a Saturday.
    let sat = Utc.with_ymd_and_hms(2026, 10, 3, 22, 30, 0).unwrap();
    assert_eq!(clock(sat), (22.5, true));
    let mon = Utc.with_ymd_and_hms(2026, 10, 5, 0, 0, 0).unwrap();
    assert_eq!(clock(mon), (0.0, false));
}

#[test]
fn fixture_objects_deserialize_into_the_owned_types() {
    let fx: Value = serde_json::from_str(TWIN_OTTER_PARITY).unwrap();
    let features: Vec<String> =
        serde_json::from_value(fx["generator"]["features"].clone()).unwrap();
    assert_eq!(features, FEATURES);
    let stages: Vec<FitStage> = serde_json::from_value(fx["generator"]["stages"].clone()).unwrap();
    assert_eq!(stages, FitStage::ALL);
    for stage in FitStage::ALL {
        let h: HazardFit =
            serde_json::from_value(fx["fitted"]["hazard"][stage.as_str()].clone()).unwrap();
        assert_eq!((h.mu.len(), h.sd.len(), h.coef.len()), (20, 20, 20));
        assert!(!h.converged, "file-only fields default");
    }
    let aft: AftFit = serde_json::from_value(fx["fitted"]["aft"].clone()).unwrap();
    assert_eq!(aft.stages, FitStage::ALL);
    assert_eq!((aft.beta.len(), aft.log_sigma.len()), (24, 4));
    assert!(aft.converged);
    let paths: PathStats = serde_json::from_value(fx["evaluation"]["path_stats"].clone()).unwrap();
    assert_eq!(paths.km.len(), 4);
    assert_eq!(paths.next[&FitStage::MergeWait][&NextStep::Merged], 0.9, "next-step keys parse");
}

fn dwell(entry_h: f64, dwell_h: f64, end: DwellEnd) -> DwellRow {
    DwellRow {
        stage: FitStage::ReviewWait,
        entry_h,
        dwell_h,
        end,
    }
}

#[test]
fn kaplan_meier_handles_delayed_entry_and_censoring() {
    let rows = [
        dwell(0.0, 1.0, DwellEnd::Next(FitStage::MergeWait)),
        dwell(0.0, 2.0, DwellEnd::Censored),
        dwell(0.5, 3.0, DwellEnd::Merged),
        dwell(0.0, 3.0, DwellEnd::Next(FitStage::DoctorWait)),
        dwell(2.5, 4.0, DwellEnd::Merged),
    ];
    let (t, s) = kaplan_meier(&rows);
    assert_eq!(t, [0.0, 1.0, 3.0, 4.0]);
    assert_eq!(s.len(), 4);
    for (got, want) in s.iter().zip([1.0, 0.75, 0.25, 0.0]) {
        assert!((got - want).abs() < 1e-15, "{s:?}");
    }

    // A last row censored leaves the curve above 0.
    let tail = [
        dwell(0.0, 1.0, DwellEnd::Merged),
        dwell(0.0, 2.0, DwellEnd::Merged),
        dwell(0.0, 5.0, DwellEnd::Censored),
    ];
    let (_, s) = kaplan_meier(&tail);
    assert!(*s.last().unwrap() > 0.0, "{s:?}");
    // A close is censored, not an event.
    let (t, _) = kaplan_meier(&[dwell(0.0, 1.0, DwellEnd::Closed)]);
    assert_eq!(t, [0.0]);
}

#[test]
fn next_excludes_closes_and_sums_to_one() {
    let rows = [
        dwell(0.0, 1.0, DwellEnd::Next(FitStage::MergeWait)),
        dwell(0.0, 1.0, DwellEnd::Next(FitStage::MergeWait)),
        dwell(0.0, 1.0, DwellEnd::Next(FitStage::DoctorWait)),
        dwell(0.0, 1.0, DwellEnd::Merged),
        dwell(0.0, 1.0, DwellEnd::Closed),
        dwell(0.0, 1.0, DwellEnd::Censored),
    ];
    let next = next_steps(&rows);
    assert_eq!(next.len(), 3);
    assert_eq!(next[&NextStep::MergeWait], 0.5);
    assert!((next.values().sum::<f64>() - 1.0).abs() < 1e-15);
}

#[test]
fn downsample_km_keeps_the_hand_example_indices() {
    let t = [0.0, 1.0, 2.0, 3.0, 4.0, 5.0];
    let s = [1.0, 0.9, 0.75, 0.5, 0.2, 0.1];
    let (dt, ds) = downsample_km(&t, &s, 4);
    assert_eq!(dt, [0.0, 3.0, 4.0, 5.0]);
    assert_eq!(ds, [1.0, 0.5, 0.2, 0.1]);
    // At or below the cap, unchanged.
    let (dt, ds) = downsample_km(&t, &s, 6);
    assert_eq!((dt.as_slice(), ds.as_slice()), (&t[..], &s[..]));
}

#[test]
fn downsample_km_bounds_the_error_on_a_2001_point_curve() {
    let mut rng = crate::eta::simulate::SplitMix64::new(10221);
    let mut t = vec![0.0];
    let mut s = vec![1.0];
    for i in 1..=2000 {
        t.push(f64::from(i) * 0.1);
        let prev = *s.last().unwrap();
        s.push(prev * (1.0 - 0.002 * rng.next_f64()));
    }
    let n = s.len() - 1;
    let (dt, ds) = downsample_km(&t, &s, 64);
    assert_eq!(dt.len(), 64);
    assert_eq!((dt[0], ds[0]), (0.0, 1.0));
    assert_eq!((dt[63], ds[63]), (t[n], s[n]));
    let bound = (1.0 - s[n]) / 63.0;
    // The stored step function at every original time.
    for (ti, si) in t.iter().zip(&s) {
        let k = dt.partition_point(|x| x <= ti) - 1;
        let err = ds[k] - si;
        assert!((0.0..bound).contains(&err), "error {err} at t={ti} (bound {bound})");
    }
    // 64 or fewer points: stored unchanged.
    let (dt, ds) = downsample_km(&t[..64], &s[..64], 64);
    assert_eq!((dt.as_slice(), ds.as_slice()), (&t[..64], &s[..64]));
}

fn meta() -> FitMeta {
    let as_of = Utc.with_ymd_and_hms(2026, 10, 4, 0, 0, 0).unwrap();
    FitMeta {
        as_of,
        window: FitWindow::standard(as_of),
        fitter: Fitter {
            version: "0.19.676".to_string(),
            revision: "0123456789abcdef0123456789abcdef01234567".to_string(),
        },
    }
}

/// `n` rows of `stage`, the first `exits` of them exiting and merging.
fn rows(stage: FitStage, n: usize, exits: usize) -> Vec<TrainingRow> {
    (0..n)
        .map(|i| TrainingRow {
            starred_any: None,
            star_source: None,
            stage,
            group: format!("g#{i}"),
            inputs: ModelInputs {
                age_h: (i % 37) as f64 * 0.7,
                ahead: (i % 5) as u32,
                hour_utc: (i % 24) as f64,
                op_hold: i % 3 == 0,
                ..ModelInputs::default()
            },
            exit: Some(i < exits),
            merge: MergeLabel {
                dur_h: 1.0 + (i % 11) as f64,
                merged: i % 4 != 0,
            },
        })
        .collect()
}

#[test]
fn hazard_gates_skip_thin_stages_with_the_right_reason() {
    let mut all = rows(FitStage::ReviewWait, 199, 50);
    all.extend(rows(FitStage::DoctorWait, 400, 19));
    all.extend(rows(FitStage::MergeWait, 400, 20));
    // Unlabelled rows never count toward the hazard's gates.
    let mut unlabelled = rows(FitStage::MergeHold, 300, 0);
    for r in &mut unlabelled {
        r.exit = None;
    }
    all.extend(unlabelled);
    let file = fit::fit(&meta(), &all, &[]);

    let skip = |s: FitStage| file.hazard_skipped[&s];
    assert_eq!(skip(FitStage::ReviewWait).reason, SkipReason::BelowMinRows);
    assert_eq!(skip(FitStage::ReviewWait).rows, 199);
    assert_eq!(skip(FitStage::DoctorWait).reason, SkipReason::BelowMinExits);
    assert_eq!(skip(FitStage::DoctorWait).exits, 19);
    assert_eq!(skip(FitStage::MergeHold).reason, SkipReason::BelowMinRows);
    assert_eq!(skip(FitStage::MergeHold).rows, 0);
    assert_eq!(file.hazard.keys().copied().collect::<Vec<_>>(), [FitStage::MergeWait]);
    assert!(file.hazard[&FitStage::MergeWait].converged);
}

#[test]
fn a_stage_with_no_rows_is_skipped_without_nan() {
    let file = fit::fit(&meta(), &rows(FitStage::MergeWait, 300, 40), &[]);
    for stage in [
        FitStage::ReviewWait,
        FitStage::DoctorWait,
        FitStage::MergeHold,
    ] {
        let skip = file.hazard_skipped[&stage];
        assert_eq!((skip.reason, skip.rows, skip.exits), (SkipReason::BelowMinRows, 0, 0));
        assert!(!file.age_p95_sec.contains_key(&stage));
    }
    let text = fit::to_json(&file);
    assert!(!text.contains("null") && !text.contains("NaN"), "{text}");
}

#[test]
fn aft_gate_drops_thin_stages_from_stages_beta_and_log_sigma() {
    let mut all = rows(FitStage::ReviewWait, 300, 30);
    all.extend(rows(FitStage::DoctorWait, 199, 30));
    // Enough rows, but only 19 merge events.
    let mut few_merges = rows(FitStage::MergeWait, 300, 30);
    for (i, r) in few_merges.iter_mut().enumerate() {
        r.merge.merged = i < 19;
    }
    all.extend(few_merges);
    let aft = fit::fit(&meta(), &all, &[]).aft.unwrap();
    assert_eq!(aft.stages, [FitStage::ReviewWait]);
    assert_eq!((aft.beta.len(), aft.log_sigma.len()), (21, 1));
    assert_eq!(aft.rows, 300);
    assert!(aft.converged);

    let none = fit::fit(&meta(), &rows(FitStage::ReviewWait, 199, 30), &[]);
    assert!(none.aft.is_none());
}

#[test]
fn writer_reader_and_load_latest_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let base = fit::fit(&meta(), &rows(FitStage::MergeWait, 300, 40), &[]);
    assert_eq!(base.id.len(), 16);
    assert_eq!(base.id, base.derive_id());

    let older = {
        let mut m = meta();
        m.as_of -= chrono::Duration::days(1);
        fit::fit(&m, &rows(FitStage::MergeWait, 300, 40), &[])
    };
    let fit_dir = fit::fit_dir(root);
    assert!(fit_dir.ends_with(".loom/state/eta/fit"));
    for f in [&base, &older] {
        fit::write(&fit_dir.join(fit::path_for(f.as_of)), f).unwrap();
    }
    assert_eq!(fit::path_for(base.as_of), "fit-20261004T000000Z.json");
    let path = fit_dir.join(fit::path_for(base.as_of));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), fit::to_json(&base));
    let back = fit::read(&path).unwrap();
    assert_eq!(back.id, base.id);

    // Strictly before: a fit at exactly `before` is not yet usable.
    let latest = fit::load_latest(root, base.as_of + chrono::Duration::seconds(1)).unwrap();
    assert_eq!(latest.as_of, base.as_of);
    let latest = fit::load_latest(root, base.as_of).unwrap();
    assert_eq!(latest.as_of, older.as_of);
    assert!(fit::load_latest(root, older.as_of).is_none());

    // An unknown schema is refused, never half-read.
    let mut alien: Value = serde_json::from_str(&fit::to_json(&base)).unwrap();
    alien["schema"] = Value::from("eta-fit/v2");
    std::fs::write(fit_dir.join("fit-alien.json"), alien.to_string()).unwrap();
    assert!(fit::read(&fit_dir.join("fit-alien.json")).is_none());
}

#[test]
fn file_shape_is_canonical() {
    let file: CoefficientFile = fit::fit(&meta(), &rows(FitStage::MergeWait, 300, 40), &[]);
    let v: Value = serde_json::from_str(&fit::to_json(&file)).unwrap();
    let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "schema",
            "id",
            "as_of",
            "window",
            "fitter",
            "settings",
            "features",
            "hazard",
            "hazard_skipped",
            "aft",
            "path_stats",
            "age_p95_sec"
        ]
    );
    assert_eq!(v["schema"], "eta-fit/v1");
    assert_eq!(v["as_of"], "2026-10-04T00:00:00Z");
    assert_eq!(v["window"]["start"], "2026-09-20T00:00:00Z");
    assert_eq!(v["settings"]["min_dur_h"], 1.0 / 60.0);
    assert_eq!(v["settings"]["km_max_points"], 64);
    assert!(fit::to_json(&file).ends_with("}\n"));
}
