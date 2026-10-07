//! Leak-free replay of a heuristic against real outcomes (#9325).

use super::{as_of, history_a, history_a_envelopes, provenance, subject, BACKTEST_GOLDEN, LEAKAGE};
use crate::eta::backtest::{self, cases_from_record, BacktestReport, Filter, ReplayCase};
use crate::eta::heuristics::{FinishV1, LandV1};
use crate::eta::history::{SampleSource, StageSample, StageSamples};
use crate::eta::score::OutcomeKind;
use crate::eta::{EstimateInput, Explanation, Heuristic, Kind, Stage};
use crate::telemetry::{
    PhaseDuration, RepoVisibility, SweepDisposition, SweepOutcomeRecord, SweepResult,
    TelemetryEnvelope, TelemetryRecord,
};
use chrono::Duration;
use std::collections::BTreeMap;

pub(super) fn record(
    issue: u32,
    repo: &str,
    phases: &[(&str, i64)],
    result: SweepResult,
) -> SweepOutcomeRecord {
    SweepOutcomeRecord {
        story_points: None,
        repo: Some(repo.to_string()),
        repo_unresolved: false,
        visibility: RepoVisibility::Public,
        issue,
        sweep_id: format!("sweep-issue-{issue}-1"),
        model: None,
        effort: None,
        config: BTreeMap::new(),
        phase_durations: phases
            .iter()
            .map(|(p, d)| PhaseDuration::new(*p, *d))
            .collect(),
        total_duration_sec: phases.iter().map(|(_, d)| *d).sum(),
        result,
        disposition: SweepDisposition::default(),
        pr_number: Some(9000 + issue),
        tokens_in: None,
        tokens_out: None,
        lines_added: None,
        lines_deleted: None,
        tokens_by_model: None,
        tokens_unattributed: None,
        failure_class: None,
        models_used: None,
        doctor_cycles: None,
        judge_verdicts: None,
        runtime: None,
        provider: None,
        profile: None,
        complexity: None,
        tokens_status: None,
        tokens_status_reason: None,
        attempt_index: None,
        previous_sweep_id: None,
        trigger: None,
        rework_events: None,
        pr_numbers: None,
        hw_lines_added: None,
        hw_lines_deleted: None,
        hw_files: None,
        generated_lines: None,
        test_lines: None,
    }
}

#[test]
fn cases_from_record_reconstruct_entry_instants_and_terminal_outcomes() {
    let r = record(
        42,
        "rjwalters/loom",
        &[
            ("curator", 300),
            ("builder", 1200),
            ("judge", 800),
            ("doctor", 600),
            ("judge", 400),
            ("merge", 200),
        ],
        SweepResult::Success,
    );
    let observed_at = as_of();
    let cases = cases_from_record(&r, observed_at);

    // Every one of the 6 phases yields both a finish and a land case (the
    // sequence ends with `merge`): 12 cases total.
    assert_eq!(cases.len(), 12);
    assert!(cases.iter().all(|c| c.subject.repo == "rjwalters/loom"));
    assert!(cases.iter().all(|c| c.subject.issue == 42));
    assert!(cases.iter().all(|c| c.subject.pr_number == Some(9042)));
    assert!(cases
        .iter()
        .all(|c| c.subject.sweep_id.as_deref() == Some("sweep-issue-42-1")));
    assert!(cases.iter().all(|c| c.actual_at == observed_at));

    let finish: Vec<&ReplayCase> = cases.iter().filter(|c| c.kind == Kind::Finish).collect();
    assert_eq!(finish.len(), 6);
    assert!(cases.iter().filter(|c| c.kind == Kind::Land).count() == 6);
    assert!(finish.iter().all(|c| c.outcome == OutcomeKind::Finished));
    assert!(cases
        .iter()
        .filter(|c| c.kind == Kind::Land)
        .all(|c| c.outcome == OutcomeKind::Landed));

    let stages: Vec<Stage> = finish.iter().map(|c| c.stage).collect();
    assert_eq!(
        stages,
        vec![
            Stage::SweepCurator,
            Stage::SweepBuilder,
            Stage::ReviewWait,
            Stage::Doctor,
            Stage::ReviewWait,
            Stage::MergeWait,
        ]
    );

    // Every case's lead to the record's own completion is exactly the sum of
    // durations from its phase onward — reconstructed, not guessed.
    let leads: Vec<i64> = finish
        .iter()
        .map(|c| (c.actual_at - c.as_of).num_seconds())
        .collect();
    assert_eq!(leads, vec![3500, 3200, 2000, 1200, 600, 200]);

    // Rework rounds count only the doctor phases already passed.
    let rework: Vec<u32> = finish.iter().map(|c| c.rework_rounds).collect();
    assert_eq!(rework, vec![0, 0, 0, 0, 1, 1]);
}

#[test]
fn a_record_with_no_merge_phase_yields_finish_cases_only() {
    let r =
        record(1, "rjwalters/loom", &[("curator", 100), ("builder", 200)], SweepResult::Success);
    let cases = cases_from_record(&r, as_of());
    assert_eq!(cases.len(), 2);
    assert!(cases.iter().all(|c| c.kind == Kind::Finish));
}

#[test]
fn the_no_phase_sampled_fallback_yields_no_case() {
    // One entry whose duration equals the whole sweep: the journal's
    // fallback shape (mirrors `StageSamples::push_outcome`'s own skip), not a
    // real single-phase sweep.
    let r = record(1, "rjwalters/loom", &[("builder", 700)], SweepResult::Failure);
    assert_eq!(cases_from_record(&r, as_of()).len(), 0);
}

fn merge_wait_sample(duration_sec: i64, observed_at: chrono::DateTime<chrono::Utc>) -> StageSample {
    StageSample {
        repo: "rjwalters/loom".to_string(),
        stage: Stage::MergeWait,
        duration_sec,
        observed_at,
        source: SampleSource::SweepOutcome,
        host: "host-a".to_string(),
        worked: None,
    }
}

/// 8 `merge_wait` samples (the `MIN_SAMPLES` floor), all strictly before `t`.
fn merge_wait_history(t: chrono::DateTime<chrono::Utc>) -> StageSamples {
    let mut h = StageSamples::default();
    for i in 0..8_i64 {
        h.stages
            .push(merge_wait_sample(600 + i * 60, t - Duration::hours(i + 1)));
    }
    h
}

fn one_case(t: chrono::DateTime<chrono::Utc>) -> ReplayCase {
    ReplayCase {
        queue: Vec::new(),
        pr_flags: None,
        subject: subject(),
        as_of: t,
        stage: Stage::MergeWait,
        rework_rounds: 0,
        kind: Kind::Land,
        outcome: OutcomeKind::Landed,
        actual_at: t + Duration::seconds(900),
        dispatch: None,
        age_sec: 0,
    }
}

#[test]
fn a_sample_observed_at_or_after_as_of_never_changes_the_replayed_answer() {
    let t = as_of();
    let case = one_case(t);
    let base = merge_wait_history(t);
    let replay = |h: &StageSamples| {
        backtest::run(&LandV1, h, std::slice::from_ref(&case), Filter::default(), &provenance())
    };
    let baseline = replay(&base);
    assert_eq!(baseline.overall.scored, 1, "the base history must be enough to estimate");
    let baseline_loss = baseline.overall.mean_pinball_loss_sec;
    assert!(baseline_loss.is_some());

    // A same-instant sample is NOT strictly earlier than `as_of` — the
    // boundary `select_at` actually enforces (`observed_at < as_of`) — so it
    // must be excluded exactly like a genuinely future one.
    let mut same_instant = base.clone();
    same_instant.stages.push(merge_wait_sample(999_999, t));
    let r1 = replay(&same_instant);
    assert_eq!(r1.overall.mean_pinball_loss_sec, baseline_loss, "a same-instant sample leaked");
    assert_eq!(r1, baseline, "the whole report is unaffected, not just the loss");

    let mut future = base.clone();
    future
        .stages
        .push(merge_wait_sample(999_999, t + Duration::seconds(1)));
    assert_eq!(
        replay(&future).overall.mean_pinball_loss_sec,
        baseline_loss,
        "a future sample leaked"
    );

    // Positive control: the harness is not simply insensitive to any
    // addition — a genuinely PAST sample (observed strictly before `as_of`)
    // does move the answer, so the two assertions above test something real.
    let mut past = base;
    past.stages
        .push(merge_wait_sample(999_999, t - Duration::seconds(1)));
    assert_ne!(
        replay(&past).overall.mean_pinball_loss_sec,
        baseline_loss,
        "a genuinely past sample should have moved the answer"
    );
}

/// The [`LEAKAGE`] fixture, parsed. Its last line is the would-be-leaking
/// future record; every earlier line is history observed before the replay
/// instants the subject record yields.
fn leakage_envelopes() -> Vec<TelemetryEnvelope> {
    LEAKAGE
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("fixture line parses"))
        .collect()
}

/// The issue number of [`LEAKAGE`]'s subject record — the one whose replay
/// cases are scored. Its own sweep is short and ordinary; the future record
/// that follows it is not.
const LEAKAGE_SUBJECT_ISSUE: u32 = 9325;

fn leakage_report(envelopes: &[TelemetryEnvelope], cases: &[ReplayCase]) -> BacktestReport {
    let mut history = StageSamples::default();
    history.push_envelopes(envelopes);
    backtest::run(&LandV1, &history, cases, Filter::default(), &provenance())
}

#[test]
fn the_leakage_fixtures_future_record_cannot_reach_the_replay_it_postdates() {
    let all = leakage_envelopes();
    let (future, past) = all.split_last().expect("fixture is non-empty");

    // The subject's cases, and only those: the future record contributes
    // history, never a case of its own here.
    let subject_record = past
        .iter()
        .find_map(|e| match &e.record {
            TelemetryRecord::SweepOutcome(r) if r.issue == LEAKAGE_SUBJECT_ISSUE => {
                Some((r, e.emitted_at))
            }
            _ => None,
        })
        .expect("fixture carries the subject record");
    let cases = cases_from_record(subject_record.0, subject_record.1);
    assert!(!cases.is_empty(), "the subject record yields replay cases");

    // Every case is replayed from an instant the future record postdates …
    let latest_as_of = cases.iter().map(|c| c.as_of).max().unwrap();
    assert!(
        future.emitted_at > latest_as_of,
        "the fixture's last record must be observed after every replay instant"
    );

    let without = leakage_report(past, &cases);
    assert!(without.overall.scored > 0, "the fixture's history must be enough to estimate");
    let with = leakage_report(&all, &cases);
    assert_eq!(with, without, "the future record leaked into the replay it postdates");

    // Positive control: the very same record, re-stamped to a time the
    // replay was entitled to see, DOES move the answer — so the fixture is a
    // genuine leak opportunity, not an inert extra line.
    let mut rewound = past.to_vec();
    let mut early = future.clone();
    early.emitted_at = cases.iter().map(|c| c.as_of).min().unwrap() - Duration::hours(1);
    rewound.push(early);
    assert_ne!(
        leakage_report(&rewound, &cases),
        without,
        "the fixture's future record must be one that would change the answer"
    );
}

/// Round every float in a serialised report to whole milliseconds, so the
/// golden pins the numbers without pinning the last bits of an f64 mean.
fn round_floats(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Number(n) => {
            if let Some(f) = n.as_f64() {
                if !n.is_i64() && !n.is_u64() {
                    *value = serde_json::json!((f * 1000.0).round() / 1000.0);
                }
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(round_floats),
        serde_json::Value::Object(map) => map.values_mut().for_each(round_floats),
        _ => {}
    }
}

/// The golden backtest report, pinned as a fixture exactly like
/// `explanation-golden.json`: the whole document, not a handful of scalars,
/// so a drift in any bucket (overall, per-repo, per-horizon) is caught.
/// Re-bless with `LOOM_ETA_BLESS=1` only for a deliberate schema change — a
/// shipped heuristic id's replayed behaviour is immutable.
#[test]
fn golden_backtest_report_land_v1_over_history_a() {
    let envelopes = history_a_envelopes();
    let history = history_a();
    let cases = backtest::cases_from_envelopes(&envelopes);
    let report = backtest::run(&LandV1, &history, &cases, Filter::default(), &provenance());

    assert_eq!(report.heuristic, "land-v1");
    assert_eq!(report.kind, Kind::Land);
    assert_eq!(report.overall.refused, report.overall.n - report.overall.scored);
    // Grouped by repo/horizon: every replayed case is accounted for exactly
    // once in each breakdown, so the buckets partition, not just summarise.
    assert_eq!(report.by_repo.values().map(|b| b.n).sum::<usize>(), report.overall.n);
    assert_eq!(report.by_horizon.values().map(|b| b.n).sum::<usize>(), report.overall.n);
    assert!(report.overall.scored > 0, "the fixture history must score something");

    let mut actual = serde_json::to_value(&report).unwrap();
    round_floats(&mut actual);
    if std::env::var("LOOM_ETA_BLESS").is_ok_and(|v| v == "1") {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/eta/fixtures/backtest-golden.json");
        let mut text = serde_json::to_string_pretty(&actual).unwrap();
        text.push('\n');
        std::fs::write(path, text).unwrap();
    }
    let golden: serde_json::Value = serde_json::from_str(BACKTEST_GOLDEN).unwrap();
    assert!(
        actual == golden,
        "land-v1's backtest drifted from fixtures/backtest-golden.json\nactual: {}",
        serde_json::to_string_pretty(&actual).unwrap()
    );
    // Round trip: the golden text parses back into a report.
    let parsed: BacktestReport = serde_json::from_value(golden).unwrap();
    assert_eq!(parsed.overall.n, report.overall.n);
    assert_eq!(parsed.heuristic, report.heuristic);
}

#[test]
fn finish_v1_backtest_scores_the_kind_it_predicts_only() {
    let envelopes = history_a_envelopes();
    let history = history_a();
    let cases = backtest::cases_from_envelopes(&envelopes);
    let finish_cases = cases.iter().filter(|c| c.kind == Kind::Finish).count();
    let land_cases = cases.iter().filter(|c| c.kind == Kind::Land).count();
    // Every land case is also a finish occurrence (both are recorded for the
    // same phase index), plus curator/builder-only occurrences and records
    // with no merge phase contribute finish cases with no land counterpart.
    assert!(finish_cases > land_cases, "finish={finish_cases} land={land_cases}");

    let report = backtest::run(&FinishV1, &history, &cases, Filter::default(), &provenance());
    assert_eq!(report.kind, Kind::Finish);
    assert_eq!(report.overall.n, finish_cases, "backtest scores only cases of its own kind");
}

#[test]
fn a_since_filter_narrows_the_replay_set() {
    let envelopes = history_a_envelopes();
    let history = history_a();
    let cases = backtest::cases_from_envelopes(&envelopes);
    let all = backtest::run(&LandV1, &history, &cases, Filter::default(), &provenance());
    let narrowed = backtest::run(
        &LandV1,
        &history,
        &cases,
        Filter {
            since: Some(as_of()),
            repo: None,
        },
        &provenance(),
    );
    assert!(narrowed.overall.n < all.overall.n);

    // A repo filter is exact, and case-insensitive on the slug.
    let other_repo = backtest::run(
        &LandV1,
        &history,
        &cases,
        Filter {
            since: None,
            repo: Some("someone/else"),
        },
        &provenance(),
    );
    assert_eq!(other_repo.overall.n, 0);
    // `finish-v1` is the kind that sees both of the fixture's repos, so it
    // is where a repo filter has something to narrow. The match is
    // case-insensitive on the slug.
    let both = backtest::run(&FinishV1, &history, &cases, Filter::default(), &provenance());
    let one_repo = backtest::run(
        &FinishV1,
        &history,
        &cases,
        Filter {
            since: None,
            repo: Some("RJWalters/Loom"),
        },
        &provenance(),
    );
    assert_eq!(both.by_repo.len(), 2, "the fixture spans two repos");
    assert_eq!(one_repo.by_repo.len(), 1);
    assert_eq!(one_repo.overall.n, both.by_repo["rjwalters/loom"].n);
    assert!(one_repo.overall.n > 0 && one_repo.overall.n < both.overall.n);
}

/// A `land-v1`-shaped heuristic that is obviously worse: it reuses
/// `land-v1`'s own refusal behaviour (so the two are compared on identical
/// terms) but overwrites every estimate with an absurdly early quantile
/// triple, which a real lead time of minutes-to-hours will always miss badly.
#[derive(Debug, Clone, Copy)]
struct SyntheticWorse;

impl Heuristic for SyntheticWorse {
    fn id(&self) -> &'static str {
        "synthetic-worse-test"
    }

    fn kind(&self) -> Kind {
        Kind::Land
    }

    fn estimate(&self, input: &EstimateInput, history: &StageSamples) -> Explanation {
        let mut e = LandV1.estimate(input, history);
        if let Some(result) = &mut e.result {
            result.p25_sec = 1;
            result.p50_sec = 2;
            result.p75_sec = 3;
        }
        e
    }
}

#[test]
fn paired_comparison_ranks_the_real_heuristic_over_a_synthetic_worse_one() {
    let envelopes = history_a_envelopes();
    let history = history_a();
    let cases = backtest::cases_from_envelopes(&envelopes);
    let comparison = backtest::compare(
        &LandV1,
        &SyntheticWorse,
        &history,
        &cases,
        Filter::default(),
        &provenance(),
    )
    .expect("both predict `land`");

    // The pairing is what makes the ranking meaningful: both sides replayed
    // the very same cases, so the only difference is the heuristic.
    assert_eq!(comparison.a.overall.n, comparison.b.overall.n, "identical replay set");
    assert_eq!(comparison.a.overall.scored, comparison.b.overall.scored, "identical replay set");
    assert!(comparison.a.overall.scored > 0);
    let a_loss = comparison.a.overall.mean_pinball_loss_sec.unwrap();
    let b_loss = comparison.b.overall.mean_pinball_loss_sec.unwrap();
    assert!(a_loss < b_loss, "land-v1 {a_loss} should beat the synthetic {b_loss}");
    assert_eq!(comparison.better.as_deref(), Some("land-v1"));

    // …and it is a pairing, not an ordering artefact: swapping the arguments
    // names the same winner.
    let swapped = backtest::compare(
        &SyntheticWorse,
        &LandV1,
        &history,
        &cases,
        Filter::default(),
        &provenance(),
    )
    .expect("both predict `land`");
    assert_eq!(swapped.better.as_deref(), Some("land-v1"));
}

#[test]
fn a_tie_or_empty_replay_set_reports_no_winner() {
    let history = StageSamples::default();
    let comparison = backtest::compare(
        &LandV1,
        &SyntheticWorse,
        &history,
        &[],
        Filter::default(),
        &provenance(),
    )
    .expect("both predict `land`");
    assert_eq!(comparison.a.overall.scored, 0);
    assert_eq!(comparison.better, None);
}

#[test]
fn two_kinds_are_not_a_pair_and_are_refused() {
    let envelopes = history_a_envelopes();
    let history = history_a();
    let cases = backtest::cases_from_envelopes(&envelopes);
    // `finish-v1` and `land-v1` each score only their own kind, so a
    // "comparison" of the two ranks two disjoint sets — refused, not
    // silently answered.
    let err =
        backtest::compare(&FinishV1, &LandV1, &history, &cases, Filter::default(), &provenance())
            .expect_err("different kinds are not comparable");
    assert_eq!(err.a, ("finish-v1".to_string(), Kind::Finish));
    assert_eq!(err.b, ("land-v1".to_string(), Kind::Land));
    assert!(err.to_string().contains("not comparable"), "{err}");
}
