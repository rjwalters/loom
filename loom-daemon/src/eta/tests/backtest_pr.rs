//! `land` replay cases from merged PRs' forge timelines (#9579).

use super::provenance;
use crate::eta::backtest::{
    self, cases_from_pr_history, cases_from_pr_records, cases_from_record, merge_case_sets,
    parse_pr_records, BacktestReport, Filter, PrCaseExclusion, PrCaseRecord, ReplayCase,
};
use crate::eta::heuristics::LandV1;
use crate::eta::history::StageSamples;
use crate::eta::journal::{entries_from_pr_history, JournalEntry};
use crate::eta::score::OutcomeKind;
use crate::eta::{EstimateInput, Explanation, Heuristic, Kind, Stage};
use crate::pr_latency::history::fixtures::{labeled, merged, open, pushed, t, unlabeled};
use crate::pr_latency::history::{PrHistory, PrState};
use crate::pr_latency::{APPROVED, CHANGES_REQUESTED, REVIEW_REQUESTED};
use crate::telemetry::{SweepOutcomeRecord, SweepResult};
use chrono::Duration;
use std::collections::BTreeMap;

/// 20 merged-out-of-sweep PRs (every odd one with a rejection lap, #9703
/// with a concurrent re-application of `loom:review-requested`), then one
/// open, one closed-unmerged, one with no closing reference and one with an
/// incomplete timeline.
const PR_HISTORY_LAND: &str = include_str!("../fixtures/pr-history-land.jsonl");

const REPO: &str = "rjwalters/loom";

fn records() -> Vec<PrCaseRecord> {
    parse_pr_records(PR_HISTORY_LAND).expect("fixture parses")
}

/// What `eta backfill` would have journaled for every fixture PR.
fn backfill(records: &[PrCaseRecord]) -> Vec<JournalEntry> {
    records
        .iter()
        .flat_map(|r| entries_from_pr_history(&r.history(), &r.repo, &provenance()))
        .collect()
}

fn history_of(rows: &[JournalEntry]) -> StageSamples {
    let mut h = StageSamples::default();
    h.push_journal(rows, "forge");
    h
}

fn replay(history: &StageSamples, cases: &[ReplayCase]) -> BacktestReport {
    backtest::run(&LandV1, history, cases, Filter::default(), &provenance())
}

#[test]
fn the_fixture_yields_land_cases_and_reports_every_exclusion() {
    let (cases, summary) = cases_from_pr_records(&records());
    assert_eq!(summary.prs, 24);
    assert_eq!(summary.contributing, 20);
    assert_eq!(summary.cases, cases.len());
    let expected: BTreeMap<String, usize> = [
        ("closed_unmerged", 1),
        ("incomplete_timeline", 1),
        ("missing_identity", 1),
        ("open", 1),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    assert_eq!(summary.excluded, expected);
    // 10 PRs: review + approval; 10: review, rejection, review, approval.
    assert_eq!(cases.len(), 10 * 2 + 10 * 4);
    assert!(cases
        .iter()
        .all(|c| c.kind == Kind::Land && c.outcome == OutcomeKind::Landed));
    assert!(cases.iter().all(|c| c.as_of < c.actual_at));
    // The issue comes from the closing reference, never the PR number.
    assert!(cases
        .iter()
        .all(|c| c.subject.pr_number == Some(c.subject.issue + 100)));
}

#[test]
fn land_v1_scores_forge_cases_against_backfilled_history() {
    let records = records();
    let (cases, _) = cases_from_pr_records(&records);
    let report = replay(&history_of(&backfill(&records)), &cases);
    assert_eq!(report.overall.n, cases.len());
    assert!(report.overall.scored > 0, "{report:?}");
    // Early cases refuse for want of earlier history, rather than leaking
    // later history to manufacture an answer.
    assert!(report.overall.refused > 0, "{report:?}");
}

#[test]
fn a_duplicate_labeling_is_one_entry_and_a_second_lap_carries_its_rework() {
    let records = records();
    let dup = records.iter().find(|r| r.number == 9703).unwrap();
    let cases = cases_from_pr_history(REPO, &dup.history(), dup.closing_issues.as_deref()).unwrap();
    let stages: Vec<(Stage, u32)> = cases.iter().map(|c| (c.stage, c.rework_rounds)).collect();
    assert_eq!(
        stages,
        vec![
            (Stage::ReviewWait, 0),
            (Stage::Doctor, 0),
            (Stage::ReviewWait, 1),
            (Stage::MergeWait, 1),
        ]
    );
}

fn two_lap_pr(number: u32, extra_later: Vec<crate::pr_latency::PrEvent>) -> PrHistory {
    let mut events = vec![
        labeled(REVIEW_REQUESTED, 100),
        labeled(CHANGES_REQUESTED, 400),
        pushed(700),
        labeled(REVIEW_REQUESTED, 710),
        labeled(APPROVED, 1000),
    ];
    events.extend(extra_later);
    merged(number, 5000, events)
}

#[test]
fn later_events_never_move_an_earlier_cases_predictor_inputs() {
    let base = cases_from_pr_history(REPO, &two_lap_pr(1, Vec::new()), Some(&[11])).unwrap();
    // A third lap and an invalidated approval, all after the first two
    // entries; the terminal is unchanged.
    let perturbed = cases_from_pr_history(
        REPO,
        &two_lap_pr(
            1,
            vec![
                unlabeled(APPROVED, 1100),
                labeled(CHANGES_REQUESTED, 1200),
                pushed(1500),
                labeled(REVIEW_REQUESTED, 1600),
                labeled(APPROVED, 2000),
            ],
        ),
        Some(&[11]),
    )
    .unwrap();
    assert!(perturbed.len() > base.len());
    // Every case at an instant before the perturbation is identical.
    for case in base.iter().filter(|c| c.as_of < t(1100)) {
        assert!(perturbed.contains(case), "{case:?} moved");
    }
    // The perturbed PR's later laps count only the rejections before them.
    let last = perturbed.last().unwrap();
    assert_eq!((last.stage, last.rework_rounds), (Stage::MergeWait, 2));
}

#[test]
fn unmeasurable_prs_are_excluded_explicitly() {
    let ok = two_lap_pr(1, Vec::new());
    let check = |h: &PrHistory, issues: Option<&[u32]>, want: PrCaseExclusion| {
        assert_eq!(cases_from_pr_history(REPO, h, issues), Err(want), "{h:?}");
    };

    let mut incomplete = ok.clone();
    incomplete.timeline_complete = false;
    check(&incomplete, Some(&[11]), PrCaseExclusion::IncompleteTimeline);

    check(
        &open(2, &[], vec![labeled(REVIEW_REQUESTED, 10)]),
        Some(&[11]),
        PrCaseExclusion::Open,
    );

    // A PR closed unmerged is never a landing, even with a full review path.
    let mut closed = ok.clone();
    closed.state = PrState::Closed;
    closed.merged_at = None;
    check(&closed, Some(&[11]), PrCaseExclusion::ClosedUnmerged);

    let mut no_merge_time = ok.clone();
    no_merge_time.merged_at = None;
    check(&no_merge_time, Some(&[11]), PrCaseExclusion::MissingMergedAt);

    let mut backwards = ok.clone();
    backwards.merged_at = Some(t(-10));
    check(&backwards, Some(&[11]), PrCaseExclusion::InvalidTerminalOrder);

    check(&ok, None, PrCaseExclusion::MissingIdentity);
    check(&ok, Some(&[]), PrCaseExclusion::MissingIdentity);
    check(&ok, Some(&[11, 12]), PrCaseExclusion::AmbiguousIdentity);
    // The same issue named twice is not ambiguous.
    assert!(cases_from_pr_history(REPO, &ok, Some(&[11, 11])).is_ok());

    // Labels applied only at/after the merge are not stage entries.
    let late = merged(3, 500, vec![labeled(APPROVED, 500), labeled(REVIEW_REQUESTED, 600)]);
    check(&late, Some(&[11]), PrCaseExclusion::NoStageEntry);
}

/// A PR's own backfilled `merge_wait` row (observed at its `merged_at`) must
/// not reach that PR's replay instants — even made absurdly distinctive —
/// while the very same row, legitimately observed before the instant, does.
#[test]
fn a_prs_own_backfilled_merge_wait_row_cannot_reach_its_own_cases() {
    let records = records();
    let rows = backfill(&records);
    let (all_cases, _) = cases_from_pr_records(&records);
    let history = history_of(&rows);

    // The latest PR's merge_wait case: the one with the most earlier history.
    let subject_pr = 9719;
    let case = all_cases
        .iter()
        .find(|c| c.subject.pr_number == Some(subject_pr) && c.stage == Stage::MergeWait)
        .cloned()
        .unwrap();
    let cases = std::slice::from_ref(&case);
    let baseline = replay(&history, cases);
    assert_eq!(baseline.overall.scored, 1, "the fixture history must be enough to estimate");

    let own = rows
        .iter()
        .position(|r| r.pr_number == Some(subject_pr) && r.stage == Some(Stage::MergeWait))
        .unwrap();
    assert!(rows[own].observed_at > case.as_of, "backfill observes at merged_at");

    // Negative control: removing, or making distinctive, the PR's own rows
    // changes nothing.
    let without: Vec<JournalEntry> = rows
        .iter()
        .filter(|r| r.pr_number != Some(subject_pr))
        .cloned()
        .collect();
    assert_eq!(replay(&history_of(&without), cases), baseline, "own rows leaked");
    let mut distinctive = rows.clone();
    distinctive[own].duration_sec = Some(999_999);
    assert_eq!(replay(&history_of(&distinctive), cases), baseline, "own merge_wait leaked");
    // …including at exactly `as_of` (strict `observed_at < as_of`).
    distinctive[own].observed_at = case.as_of;
    assert_eq!(replay(&history_of(&distinctive), cases), baseline, "same-instant row leaked");

    // Positive control. Thin the history to exactly one sample short of the
    // `MIN_SAMPLES` floor (seven other PRs' `merge_wait` rows) so the PR's
    // own row is decisive: observed at its `merged_at` the case refuses,
    // and the very same row observed one second before the replay instant
    // flips it to an estimate — so the negative controls above test a row
    // that would change the answer if it were visible.
    let thin: Vec<JournalEntry> = rows
        .iter()
        .filter(|r| {
            r.stage != Some(Stage::MergeWait)
                || r.pr_number
                    .is_some_and(|n| (subject_pr - 7..=subject_pr).contains(&n))
        })
        .cloned()
        .collect();
    let own_thin = thin
        .iter()
        .position(|r| r.pr_number == Some(subject_pr) && r.stage == Some(Stage::MergeWait))
        .unwrap();
    let refused = replay(&history_of(&thin), cases);
    assert_eq!((refused.overall.n, refused.overall.scored), (1, 0), "own row stayed invisible");
    let mut rewound = thin;
    rewound[own_thin].observed_at = case.as_of - Duration::seconds(1);
    let visible = replay(&history_of(&rewound), cases);
    assert_eq!(visible.overall.scored, 1, "the row must change the answer if visible");
    assert_ne!(visible, refused);
}

fn sweep_record(issue: u32, phases: &[(&str, i64)]) -> SweepOutcomeRecord {
    super::backtest::record(issue, REPO, phases, SweepResult::Success)
}

#[test]
fn a_case_both_sources_answer_is_counted_once() {
    // An in-sweep merge: the sweep record and the forge timeline describe
    // the same review → rejection → review → approval path.
    let record = sweep_record(
        11,
        &[
            ("builder", 600),
            ("judge", 300),
            ("doctor", 300),
            ("judge", 290),
            ("merge", 4000),
        ],
    );
    let sweep_cases: Vec<ReplayCase> = cases_from_record(&record, t(5000))
        .into_iter()
        .filter(|c| c.kind == Kind::Land)
        .collect();
    let forge = cases_from_pr_history(REPO, &two_lap_pr(111, Vec::new()), Some(&[11])).unwrap();
    assert_eq!(forge.len(), 4);

    let (merged_set, dropped) = merge_case_sets(sweep_cases.clone(), forge.clone());
    assert_eq!(dropped, 4, "every forge case was already answered by the sweep");
    assert_eq!(merged_set, sweep_cases);

    // Reading the same PR twice (offline file + forge) is one set, not two;
    // its two genuine review laps stay distinct.
    let mut twice = forge.clone();
    twice.extend(forge.clone());
    let (deduped, dropped) = merge_case_sets(Vec::new(), twice);
    assert_eq!((deduped.len(), dropped), (4, 4));
    assert_eq!(
        deduped
            .iter()
            .filter(|c| c.stage == Stage::ReviewWait)
            .count(),
        2
    );

    // A different issue is not collapsed into it.
    let other = cases_from_pr_history(REPO, &two_lap_pr(112, Vec::new()), Some(&[12])).unwrap();
    let (kept, dropped) = merge_case_sets(sweep_cases.clone(), other);
    assert_eq!((kept.len(), dropped), (sweep_cases.len() + 4, 0));
}

/// `land-v1` with every quantile replaced by an absurdly early triple.
#[derive(Debug, Clone, Copy)]
struct Worse;

impl Heuristic for Worse {
    fn id(&self) -> &'static str {
        "synthetic-worse-test"
    }
    fn kind(&self) -> Kind {
        Kind::Land
    }
    fn estimate(&self, input: &EstimateInput, history: &StageSamples) -> Explanation {
        let mut e = LandV1.estimate(input, history);
        if let Some(r) = &mut e.result {
            (r.p25_sec, r.p50_sec, r.p75_sec) = (1, 2, 3);
        }
        e
    }
}

/// `land-v1` under another id: identical predictions.
#[derive(Debug, Clone, Copy)]
struct Twin;

impl Heuristic for Twin {
    fn id(&self) -> &'static str {
        "land-v1-twin-test"
    }
    fn kind(&self) -> Kind {
        Kind::Land
    }
    fn estimate(&self, input: &EstimateInput, history: &StageSamples) -> Explanation {
        LandV1.estimate(input, history)
    }
}

#[test]
fn compare_over_forge_land_cases_names_a_real_winner_and_ties_identical_ones() {
    let records = records();
    let (cases, _) = cases_from_pr_records(&records);
    let history = history_of(&backfill(&records));
    let since = Some(t(10 * 6 * 3600));
    for filter in [
        Filter::default(),
        Filter { since, repo: None },
        Filter {
            since,
            repo: Some("RJWalters/Loom"),
        },
    ] {
        let c =
            backtest::compare(&LandV1, &Worse, &history, &cases, filter, &provenance()).unwrap();
        assert_eq!(c.a.overall.n, c.b.overall.n, "one case set");
        assert_eq!(c.a.overall.scored, c.b.overall.scored, "one case set");
        assert!(c.a.overall.scored > 0);
        assert_eq!(c.better.as_deref(), Some("land-v1"));

        let tie =
            backtest::compare(&LandV1, &Twin, &history, &cases, filter, &provenance()).unwrap();
        assert!(tie.a.overall.scored > 0);
        assert_eq!(tie.a.overall.mean_pinball_loss_sec, tie.b.overall.mean_pinball_loss_sec);
        assert_eq!(tie.better, None, "identical predictions are a valid tie");
    }
    let narrowed =
        backtest::run(&LandV1, &history, &cases, Filter { since, repo: None }, &provenance());
    assert!(narrowed.overall.n < cases.len());
}

#[test]
fn a_record_round_trips_through_its_offline_form() {
    let h = two_lap_pr(7, Vec::new());
    let record = PrCaseRecord::from_history(REPO, &h, Some(vec![11]));
    let line = serde_json::to_string(&record).unwrap();
    let parsed = parse_pr_records(&line).unwrap();
    assert_eq!(parsed, vec![record.clone()]);
    assert_eq!(
        cases_from_pr_history(REPO, &parsed[0].history(), Some(&[11])),
        cases_from_pr_history(REPO, &h, Some(&[11]))
    );
    // A JSON array is accepted as well as JSON Lines.
    let array = serde_json::to_string(&vec![record]).unwrap();
    assert_eq!(parse_pr_records(&array).unwrap().len(), 1);
    assert!(parse_pr_records("{not json}\n").is_err());
}
