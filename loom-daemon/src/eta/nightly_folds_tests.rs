//! The nightly folds (#10492): one record per heuristic per day, strictly
//! point-in-time, judged by `eta promote`'s backtest gate function, with the
//! calibrating heuristic given its replay calibration evidence, idempotent;
//! every resolved case folded exactly once, each scored with its prediction
//! day's coefficient file (#10532 review).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::eta::history::{SampleSource, StageSample};
use crate::eta::shadow::GateStatus;
use crate::eta::tests::{history_a_envelopes, provenance};
use crate::eta::Stage;
use chrono::TimeZone;

fn inputs() -> Inputs {
    Inputs {
        envelopes: history_a_envelopes(),
        ..Inputs::default()
    }
}

fn identity(h: StageSamples) -> StageSamples {
    h
}

fn builtin(_: DateTime<Utc>) -> Registry {
    Registry::builtin()
}

/// The UTC day with the most `land` cases that resolve inside it.
fn busiest_day(inputs: &Inputs) -> NaiveDate {
    let cases = backtest::cases_from_envelopes(&inputs.envelopes);
    let mut per_day = std::collections::BTreeMap::new();
    for c in cases.iter().filter(|c| c.kind == Kind::Land) {
        if c.as_of.date_naive() == c.actual_at.date_naive() {
            *per_day.entry(c.as_of.date_naive()).or_insert(0_usize) += 1;
        }
    }
    per_day
        .into_iter()
        .max_by_key(|(day, n)| (*n, *day))
        .map(|(day, _)| day)
        .expect("the fixture has same-day land cases")
}

/// [`inputs`] plus enough slow, already-landed `land` cases in the two weeks
/// before `day` for `land-2026-10-06-even-lark`'s conformal calibration to
/// engage (every case lands days later than `land-v2` expects).
fn calibrating_inputs() -> (Inputs, NaiveDate) {
    let mut inputs = inputs();
    let day = busiest_day(&inputs);
    let start = day_start(day);
    let template = backtest::cases_from_envelopes(&inputs.envelopes)
        .into_iter()
        .find(|c| c.kind == Kind::Land && c.as_of.date_naive() == day)
        .expect("a land case on the day");
    let n = crate::eta::conformal::MIN_CELL_EVENTS * 2;
    for i in 0..n {
        let mut slow = template.clone();
        slow.subject.issue += 2_000_000 + u32::try_from(i).unwrap();
        slow.as_of = start - Duration::days(13) + Duration::hours(i64::try_from(i).unwrap() * 6);
        slow.actual_at = start - Duration::hours(1);
        inputs.pr_cases.push(slow);
    }
    (inputs, day)
}

fn fold(inputs: &Inputs, day: NaiveDate) -> DayRecords {
    run_day(inputs, day, None, &identity, &builtin, &provenance())
}

#[test]
fn one_fold_per_registered_land_heuristic_and_one_summary_per_challenger() {
    let inputs = inputs();
    let day = busiest_day(&inputs);
    let records = fold(&inputs, day);

    let registry = Registry::builtin();
    let want: Vec<&str> = registry.for_kind(Kind::Land).map(|h| h.id()).collect();
    let got: Vec<&str> = records.folds.iter().map(|f| f.heuristic.as_str()).collect();
    assert_eq!(got, want);
    assert!(records
        .folds
        .iter()
        .all(|f| f.day == records.day && f.kind == "land"));
    assert_eq!(records.folds.iter().filter(|f| f.is_current).count(), 1);
    assert_eq!(records.summaries.len(), want.len() - 1);

    let current = records.folds.iter().find(|f| f.is_current).unwrap();
    assert!(current.n_cases > 0, "the busiest day has cases");
    assert!(current.delta_pinball4_loss_sec.is_none() && current.win.is_none());
    for f in records.folds.iter().filter(|f| !f.is_current) {
        assert_eq!(f.n_cases, current.n_cases, "every heuristic is asked the same cases");
        assert_eq!(f.compared_to, current.heuristic);
        assert_eq!(f.delta_answer_rate.is_some(), f.answer_rate.is_some());
    }
    // Ids are derived from (heuristic, day) only.
    let again = fold(&inputs, day);
    assert_eq!(again, records);
}

#[test]
fn a_day_with_no_cases_reports_absent_rates_not_zero() {
    let day = NaiveDate::from_ymd_opt(2020, 1, 1).unwrap();
    let records = fold(&Inputs::default(), day);
    for f in &records.folds {
        assert_eq!((f.n_cases, f.n_answered), (0, 0));
        assert_eq!(f.answer_rate, None);
        assert_eq!(f.pinball4_loss_sec, None);
        assert_eq!(f.cov_25_75, None);
        assert_eq!(f.late_surprise, None);
    }
    for s in &records.summaries {
        assert!(!s.gate_ready);
        assert_eq!((s.days, s.wins), (0, 0));
        assert_eq!(s.win_rate, None);
    }
}

/// The leak test: perturbing anything observed at or after the cutoff leaves
/// the day's records bit-identical.
#[test]
fn perturbing_post_cutoff_data_leaves_the_fold_bit_identical() {
    // With the calibration path engaged, so the leak test covers it too.
    let (base, day) = calibrating_inputs();
    let cutoff = day_start(day) + Duration::days(1);
    let baseline = fold(&base, day);
    let baseline_json = serde_json::to_string(&baseline).unwrap();

    let mut perturbed = base.clone();
    // Later sweep outcomes, with wildly different durations.
    let earliest = base.envelopes.iter().map(|e| e.emitted_at).min().unwrap();
    for e in &base.envelopes {
        let mut late = e.clone();
        late.emitted_at = cutoff + Duration::hours(1) + (e.emitted_at - earliest);
        if let crate::telemetry::TelemetryRecord::SweepOutcome(r) = &mut late.record {
            for p in &mut r.phase_durations {
                p.duration_sec = p.duration_sec * 50 + 7;
            }
            r.total_duration_sec *= 50;
        }
        perturbed.envelopes.push(late);
    }
    // A case that began on the day but had not resolved by the cutoff, and one
    // entirely after it.
    let template = backtest::cases_from_envelopes(&base.envelopes)
        .into_iter()
        .find(|c| c.kind == Kind::Land && c.as_of.date_naive() == day)
        .expect("a land case on the day");
    let mut unresolved = template.clone();
    unresolved.actual_at = cutoff + Duration::minutes(5);
    let mut later = template.clone();
    later.as_of = cutoff + Duration::minutes(1);
    later.actual_at = cutoff + Duration::hours(2);
    // And a post-cutoff case that lands absurdly late, which would move the
    // calibration if any of it leaked in.
    let mut slow_later = template.clone();
    slow_later.subject.issue += 3_000_000;
    slow_later.as_of = cutoff + Duration::minutes(2);
    slow_later.actual_at = cutoff + Duration::days(30);
    perturbed
        .pr_cases
        .extend([unresolved, later, slow_later.clone()]);
    // A calibration observation resolved after the cutoff, through the scope
    // hook (only the cut replay may supply calibration evidence).
    let (pit_history, _) = point_in_time(&base, cutoff, &identity);
    let future_obs = backtest::calibration_from_replay(
        &crate::eta::heuristics::LandV2,
        &pit_history,
        &[slow_later],
        &provenance(),
    );
    assert_eq!(future_obs.len(), 1, "the future observation is real");
    // And a history sample from the future, through the scope hook.
    let future_sample = |mut h: StageSamples| {
        h.calibration.extend(future_obs.iter().cloned());
        h.stages.push(StageSample {
            repo: "rjwalters/loom".to_string(),
            stage: Stage::MergeWait,
            duration_sec: 9_999_999,
            observed_at: cutoff + Duration::seconds(1),
            source: SampleSource::ForgeTimeline,
            host: "forge".to_string(),
            worked: None,
        });
        h
    };

    let leaked = run_day(&perturbed, day, None, &future_sample, &builtin, &provenance());
    assert_eq!(serde_json::to_string(&leaked).unwrap(), baseline_json);
    assert!(baseline.folds.iter().any(|f| f.n_cases > 0));

    // Control: the same extra case, resolved just *before* the cutoff, is
    // read — so the assertion above is not vacuous.
    let mut control = base.clone();
    let mut resolved = template.clone();
    resolved.actual_at = cutoff - Duration::seconds(1);
    resolved.subject.issue += 1_000_000;
    control.pr_cases.push(resolved);
    let seen = fold(&control, day);
    assert_ne!(serde_json::to_string(&seen).unwrap(), baseline_json);
}

/// `gate_ready` uses `eta promote`'s backtest gate function, on a replay
/// calibrated the way `eta promote` calibrates it. (Same gate function, not
/// the same data: in production the summary also reads fleet snapshots.)
#[test]
fn gate_ready_uses_the_promote_backtest_gate_function() {
    let inputs = inputs();
    let day = NaiveDate::from_ymd_opt(2026, 9, 21).unwrap();
    let records = fold(&inputs, day);
    let cutoff = day_start(day) + Duration::days(1);
    let (mut history, cases) = point_in_time(&inputs, cutoff, &identity);
    let registry = Registry::builtin();
    backtest::with_replay_calibration(|id| registry.get(id), &mut history, &cases, &provenance());
    let current = registry.current(Kind::Land, None);
    assert!(!records.summaries.is_empty());
    for s in &records.summaries {
        let candidate = registry.get(&s.heuristic).unwrap();
        let cmp = backtest::compare(
            current,
            candidate,
            &history,
            &cases,
            Filter::default(),
            &provenance(),
        )
        .unwrap();
        let decision = shadow::evaluate(
            Kind::Land,
            current.id(),
            candidate.id(),
            Some(&cmp),
            &shadow::PairedStats::of(
                shadow::PairKey {
                    kind: Kind::Land,
                    current: current.id().to_string(),
                    candidate: candidate.id().to_string(),
                },
                shadow::PairSums::default(),
            ),
            cutoff,
        );
        assert_eq!(s.gate_ready, decision.backtest.status == GateStatus::Passed, "{}", s.heuristic);
        assert_eq!(s.gate_detail, decision.backtest.detail);
        assert_eq!(s.days, decision.backtest.day_wins.days as u64);
        assert_eq!(s.wins, decision.backtest.day_wins.wins as u64);
    }
}

/// #10532 review: the fold gives `land-2026-10-06-even-lark` the same
/// replay calibration evidence `eta backtest` / `eta promote` do, so its fold
/// is the calibrated heuristic's and not its uncalibrated `land-v2` fallback.
#[test]
fn the_calibrating_heuristic_is_folded_calibrated_not_as_its_fallback() {
    use crate::eta::heuristics::LAND_EVEN_LARK;
    let (inputs, day) = calibrating_inputs();
    let start = day_start(day);
    let cutoff = start + Duration::days(1);
    let records = fold(&inputs, day);

    let (uncalibrated, cases) = point_in_time(&inputs, cutoff, &identity);
    assert!(uncalibrated.calibration.is_empty(), "only the cut replay supplies evidence");
    let registry = Registry::builtin();
    // The CLI path: `eta backtest` / `eta promote` call exactly this.
    let mut cli = uncalibrated.clone();
    backtest::with_replay_calibration(|id| registry.get(id), &mut cli, &cases, &provenance());
    assert!(cli.calibration.len() >= crate::eta::conformal::MIN_CELL_EVENTS);
    assert!(
        cli.calibration
            .iter()
            .all(|o| o.resolved_at.is_none_or(|r| r < cutoff)),
        "calibration evidence is itself point-in-time"
    );

    let plover = registry
        .get(LAND_EVEN_LARK)
        .expect("even-lark is registered");
    // The day's cohort: here, the cases that resolved on it.
    let day_cases: Vec<ReplayCase> = cases
        .iter()
        .filter(|c| c.actual_at >= start)
        .cloned()
        .collect();
    let pinball = |h: &StageSamples| {
        own_stats(&backtest::replay_scored(
            plover,
            h,
            &day_cases,
            Filter::default(),
            &provenance(),
        ))
        .pinball4
    };
    let (calibrated, fallback) = (pinball(&cli), pinball(&uncalibrated));
    assert_ne!(calibrated, fallback, "the fixture makes calibration matter");

    let folded = records
        .folds
        .iter()
        .find(|f| f.heuristic == LAND_EVEN_LARK)
        .expect("an even-lark fold");
    assert_eq!(folded.pinball4_loss_sec, calibrated, "the fold is the CLI path's answer");
    assert_ne!(folded.pinball4_loss_sec, fallback, "not the uncalibrated fallback");
}

fn at(day: u32, h: u32, m: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, day, h, m, 0).unwrap()
}

fn d(day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 10, day).unwrap()
}

#[test]
fn due_days_wait_for_half_past_midnight_and_catch_up_boundedly() {
    let none = BTreeSet::new();
    assert_eq!(due_days(at(6, 0, 29), &none), vec![d(4)], "yesterday is not due yet");
    assert_eq!(due_days(at(6, 0, 30), &none), vec![d(5)]);
    assert_eq!(due_days(at(6, 23, 0), &none), vec![d(5)]);

    let done: BTreeSet<_> = [d(5)].into();
    assert!(due_days(at(6, 12, 0), &done).is_empty(), "idempotent");
    assert_eq!(due_days(at(8, 1, 0), &done), vec![d(6), d(7)]);

    let old: BTreeSet<_> = [NaiveDate::from_ymd_opt(2026, 9, 1).unwrap()].into();
    let days = due_days(at(20, 1, 0), &old);
    assert_eq!(days.len(), MAX_CATCH_UP_DAYS as usize);
    assert_eq!(days.last(), Some(&d(19)), "the newest days are kept");
    assert!(days.windows(2).all(|w| w[0] < w[1]), "oldest first");
}

#[test]
fn run_due_writes_each_day_once_and_the_scoreboard_state() {
    let root = tempfile::tempdir().unwrap();
    let now = at(6, 1, 0);
    let first = run_due(root.path(), now, None, &identity, &builtin, &provenance());
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].day, "2026-10-05");
    assert!(day_path(root.path(), d(5)).exists());
    let state = read_state(root.path()).expect("summary.json");
    assert_eq!(state.day, "2026-10-05");
    assert_eq!(state.summaries, first[0].summaries);

    let again =
        run_due(root.path(), now + Duration::hours(1), None, &identity, &builtin, &provenance());
    assert!(again.is_empty(), "a restart does not duplicate the day");
    let next = run_due(root.path(), at(7, 1, 0), None, &identity, &builtin, &provenance());
    assert_eq!(next.iter().map(|r| r.day.as_str()).collect::<Vec<_>>(), ["2026-10-06"]);
}

/// A synthetic `land` case like the fixture's, `issue_offset` away from every
/// fixture issue, predicted at `as_of` and landed at `actual_at`.
fn extra_case(
    inputs: &Inputs,
    issue_offset: u32,
    as_of: DateTime<Utc>,
    actual_at: DateTime<Utc>,
) -> ReplayCase {
    let mut c = backtest::cases_from_envelopes(&inputs.envelopes)
        .into_iter()
        .find(|c| c.kind == Kind::Land)
        .expect("the fixture has land cases");
    c.subject.issue += issue_offset;
    c.as_of = as_of;
    c.actual_at = actual_at;
    c
}

fn current_n_cases(records: &DayRecords) -> u64 {
    records
        .folds
        .iter()
        .find(|f| f.is_current)
        .expect("a current fold")
        .n_cases
}

/// #10532 review, finding 1: a case predicted before midnight and landed after
/// it was dropped from every fold (unresolved at its prediction day's cutoff,
/// then outside the next day's `as_of` window). It is folded on the day it
/// resolves, once.
#[test]
fn a_cross_midnight_case_is_folded_exactly_once_on_the_day_it_resolves() {
    // The case alone: `n_cases` counts answered and refused cases alike.
    let d = NaiveDate::from_ymd_opt(2026, 10, 2).unwrap();
    let midnight = day_start(d);
    let with = Inputs {
        pr_cases: vec![extra_case(
            &inputs(),
            4_000_000,
            midnight - Duration::hours(1),
            midnight + Duration::hours(1),
        )],
        ..Inputs::default()
    };
    for day in [d - Duration::days(1), d + Duration::days(1)] {
        assert_eq!(current_n_cases(&fold(&with, day)), 0, "not on {day}");
    }
    let folded = fold(&with, d);
    assert_eq!(current_n_cases(&folded), 1, "folded on the day it resolves");
    assert!(folded.folds.iter().all(|f| f.n_cases == 1));
}

/// Finding 1, fixture-wide: across consecutive days every resolved case is in
/// exactly one day's cohort, including the fixture's own cross-midnight ones.
#[test]
fn every_fixture_case_is_in_exactly_one_daily_cohort() {
    let inputs = inputs();
    let far = Utc.with_ymd_and_hms(2100, 1, 1, 0, 0, 0).unwrap();
    let all = known_cases(&inputs, far);
    let crossing = all
        .iter()
        .filter(|c| c.as_of.date_naive() != c.actual_at.date_naive())
        .count();
    assert!(crossing > 0, "the fixture has cross-midnight land cases");
    let first = all.iter().map(|c| c.as_of.date_naive()).min().unwrap();
    let last = all.iter().map(|c| c.actual_at.date_naive()).max().unwrap();
    let key = |c: &ReplayCase| {
        (c.subject.repo.clone(), c.subject.issue, c.stage.as_str(), c.as_of, c.actual_at)
    };
    let mut folded = Vec::new();
    let mut day = first;
    while day <= last {
        let known = known_cases(&inputs, day_start(day) + Duration::days(1));
        folded.extend(cohort(&inputs, day, &known).iter().map(key));
        day += Duration::days(1);
    }
    let mut want: Vec<_> = all.iter().map(key).collect();
    want.sort();
    folded.sort();
    assert_eq!(folded, want);
}

/// Finding 1's other half: the cross-midnight case is folded at the end of
/// the day it resolves, but its prediction still reads nothing at or after
/// its own `as_of` — not its own landing, not anything else that day.
#[test]
fn a_cross_midnight_prediction_never_sees_its_own_outcome() {
    // The case alone, replayed against the fixture's history (all of it
    // observed weeks earlier), handed in through the scope hook.
    let d = NaiveDate::from_ymd_opt(2026, 10, 2).unwrap();
    let midnight = day_start(d);
    let case = extra_case(
        &inputs(),
        5_000_000,
        midnight - Duration::hours(1),
        midnight + Duration::hours(1),
    );
    let inputs = Inputs {
        pr_cases: vec![case.clone()],
        ..Inputs::default()
    };
    let fixture = crate::eta::tests::history_a();
    // The fixture history plus, at `at`, many absurdly slow samples of the
    // case's own stage.
    let with_flood = |at: Option<DateTime<Utc>>| {
        let (fixture, repo, stage) = (fixture.clone(), case.subject.repo.clone(), case.stage);
        move |_: StageSamples| {
            let mut h = fixture.clone();
            for i in 0..at.map_or(0, |_| 200) {
                h.stages.push(StageSample {
                    repo: repo.clone(),
                    stage,
                    duration_sec: 9_000_000 + i,
                    observed_at: at.unwrap(),
                    source: SampleSource::SweepOutcome,
                    host: "local".to_string(),
                    worked: None,
                });
            }
            h
        }
    };
    let run = |scope: &dyn Fn(StageSamples) -> StageSamples| {
        let records = run_day(&inputs, d, None, scope, &builtin, &provenance());
        assert_eq!(current_n_cases(&records), 1, "the case is in this day's fold");
        serde_json::to_string(&records).unwrap()
    };
    let baseline = run(&with_flood(None));
    assert!(baseline.contains("\"n_answered\":1"), "the fixture history answers it");
    // Observed at its own landing, and between its prediction and the cutoff.
    let at_landing = with_flood(Some(midnight + Duration::hours(1)));
    let after_prediction = with_flood(Some(midnight - Duration::minutes(59)));
    assert_eq!(run(&at_landing), baseline, "its own landing leaked in");
    assert_eq!(run(&after_prediction), baseline, "a post-prediction sample leaked in");
    // Control: the same flood observed just before the prediction is read.
    let before_prediction = with_flood(Some(midnight - Duration::hours(2)));
    assert_ne!(run(&before_prediction), baseline, "the flood is not vacuous");
}

/// A fixture fit cut off at `as_of`; `shift` perturbs its models, so two
/// fits give distinguishable answers.
fn dated_fit(as_of: DateTime<Utc>, shift: f64) -> crate::eta::fit::CoefficientFile {
    let mut file = crate::eta::tests::land_twin_otter::fixture_fit(as_of);
    if let Some(aft) = file.aft.as_mut() {
        for s in &mut aft.log_sigma {
            *s += shift;
        }
        if let Some(b) = aft.beta.first_mut() {
            *b += shift * 2.0;
        }
    }
    file.with_derived_id()
}

/// The fixture's distinct `land` prediction days, oldest first.
fn prediction_days(inputs: &Inputs) -> Vec<NaiveDate> {
    let far = Utc.with_ymd_and_hms(2100, 1, 1, 0, 0, 0).unwrap();
    let days: BTreeSet<NaiveDate> = known_cases(inputs, far)
        .iter()
        .map(|c| c.as_of.date_naive())
        .collect();
    days.into_iter().collect()
}

/// The summary [`run_day`] would publish for `candidate`, recomputed with
/// main's independent walk-forward replay ([`crate::eta::walk_forward`]):
/// each estimate served by the newest of `files` cut off strictly before its
/// own `as_of`, over `window`.
fn walk_forward_gate(
    inputs: &Inputs,
    day: NaiveDate,
    files: Vec<crate::eta::fit::CoefficientFile>,
    window_from: Option<NaiveDate>,
    candidate: &str,
) -> shadow::BacktestGate {
    let cutoff = day_start(day) + Duration::days(1);
    let (mut history, cases) = point_in_time(inputs, cutoff, &identity);
    let fits = crate::eta::walk_forward::DatedFits::new(files);
    backtest::with_replay_calibration(|id| fits.heuristic(id), &mut history, &cases, &provenance());
    let window: Vec<ReplayCase> = cases
        .into_iter()
        .filter(|c| window_from.is_none_or(|w| c.as_of.date_naive() >= w))
        .collect();
    let current = fits
        .heuristic(Registry::default_current(Kind::Land))
        .unwrap();
    let cmp = backtest::compare(
        &current,
        &fits.heuristic(candidate).unwrap(),
        &history,
        &window,
        Filter::default(),
        &provenance(),
    )
    .ok();
    shadow::backtest_gate(current.id(), candidate, cmp.as_ref())
}

/// #10532 review, finding 2: with two distinguishable dated fits, the summary
/// scores every case with its own prediction day's fit — exactly what an
/// independent walk-forward replay gives — not with the one fit that predates
/// the whole window.
#[test]
fn the_summary_scores_each_case_with_its_prediction_days_fit() {
    use crate::eta::heuristics::LAND_TWIN_OTTER;
    let inputs = inputs();
    let days = prediction_days(&inputs);
    assert!(days.len() >= 4, "the fixture spans several prediction days");
    // Cut off in the last second of a day, so "strictly before the day
    // began" (the fold's rule) and "strictly before `as_of`" (the
    // walk-forward replay's) pick the same file for every case.
    let a = dated_fit(day_start(days[0]) - Duration::seconds(1), 0.0);
    let b = dated_fit(day_start(days[days.len() / 2]) - Duration::seconds(1), 0.8);
    let root = tempfile::tempdir().unwrap();
    let dir = crate::eta::fit::coeffs::fit_dir(root.path());
    std::fs::create_dir_all(&dir).unwrap();
    for f in [&a, &b] {
        crate::eta::fit::coeffs::write(&dir.join(crate::eta::fit::coeffs::path_for(f.as_of)), f)
            .unwrap();
    }
    let archive = FitArchive::load(root.path());
    let day = *days.last().unwrap();
    let records = run_day(
        &inputs,
        day,
        None,
        &identity,
        &|before| archive.registry_before(before),
        &provenance(),
    );
    let s = records
        .summaries
        .iter()
        .find(|s| s.heuristic == LAND_TWIN_OTTER)
        .expect("a twin-otter summary");

    let walked =
        walk_forward_gate(&inputs, day, vec![a.clone(), b], Some(days[0]), LAND_TWIN_OTTER);
    let only_a = walk_forward_gate(&inputs, day, vec![a], Some(days[0]), LAND_TWIN_OTTER);
    assert_ne!(walked.detail, only_a.detail, "the two fits are distinguishable");
    assert_eq!(s.gate_detail, walked.detail);
    assert_eq!(
        (s.days, s.wins, s.ties),
        (
            walked.day_wins.days as u64,
            walked.day_wins.wins as u64,
            walked.day_wins.ties as u64,
        )
    );
    assert_eq!(s.cases, walked.cases as u64);
    assert_eq!(s.fitted_from.as_deref(), Some(days[0].format("%Y-%m-%d").to_string().as_str()));
    assert_eq!(s.cases_before_fit, 0);
}

/// Finding 2's missing-fit half: a prediction day older than every retained
/// coefficient file is left out of the summary and counted, not scored as a
/// `no_model` refusal; with no file at all nothing is left out.
#[test]
fn cases_predicted_before_every_retained_fit_are_left_out_and_counted() {
    use crate::eta::heuristics::LAND_TWIN_OTTER;
    let inputs = inputs();
    let days = prediction_days(&inputs);
    let from = days[days.len() / 2];
    let only = dated_fit(day_start(from) - Duration::seconds(1), 0.0);
    let registry_for = |before: DateTime<Utc>| {
        Registry::with_fit((only.as_of < before).then(|| Arc::new(only.clone())))
    };
    let day = *days.last().unwrap();
    let records = run_day(&inputs, day, None, &identity, &registry_for, &provenance());
    let before_fit = known_cases(&inputs, day_start(day) + Duration::days(1))
        .iter()
        .filter(|c| c.as_of.date_naive() < from)
        .count();
    assert!(before_fit > 0, "the fixture has cases before the fit");
    let walked = walk_forward_gate(&inputs, day, vec![only], Some(from), LAND_TWIN_OTTER);
    for s in &records.summaries {
        assert_eq!(s.fitted_from.as_deref(), Some(from.format("%Y-%m-%d").to_string().as_str()));
        assert_eq!(s.cases_before_fit, before_fit as u64);
    }
    let s = records
        .summaries
        .iter()
        .find(|s| s.heuristic == LAND_TWIN_OTTER)
        .unwrap();
    assert_eq!(s.gate_detail, walked.detail);

    let unfitted = run_day(&inputs, day, None, &identity, &builtin, &provenance());
    for s in &unfitted.summaries {
        assert_eq!((s.fitted_from.as_deref(), s.cases_before_fit), (None, 0));
    }
}

/// The production `registry_for` reads the fit directories once and then
/// picks exactly what [`Registry::load`] would for any `before`.
#[test]
fn the_fit_archive_picks_what_registry_load_picks() {
    let root = tempfile::tempdir().unwrap();
    let dir = crate::eta::fit::coeffs::fit_dir(root.path());
    std::fs::create_dir_all(&dir).unwrap();
    let t0 = Utc.with_ymd_and_hms(2026, 9, 1, 12, 0, 0).unwrap();
    for (i, shift) in [0.0, 0.3, 0.6].into_iter().enumerate() {
        let f = dated_fit(t0 + Duration::days(i64::try_from(i).unwrap() * 2), shift);
        crate::eta::fit::coeffs::write(&dir.join(crate::eta::fit::coeffs::path_for(f.as_of)), &f)
            .unwrap();
    }
    std::fs::write(dir.join("other.json"), "{\"schema\":\"nope\"}").unwrap();
    let archive = FitArchive::load(root.path());
    for h in [-24, 0, 1, 47, 48, 49, 200] {
        let before = t0 + Duration::hours(h);
        let (got, want) = (archive.registry_before(before), Registry::load(root.path(), before));
        assert_eq!(got.fit_id(), want.fit_id(), "{before}");
        assert_eq!(got.fit_v2_id(), want.fit_v2_id(), "{before}");
    }
    assert!(FitArchive::load(&root.path().join("missing"))
        .registry_before(t0 + Duration::days(9))
        .fit_id()
        .is_none());
}

#[test]
fn the_fold_path_makes_no_forge_call_and_spawns_no_process() {
    let this = include_str!("nightly_folds.rs");
    let production = this.split("#[cfg(test)]").next().unwrap();
    for needle in [
        concat!("gh", "_invocation"),
        concat!("Gh", "Invocation"),
        concat!("forge", "_listing"),
        concat!("Command", "::new"),
        concat!("run", "_gh"),
    ] {
        assert!(!production.contains(needle), "nightly_folds.rs mentions `{needle}`");
    }
}
