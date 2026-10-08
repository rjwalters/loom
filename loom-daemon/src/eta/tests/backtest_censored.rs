//! In-flight cases scored as censored, with an issue-resampled interval
//! (#9970 Slice 2). Deterministic and offline: `history-a` and a fixed cutoff.

use super::{as_of, history_a, history_a_envelopes, provenance};
use crate::eta::backtest::{self, Filter, ReplayCase};
use crate::eta::heuristics::{LandV1, LandV2};
use crate::eta::score::{score_censored, EstimateSummary, OutcomeKind};
use crate::eta::{Heuristic, Kind, Stage, Subject};
use chrono::{DateTime, Duration, TimeZone, Utc};

/// 2026-09-15 05:30: issue 9066 is mid-flight (it merges at 06:00), 9059 and
/// the June 7777 laps are resolved, everything later is not yet askable.
fn cutoff() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 15, 5, 30, 0).unwrap()
}

fn case(issue: u32, as_of: DateTime<Utc>, actual_at: DateTime<Utc>) -> ReplayCase {
    ReplayCase {
        subject: Subject::new("rjwalters/loom", None, issue),
        as_of,
        stage: Stage::MergeWait,
        rework_rounds: 0,
        kind: Kind::Land,
        outcome: OutcomeKind::Landed,
        actual_at,
        dispatch: None,
        age_sec: 0,
        queue: Vec::new(),
        pr_flags: None,
        priority: None,
    }
}

#[test]
fn censor_at_keeps_resolved_censors_in_flight_and_drops_the_unaskable() {
    let t = as_of();
    let cases = vec![
        case(1, t, t + Duration::hours(1)),
        case(2, t, t + Duration::hours(5)),
        case(3, t + Duration::hours(3), t + Duration::hours(4)),
    ];
    let v = backtest::censor_at(&cases, t + Duration::hours(2));
    assert_eq!(v.len(), 2, "the case asked after the cutoff is dropped");
    assert_eq!(v[0], cases[0], "a resolved case is untouched");
    assert_eq!(v[1].outcome, OutcomeKind::Censored);
    assert_eq!(v[1].actual_at, t + Duration::hours(2), "its real landing is discarded");
    assert_eq!(v[1].as_of, t);
}

#[test]
fn a_censored_score_decides_only_what_the_lower_bound_decides() {
    let cases = backtest::cases_from_envelopes(&history_a_envelopes());
    let in_flight = backtest::censor_at(&cases, cutoff());
    let h = history_a();
    let c = in_flight
        .iter()
        .find(|c| c.outcome == OutcomeKind::Censored)
        .expect("fixture has an in-flight case");
    let mut summary = EstimateSummary::of(&LandV1.estimate(&super::input_at(c.stage, 0, 0), &h));
    // Pin the interval so the cases below are about the rule, not the fixture.
    (summary.p25_sec, summary.p50_sec, summary.p75_sec, summary.p90_sec) =
        (Some(600), Some(1200), Some(1800), Some(3600));
    summary.as_of = as_of();
    let at = |sec: i64| score_censored(&summary, as_of() + Duration::seconds(sec));

    let inside = at(1500);
    assert_eq!((inside.covered, inside.above_p75, inside.above_p90), (None, None, None));
    let past_p75 = at(1801);
    assert_eq!(
        (past_p75.covered, past_p75.above_p75, past_p75.above_p90),
        (Some(false), Some(true), None)
    );
    assert_eq!(at(3600).above_p90, None, "strict, as for a resolved case");
    let past_p90 = at(3601);
    assert_eq!((past_p90.covered, past_p90.above_p90), (Some(false), Some(true)));
    for s in [&inside, &past_p75, &past_p90] {
        assert_eq!(s.outcome, OutcomeKind::Censored);
        assert_eq!((s.pinball_loss_sec, s.pinball4_loss_sec, s.error_sec), (None, None, None));
    }
    let mut no_p90 = summary.clone();
    no_p90.p90_sec = None;
    assert_eq!(
        score_censored(&no_p90, as_of() + Duration::seconds(7200)).above_p90,
        None,
        "no p90 recorded: undecided, never false"
    );
}

#[test]
fn in_flight_cases_are_reported_apart_and_move_the_late_rate_up() {
    let cases = backtest::cases_from_envelopes(&history_a_envelopes());
    let h = history_a();
    let p = provenance();
    let view = backtest::censor_at(&cases, cutoff());
    let n_censored = view
        .iter()
        .filter(|c| c.kind == Kind::Land && c.outcome == OutcomeKind::Censored)
        .count();
    let resolved_only: Vec<ReplayCase> = view
        .iter()
        .filter(|c| c.outcome != OutcomeKind::Censored)
        .cloned()
        .collect();

    let with = backtest::run(&LandV1, &h, &view, Filter::default(), &p);
    let without = backtest::run(&LandV1, &h, &resolved_only, Filter::default(), &p);

    // The buckets are unchanged by in-flight cases: a decided late miss must
    // not read as a refusal, and a loss is never computed against the cutoff.
    assert_eq!(with.overall, without.overall);
    assert!(without.late_surprise.is_none(), "no censored case, no section");

    let late = with.late_surprise.clone().expect("censored cases present");
    assert_eq!(late.censored, 7);
    assert_eq!(late.censored, n_censored);
    assert_eq!((late.censored_answered, late.censored_late), (7, 3));
    assert_eq!((late.resolved_answered, late.resolved_late), (86, 11));
    let resolved_only_rate = late.resolved_only_rate.unwrap();
    assert!((resolved_only_rate - 11.0 / 86.0).abs() < 1e-12);
    // 14 of 93 decidable cases are late: dropping the in-flight items read
    // 12.8%, counting them reads 15.1%.
    let point = late.rate.value.unwrap();
    assert!((point - 14.0 / 93.0).abs() < 1e-12);
    assert!(
        point > resolved_only_rate,
        "dropping slow in-flight items flatters the heuristic"
    );
    assert_eq!(late.rate.n, 93);
    // Resampled by issue: 22 issues, not 93 cases.
    assert_eq!(late.items, 22);
    let (lo, hi) = (late.rate.lo.unwrap(), late.rate.hi.unwrap());
    assert!(lo <= point && point <= hi && lo < hi, "{lo} {point} {hi}");

    // Deterministic: the same replay gives the identical report.
    assert_eq!(with, backtest::run(&LandV1, &h, &view, Filter::default(), &p));
}

#[test]
fn compare_counts_in_flight_answers_and_gives_an_issue_resampled_late_delta() {
    let cases = backtest::cases_from_envelopes(&history_a_envelopes());
    let view = backtest::censor_at(&cases, cutoff());
    let c =
        backtest::compare(&LandV1, &LandV2, &history_a(), &view, Filter::default(), &provenance())
            .expect("both predict `land`");
    let land = view.iter().filter(|c| c.kind == Kind::Land);
    let resolved = land
        .clone()
        .filter(|c| c.outcome != OutcomeKind::Censored)
        .count();
    assert_eq!(c.paired.cases, land.count(), "the union includes in-flight cases");
    assert!(c.paired.cases > resolved);

    // Identical heuristics have a zero late delta, with a zero-width interval.
    let same =
        backtest::compare(&LandV1, &LandV1, &history_a(), &view, Filter::default(), &provenance())
            .unwrap();
    let d = same.paired.delta_late_rate.expect("decidable cases");
    assert_eq!((d.value, d.lo, d.hi), (Some(0.0), Some(0.0), Some(0.0)));
    assert_eq!(d.n, 93);
    assert_eq!(same.paired.a_answered, same.paired.b_answered);

    let delta = c.paired.delta_late_rate.expect("decidable cases");
    let (lo, hi) = (delta.lo.unwrap(), delta.hi.unwrap());
    assert!(lo <= delta.value.unwrap() && delta.value.unwrap() <= hi);

    // The gate's late rates share `delta_late_rate`'s lower-bound population:
    // every in-flight case enters, undecided ones as not late — 14/93, not
    // the 14/89 an `above_p90`-keyed count gives by admitting only the late.
    assert_eq!(c.paired.late_pairs, delta.n);
    assert_eq!(c.paired.late_pairs, 93);
    assert_eq!(c.paired.a_late_rate, Some(14.0 / 93.0));
    assert_eq!(c.paired.b_late_rate, Some(14.0 / 93.0));

    // With no in-flight cases the gate's rate is the resolved rate, unchanged.
    let resolved_only: Vec<_> = view
        .iter()
        .filter(|c| c.outcome != OutcomeKind::Censored)
        .cloned()
        .collect();
    let r = backtest::compare(
        &LandV1,
        &LandV2,
        &history_a(),
        &resolved_only,
        Filter::default(),
        &provenance(),
    )
    .unwrap();
    assert_eq!(r.paired.late_pairs, 86);
    assert_eq!(r.paired.a_late_rate, Some(11.0 / 86.0));
}
