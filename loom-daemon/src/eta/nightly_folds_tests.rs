//! The nightly folds (#10492): one record per heuristic per day, strictly
//! point-in-time, agreeing with `eta promote`'s backtest gate, idempotent.

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
    let base = inputs();
    let day = busiest_day(&base);
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
    perturbed.pr_cases = vec![unresolved, later];
    // And a history sample from the future, through the scope hook.
    let future_sample = |mut h: StageSamples| {
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
    control.pr_cases = vec![resolved];
    let seen = fold(&control, day);
    assert_ne!(serde_json::to_string(&seen).unwrap(), baseline_json);
}

/// `gate_ready` is `eta promote`'s backtest gate on the same replay.
#[test]
fn gate_ready_agrees_with_the_promote_backtest_gate() {
    let inputs = inputs();
    let day = NaiveDate::from_ymd_opt(2026, 9, 21).unwrap();
    let records = fold(&inputs, day);
    let cutoff = day_start(day) + Duration::days(1);
    let (history, cases) = point_in_time(&inputs, cutoff, &identity);
    let registry = Registry::builtin();
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
