//! Offline point-in-time evaluation (#10193): the temporal-separation
//! guarantees, mechanically.

use super::{history_a, input_at};
use crate::eta::explanation::{EstimateResult, Explanation};
use crate::eta::heuristics::LandV1;
use crate::eta::offline::candidate::Settings;
use crate::eta::offline::dataset::{
    assert_point_in_time, Label, LandingEvent, Logged, LoggedLine, Row,
};
use crate::eta::offline::evaluate::{
    bootstrap, fold_rows, plan, run, score_row, FoldRole, IssueSums, Protocol,
};
use crate::eta::offline::model::{KmModel, KmSettings};
use crate::eta::offline::qr::{QrModel, QrSettings};
use crate::eta::score::{pinball, score, EstimateSummary, OutcomeKind};
use crate::eta::simulate::SplitMix64;
use crate::eta::{Heuristic, Kind, Stage, Subject};
use crate::telemetry::kinds::eta::EtaOutcomeRecord;
use chrono::{DateTime, Duration, TimeZone, Utc};

const REPO: &str = "rjwalters/loom";

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap()
}

fn base() -> Explanation {
    LandV1.estimate(&input_at(Stage::ReviewWait, 0, 0), &history_a())
}

/// A `land` estimate of `issue` at `as_of`, `age` into `stage`, by
/// `heuristic`, answering `q` (or refusing on `None`).
fn estimate(
    issue: u32,
    as_of: DateTime<Utc>,
    stage: Stage,
    age: i64,
    heuristic: &str,
    q: Option<(i64, i64, i64)>,
) -> Explanation {
    let mut e = base();
    e.heuristic = heuristic.to_string();
    e.kind = Kind::Land;
    e.as_of = as_of;
    e.subject = Subject::new(REPO, Some(1), issue);
    e.estimate_id = crate::eta::estimate_id(&e.subject, Kind::Land, heuristic, as_of);
    let current = e.current_stage.as_mut().unwrap();
    current.stage = stage;
    current.age_sec = age;
    e.result = q.map(|(p25, p50, p75)| EstimateResult {
        p25_sec: p25,
        p50_sec: p50,
        p75_sec: p75,
        p90_sec: None,
        eta_p50_at: as_of + Duration::seconds(p50),
        samples_min: 9,
        stage_marks: Vec::new(),
        tail_extrapolated: false,
    });
    e
}

fn estimate_line(e: &Explanation, observed_at: DateTime<Utc>) -> LoggedLine {
    LoggedLine {
        observed_at,
        event: "eta.estimate".to_string(),
        body: serde_json::to_value(e).unwrap(),
    }
}

/// An outcome record for `e`, resolved `outcome` at `actual_at`.
fn outcome_line(
    e: &Explanation,
    outcome: OutcomeKind,
    actual_at: DateTime<Utc>,
    observed_at: DateTime<Utc>,
) -> LoggedLine {
    let summary = EstimateSummary::of(e);
    let record = EtaOutcomeRecord {
        score: score(&summary, outcome, actual_at, &[]),
        loom: e.loom.clone(),
        estimate: summary,
        outcome_source: "bus".to_string(),
        outcome_resolution_sec: None,
        result: None,
    };
    LoggedLine {
        observed_at,
        event: "eta.outcome".to_string(),
        body: serde_json::to_value(&record).unwrap(),
    }
}

/// A synthetic fleet: `issues` items entering `review_wait` over
/// `[t0, t0 + span_days)`, estimated every two hours by `land-v2` (a
/// narrow, optimistic interval) until each lands (or is abandoned), with
/// the outcome logged after a lag.
fn fleet(issues: u32, span_days: i64) -> Vec<LoggedLine> {
    let mut rng = SplitMix64::new(7);
    let mut lines = Vec::new();
    for issue in 1..=issues {
        let start = t0() + Duration::seconds((rng.next_u64() % (span_days as u64 * 86_400)) as i64);
        // A conflicted PR takes three times as long: a signal a
        // feature-based model can find and `land-v2` ignores.
        let conflict = issue % 3 == 0;
        let scale = if conflict { 3.0 } else { 1.0 };
        let duration = 1_800 + (rng.next_f64() * rng.next_f64() * 20.0 * 3_600.0 * scale) as i64;
        let end = start + Duration::seconds(duration);
        let mut at = start;
        let mut last = None;
        while at < end {
            let age = (at - start).num_seconds();
            let mut e =
                estimate(issue, at, Stage::ReviewWait, age, "land-v2", Some((1_200, 3_600, 7_200)));
            let f = e.features.as_mut().unwrap();
            f.pr_merge_conflict = Some(conflict);
            f.repo_open_prs = Some(issue % 7);
            f.pr_friction_observed_at = Some(at - Duration::minutes(2));
            lines.push(estimate_line(&e, at + Duration::seconds(1)));
            last = Some(e);
            at += Duration::hours(2);
        }
        let outcome = if issue % 17 == 0 {
            OutcomeKind::Abandoned
        } else {
            OutcomeKind::Landed
        };
        if let Some(e) = last {
            lines.push(outcome_line(&e, outcome, end, end + Duration::minutes(3)));
        }
    }
    lines
}

fn margin() -> Duration {
    Duration::seconds(120)
}

#[test]
fn ingest_keys_records_by_knowable_at_and_skips_what_accuracy_excludes() {
    let at = t0();
    let land = estimate(1, at, Stage::ReviewWait, 0, "land-v2", Some((1, 2, 3)));
    let mut finish = land.clone();
    finish.kind = Kind::Finish;
    let mut incomplete = land.clone();
    incomplete.loom.complete = false;
    let mut as_string = estimate_line(&land, at);
    as_string.body = serde_json::Value::String(as_string.body.to_string());
    let landed = at + Duration::hours(3);
    let lines = vec![
        as_string,
        estimate_line(&finish, at),
        estimate_line(&incomplete, at),
        // Two outcome records for one landing: the earlier logging wins.
        outcome_line(&land, OutcomeKind::Landed, landed, landed + Duration::minutes(9)),
        outcome_line(&land, OutcomeKind::Landed, landed, landed + Duration::minutes(4)),
        LoggedLine {
            observed_at: at,
            event: "sweep.outcome".to_string(),
            body: serde_json::Value::Null,
        },
    ];
    let logged = Logged::ingest(&lines, margin());
    assert_eq!(logged.estimates.len(), 1);
    assert_eq!(logged.estimates[0].knowable_at, at + margin());
    assert_eq!(logged.skipped.not_land, 1);
    assert_eq!(logged.skipped.incomplete_provenance, 1);
    assert_eq!(logged.skipped.other_event, 1);
    assert_eq!(
        logged.events,
        vec![LandingEvent {
            story: format!("{REPO}#1"),
            actual_at: landed,
            knowable_at: landed + Duration::minutes(4) + margin(),
            outcome: OutcomeKind::Landed,
        }]
    );
}

#[test]
fn training_labels_are_censored_at_the_cutoff_not_just_filtered_by_as_of() {
    let cutoff = t0() + Duration::days(1);
    let as_of = cutoff - Duration::hours(5);
    let e1 = estimate(1, as_of, Stage::ReviewWait, 0, "land-v2", Some((1, 2, 3)));
    let e2 = estimate(2, as_of, Stage::ReviewWait, 0, "land-v2", Some((1, 2, 3)));
    let e3 = estimate(3, as_of, Stage::ReviewWait, 0, "land-v2", Some((1, 2, 3)));
    let lines = vec![
        estimate_line(&e1, as_of),
        estimate_line(&e2, as_of),
        estimate_line(&e3, as_of),
        // 1 lands inside the training window and is known before the cutoff.
        outcome_line(
            &e1,
            OutcomeKind::Landed,
            as_of + Duration::hours(1),
            as_of + Duration::hours(1),
        ),
        // 2 lands AFTER the cutoff: the validation window's outcome.
        outcome_line(
            &e2,
            OutcomeKind::Landed,
            cutoff + Duration::hours(2),
            cutoff + Duration::hours(2),
        ),
        // 3 lands before the cutoff, but its outcome is only logged after it.
        outcome_line(
            &e3,
            OutcomeKind::Landed,
            cutoff - Duration::minutes(10),
            cutoff + Duration::minutes(30),
        ),
    ];
    let logged = Logged::ingest(&lines, margin());
    let rows = logged.rows(cutoff - Duration::days(14), cutoff, cutoff);
    assert_point_in_time(&rows, cutoff).unwrap();
    let label = |issue: u32| {
        rows.iter()
            .find(|r| r.snapshot.issue == issue)
            .map(|r| r.label)
            .unwrap()
    };
    assert!(matches!(
        label(1),
        Label::Landed {
            remaining_sec: 3600,
            ..
        }
    ));
    let censored = Label::Censored {
        at: cutoff,
        elapsed_sec: 5 * 3600,
    };
    assert_eq!(label(2), censored);
    assert_eq!(label(3), censored);
}

#[test]
fn an_estimate_knowable_only_after_the_cutoff_is_not_training_data() {
    let cutoff = t0() + Duration::days(1);
    let late = estimate(1, cutoff - Duration::seconds(30), Stage::ReviewWait, 0, "land-v2", None);
    let logged = Logged::ingest(&[estimate_line(&late, cutoff - Duration::seconds(29))], margin());
    assert!(logged.rows(t0(), cutoff, cutoff).is_empty());
}

#[test]
fn the_point_in_time_assertion_rejects_every_kind_of_leak() {
    let cutoff = t0() + Duration::days(1);
    let as_of = cutoff - Duration::hours(1);
    let e = estimate(1, as_of, Stage::ReviewWait, 0, "land-v2", Some((1, 2, 3)));
    let logged = Logged::ingest(&[estimate_line(&e, as_of)], margin());
    let good = logged.rows(t0(), cutoff, cutoff);
    assert_point_in_time(&good, cutoff).unwrap();

    let forged = |f: &dyn Fn(&mut Row)| {
        let mut rows = good.clone();
        f(&mut rows[0]);
        assert_point_in_time(&rows, cutoff).unwrap_err()
    };
    let e = forged(&|r| {
        r.label = Label::Landed {
            remaining_sec: 7200,
            knowable_at: cutoff + Duration::hours(1),
        }
    });
    assert!(e.what.contains("landing"), "{e}");
    let e = forged(&|r| {
        r.label = Label::Censored {
            at: cutoff + Duration::hours(1),
            elapsed_sec: 7200,
        }
    });
    assert!(e.what.contains("censored"), "{e}");
    let e = forged(&|r| r.snapshot.knowable_at = cutoff);
    assert!(e.what.contains("knowable"), "{e}");
    let e = forged(&|r| {
        r.label = Label::Abandoned {
            knowable_at: cutoff,
        }
    });
    assert!(e.what.contains("abandonment"), "{e}");
}

/// The issue's future-invariance requirement: fit at `T`, then add, delete
/// or alter any record knowable at or after `T` — including later outcomes
/// of training-window items — and refit. The model and every fitted
/// transform must be bit-identical.
#[test]
fn future_invariance_fitted_model_is_bit_identical_whatever_happens_after_the_cutoff() {
    let lines = fleet(120, 20);
    let cutoff = t0() + Duration::days(15);
    let settings = KmSettings {
        age_buckets: 4,
        min_cell_issues: 5,
    };
    let fit = |lines: &[LoggedLine]| {
        let logged = Logged::ingest(lines, margin());
        let train = logged.rows(cutoff - Duration::days(14), cutoff, cutoff);
        let model = KmModel::fit(&train, cutoff, settings).unwrap();
        (train, model)
    };
    let qr_fit = |lines: &[LoggedLine]| {
        let logged = Logged::ingest(lines, margin());
        let train = logged.rows(cutoff - Duration::days(14), cutoff, cutoff);
        QrModel::fit(&train, cutoff, QrSettings { ridge_milli: 100 }).unwrap()
    };
    let (train, model) = fit(&lines);
    let qr = qr_fit(&lines);
    assert!(qr.coef.is_some(), "the regression fits on the fixture");
    assert!(!qr.kept.is_empty() && !qr.censor_times.is_empty());
    assert!(model.rows > 100 && model.issues > 20, "fixture too thin: {}", model.rows);
    assert!(model.global.p50.is_some());
    assert!(train
        .iter()
        .any(|r| matches!(r.label, Label::Censored { .. })));
    assert!(train
        .iter()
        .any(|r| matches!(r.label, Label::Landed { .. })));

    let knowable = |l: &LoggedLine| l.observed_at + margin();
    let mut perturbed: Vec<LoggedLine> = lines
        .iter()
        // Delete every second record knowable at or after T.
        .enumerate()
        .filter(|(i, l)| knowable(l) < cutoff || i % 2 == 0)
        .map(|(_, l)| l.clone())
        .collect();
    for line in &mut perturbed {
        if knowable(line) >= cutoff && line.event == "eta.estimate" {
            // Alter a later estimate's answer and stage.
            let mut e: Explanation = serde_json::from_value(line.body.clone()).unwrap();
            e.result = None;
            e.current_stage.as_mut().unwrap().stage = Stage::MergeWait;
            line.body = serde_json::to_value(&e).unwrap();
        }
    }
    // Later outcomes for training-window items: one that landed after T,
    // and one that landed before T but was only logged after it.
    let in_window = train
        .iter()
        .find(|r| matches!(r.label, Label::Censored { .. }))
        .unwrap();
    let e = estimate(
        in_window.snapshot.issue,
        in_window.snapshot.as_of,
        Stage::ReviewWait,
        0,
        "land-v2",
        Some((1, 2, 3)),
    );
    perturbed.push(outcome_line(
        &e,
        OutcomeKind::Landed,
        cutoff + Duration::hours(1),
        cutoff + Duration::hours(1),
    ));
    perturbed.push(outcome_line(&e, OutcomeKind::Landed, cutoff - Duration::minutes(5), cutoff));
    // A new issue entirely after T, and an estimate before T logged after T.
    let future = estimate(
        9_999,
        cutoff + Duration::hours(3),
        Stage::Doctor,
        60,
        "land-v2",
        Some((5, 6, 7)),
    );
    perturbed.push(estimate_line(&future, cutoff + Duration::hours(3)));
    perturbed.push(outcome_line(
        &future,
        OutcomeKind::Landed,
        cutoff + Duration::hours(4),
        cutoff + Duration::hours(4),
    ));
    let late =
        estimate(9_998, cutoff - Duration::hours(2), Stage::SweepBuilder, 0, "land-v2", None);
    perturbed.push(estimate_line(&late, cutoff - Duration::seconds(60)));
    assert_ne!(perturbed.len(), lines.len());

    let (train2, model2) = fit(&perturbed);
    assert_eq!(train, train2, "training rows moved with post-cutoff records");
    assert_eq!(model, model2, "fitted model moved with post-cutoff records");
    assert_eq!(serde_json::to_string(&model).unwrap(), serde_json::to_string(&model2).unwrap());
    // The regression too: coefficients, kept columns, means, scales,
    // censoring curve and vocabulary, compared bit for bit.
    let qr2 = qr_fit(&perturbed);
    assert_eq!(qr, qr2, "fitted regression moved with post-cutoff records");
    let bits = |m: &QrModel| {
        let mut v: Vec<u64> = m.means.iter().chain(&m.sds).map(|x| x.to_bits()).collect();
        v.extend(m.coef.iter().flatten().flatten().map(|x| x.to_bits()));
        v
    };
    assert_eq!(bits(&qr), bits(&qr2));

    // Control: the same outcome made knowable BEFORE the cutoff does move
    // the fit, so the equality above is not vacuous.
    let mut known = lines.clone();
    let landed = in_window.snapshot.as_of + Duration::minutes(1);
    known.push(outcome_line(&e, OutcomeKind::Landed, landed, landed));
    let (train3, model3) = fit(&known);
    assert_ne!(train, train3);
    assert_ne!(model, model3);
    assert_ne!(bits(&qr), bits(&qr_fit(&known)));
}

#[test]
fn a_feature_read_after_the_estimate_instant_is_a_leak() {
    let cutoff = t0() + Duration::days(1);
    let as_of = cutoff - Duration::hours(1);
    let mut e = estimate(1, as_of, Stage::ReviewWait, 0, "land-v2", Some((1, 2, 3)));
    e.features.as_mut().unwrap().pr_friction_observed_at = Some(as_of + Duration::seconds(1));
    let rows = Logged::ingest(&[estimate_line(&e, as_of)], margin()).rows(t0(), cutoff, cutoff);
    let err = assert_point_in_time(&rows, cutoff).unwrap_err();
    assert!(err.what.contains("feature"), "{err}");
}

#[test]
fn the_regression_learns_a_logged_friction_feature_under_censoring() {
    let cutoff = t0() + Duration::days(15);
    let logged = Logged::ingest(&fleet(250, 20), margin());
    let train = logged.rows(cutoff - Duration::days(14), cutoff, cutoff);
    assert!(train
        .iter()
        .any(|r| matches!(r.label, Label::Censored { .. })));
    let qr = QrModel::fit(&train, cutoff, QrSettings { ridge_milli: 10 }).unwrap();
    let conflict = |want: bool| {
        train
            .iter()
            .find(|r| {
                r.snapshot.age_sec == Some(0)
                    && r.snapshot.features.as_ref().unwrap().pr_merge_conflict == Some(want)
            })
            .unwrap()
    };
    let (slow, fast) = (qr.predict(conflict(true)).unwrap(), qr.predict(conflict(false)).unwrap());
    assert!(slow.1 > fast.1, "conflicted p50 {} vs clean {}", slow.1, fast.1);
    for (p25, p50, p75) in [slow, fast] {
        assert!(p25 <= p50 && p50 <= p75);
    }
    // Too little to fit: no coefficients, no answers — never a guess.
    let thin = QrModel::fit(&train[..5], cutoff, QrSettings { ridge_milli: 10 }).unwrap();
    assert!(thin.coef.is_none());
    assert_eq!(thin.predict(&train[0]), None);
}

#[test]
fn the_walk_forward_plan_nests_selection_strictly_before_reporting() {
    let now = t0() + Duration::days(20);
    let folds = plan(&Protocol::standard(now)).unwrap();
    let cutoffs: Vec<_> = folds.iter().map(|f| (f.role, f.cutoff)).collect();
    assert_eq!(
        cutoffs,
        vec![
            (FoldRole::Selection, now - Duration::days(4)),
            (FoldRole::Selection, now - Duration::days(3)),
            (FoldRole::Reported, now - Duration::days(2)),
            (FoldRole::Reported, now - Duration::days(1)),
        ]
    );
    let freeze = now - Duration::hours(48);
    for f in &folds {
        assert_eq!(f.train_from, f.cutoff - Duration::days(14));
        assert_eq!(f.validate_until, f.cutoff + Duration::days(1));
        match f.role {
            FoldRole::Selection => assert_eq!(f.observe_until, freeze),
            FoldRole::Reported => {
                assert_eq!(f.observe_until, now);
                assert!(f.cutoff >= freeze);
            }
        }
    }
    let mut empty = Protocol::standard(now);
    empty.reported_folds = 0;
    assert!(plan(&empty).is_err());
}

#[test]
fn selection_fold_outcomes_are_censored_at_the_freeze() {
    let now = t0() + Duration::days(20);
    let logged = Logged::ingest(&fleet(150, 20), margin());
    let protocol = Protocol::standard(now);
    for fold in plan(&protocol).unwrap() {
        let rows = fold_rows(&logged, &fold).unwrap();
        for row in &rows.validate {
            match row.label {
                Label::Censored { at, .. } => assert_eq!(at, fold.observe_until),
                Label::Landed { knowable_at, .. } | Label::Abandoned { knowable_at } => {
                    assert!(knowable_at < fold.observe_until);
                }
            }
        }
    }
}

#[test]
fn score_row_is_pinball_when_landed_and_truncated_when_open() {
    let q = (1_000, 2_000, 4_000);
    let landed = Label::Landed {
        remaining_sec: 3_000,
        knowable_at: t0(),
    };
    let s = score_row(q, &landed).unwrap();
    let expected = pinball(0.25, 2_000.0) + pinball(0.5, 1_000.0) + pinball(0.75, -1_000.0);
    assert!((s.truncated_pinball - expected).abs() < 1e-9);
    assert_eq!(s.error, Some(1_000.0));
    assert_eq!(s.covered, Some(true));

    // Open for 500s: every quantile is above the bound, so nothing is
    // known to be wrong yet.
    let early = Label::Censored {
        at: t0(),
        elapsed_sec: 500,
    };
    let s = score_row(q, &early).unwrap();
    assert!(s.truncated_pinball.abs() < 1e-9);
    assert_eq!((s.error, s.covered), (None, None));

    // Open past its p75: a known miss, and a known loss.
    let late = Label::Censored {
        at: t0(),
        elapsed_sec: 10_000,
    };
    let s = score_row(q, &late).unwrap();
    assert_eq!(s.covered, Some(false));
    assert!(s.truncated_pinball > 0.0);
    assert!(score_row(q, &Label::Abandoned { knowable_at: t0() }).is_none());
}

#[test]
fn kaplan_meier_quantiles_move_out_under_censoring() {
    let cutoff = t0() + Duration::days(1);
    let mut lines = Vec::new();
    for issue in 1..=40_u32 {
        let as_of = t0() + Duration::minutes(i64::from(issue));
        let e = estimate(issue, as_of, Stage::MergeWait, 0, "land-v2", Some((1, 2, 3)));
        lines.push(estimate_line(&e, as_of));
        // Half land after `issue` hours; the rest are still open at the cutoff.
        if issue <= 20 {
            let landed = as_of + Duration::hours(i64::from(issue % 10 + 1));
            lines.push(outcome_line(&e, OutcomeKind::Landed, landed, landed));
        }
    }
    let logged = Logged::ingest(&lines, margin());
    let train = logged.rows(t0(), cutoff, cutoff);
    let settings = KmSettings {
        age_buckets: 1,
        min_cell_issues: 10,
    };
    let model = KmModel::fit(&train, cutoff, settings).unwrap();
    let cell = &model.stages["merge_wait"].all;
    assert_eq!((cell.rows, cell.issues, cell.events), (40, 40, 20));
    // Only half ever land, so the curve never reaches the 75th percentile.
    assert!(cell.p25.is_some());
    assert!(cell.p75.is_none());
    assert_eq!(model.predict(&train[0]), None);
    assert_eq!(model.stage_vocab, vec!["merge_wait".to_string()]);
}

#[test]
fn bootstrap_is_reproducible_and_brackets_the_point_value() {
    let mut sums = IssueSums::new();
    for i in 0..30 {
        sums.insert(format!("i{i}"), (f64::from(i) * 3.0, 3));
    }
    let a = bootstrap(&sums, 500, 1);
    assert_eq!(a, bootstrap(&sums, 500, 1));
    let (value, lo, hi) = (a.value.unwrap(), a.lo.unwrap(), a.hi.unwrap());
    assert!((value - 14.5).abs() < 1e-9);
    assert!(lo < value && value < hi);
    assert_eq!(a.n, 90);
    let empty = bootstrap(&IssueSums::new(), 500, 1);
    assert_eq!((empty.value, empty.lo, empty.n), (None, None, 0));
}

#[test]
fn the_protocol_scores_baseline_and_model_on_identical_rows_reproducibly() {
    let now = t0() + Duration::days(20);
    let logged = Logged::ingest(&fleet(250, 20), margin());
    let grid = [
        Settings::Km(KmSettings {
            age_buckets: 1,
            min_cell_issues: 5,
        }),
        Settings::Km(KmSettings {
            age_buckets: 2,
            min_cell_issues: 5,
        }),
        Settings::Qr(QrSettings { ridge_milli: 10 }),
        Settings::Qr(QrSettings { ridge_milli: 1_000 }),
    ];
    let (report, folds) = run(&logged, &Protocol::standard(now), "land-v2", &grid, 200).unwrap();
    assert_eq!(folds.len(), 4);
    assert_eq!(report.selection.len(), 4);
    let families: Vec<&str> = report
        .comparisons
        .iter()
        .map(|c| c.settings.family())
        .collect();
    assert_eq!(families, vec!["km", "qr"], "one frozen model per family");
    for c in &report.comparisons {
        assert!(grid.contains(&c.settings));
        assert!(c.paired_rows > 0 && c.paired_issues > 0, "{}", c.settings.id());
        assert_eq!(c.baseline.truncated_pinball.n, c.paired_rows);
        assert_eq!(c.model.truncated_pinball.n, c.paired_rows);
        assert!(c.delta_truncated_pinball.value.is_some());
        // Still-open rows are scored (censored), not dropped.
        assert!(c.model.truncated_pinball.n > c.model.landed_mae.n);
    }
    let (again, _) = run(&logged, &Protocol::standard(now), "land-v2", &grid, 200).unwrap();
    assert_eq!(report, again);
}

/// "No random or k-fold split exists anywhere in the pipeline": the only
/// pseudo-random draw is the issue bootstrap.
#[test]
fn the_pipeline_has_no_shuffle_or_k_fold() {
    for source in [
        include_str!("../offline/dataset.rs"),
        include_str!("../offline/model.rs"),
        include_str!("../offline/qr.rs"),
        include_str!("../offline/candidate.rs"),
        include_str!("../offline/evaluate.rs"),
    ] {
        for banned in [".shuffle(", "choose_multiple", "rand::", "kfold", "k_fold"] {
            assert!(!source.contains(banned), "found `{banned}`");
        }
    }
    let evaluate = include_str!("../offline/evaluate.rs");
    assert_eq!(evaluate.matches("SplitMix64::new(").count(), 1);
}
