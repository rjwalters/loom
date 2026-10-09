//! The backtest's starred / held / sequenced breakdown (#10524): each case's
//! PR labels at `as_of`, point-in-time, and the per-subset coverage and late
//! surprise.

use super::{as_of, provenance, subject};
use crate::eta::backtest::subsets::{
    SUBSET_HELD, SUBSET_LABELS_KNOWN, SUBSET_SEQUENCED, SUBSET_STARRED, SUBSET_STARRED_ANY,
    SUBSET_STAR_ANY_UNKNOWN, SUBSET_UNSTARRED_ANY,
};
use crate::eta::backtest::{self, pr_case_entries, Filter, ReplayCase};
use crate::eta::explanation::EstimateResult;
use crate::eta::fit::features_v2::PriorityInputs;
use crate::eta::heuristics::blank;
use crate::eta::history::StageSamples;
use crate::eta::labels::{FLAG_OP_HOLD, FLAG_SEQUENCED, FLAG_STARRED};
use crate::eta::score::OutcomeKind;
use crate::eta::{EstimateInput, Explanation, Heuristic, Kind, Stage};
use crate::pr_latency::history::fixtures::{labeled, merged, t, unlabeled};
use crate::pr_latency::history::{PrEvent, PrHistory};
use crate::pr_latency::{APPROVED, REVIEW_REQUESTED};
use chrono::Duration;

const REPO: &str = "rjwalters/loom";
const STAR: &str = "loom:operator-priority";
const SEQUENCED: &str = "loom:sequenced";
const OPERATOR: &str = "loom:operator";

/// Starred from the start; review at 100, approved at 400, sequenced at 600
/// (mid-`merge_wait`, so no new entry), operator hold at 800, merged at 2000.
fn starred_pr(later: Vec<PrEvent>) -> PrHistory {
    let mut events = vec![
        labeled(STAR, 50),
        labeled(REVIEW_REQUESTED, 100),
        unlabeled(REVIEW_REQUESTED, 400),
        labeled(APPROVED, 400),
        labeled(SEQUENCED, 600),
        labeled(OPERATOR, 800),
    ];
    events.extend(later);
    merged(31, 2000, events)
}

fn flags_by_entry(h: &PrHistory) -> Vec<(i64, Stage, Option<u8>)> {
    pr_case_entries(REPO, h, Some(&[11]))
        .unwrap()
        .cases
        .iter()
        .map(|c| ((c.as_of - t(0)).num_seconds(), c.stage, c.pr_flags))
        .collect()
}

#[test]
fn a_forge_case_carries_the_pr_labels_in_force_at_its_own_entry() {
    assert_eq!(
        flags_by_entry(&starred_pr(Vec::new())),
        vec![
            (100, Stage::ReviewWait, Some(FLAG_STARRED)),
            (400, Stage::MergeWait, Some(FLAG_STARRED)),
            (800, Stage::MergeHold, Some(FLAG_STARRED | FLAG_SEQUENCED | FLAG_OP_HOLD)),
        ]
    );
}

/// The leak test for the label subsets: a label added after a case's entry
/// (or removed after it) never moves that case into or out of a subset.
#[test]
fn later_labels_never_move_an_earlier_cases_subset() {
    let base = flags_by_entry(&starred_pr(Vec::new()));
    let perturbed = flags_by_entry(&starred_pr(vec![
        unlabeled(STAR, 900),
        unlabeled(OPERATOR, 1000),
        labeled("loom:merge-conflict", 1100),
    ]));
    // The release at 1000 is a new `merge_wait` entry; every earlier entry
    // is unchanged. The new entry sees the star already gone, and not the
    // conflict label added after it.
    assert_eq!(perturbed[..3], base[..]);
    assert_eq!(perturbed[3], (1000, Stage::MergeWait, Some(FLAG_SEQUENCED)));
}

/// Answers every case with remaining `(600, 900, 1200, 1500)`; with any PR
/// label in its input it answers `(10, 20, 30, 40)` instead, so a test can
/// see whether labels reached the estimator.
#[derive(Debug, Clone, Copy)]
struct Fixed;

impl Heuristic for Fixed {
    fn id(&self) -> &'static str {
        "fixed-subset-test"
    }

    fn kind(&self) -> Kind {
        Kind::Land
    }

    fn models_hold(&self) -> bool {
        true
    }

    fn estimate(&self, input: &EstimateInput, _history: &StageSamples) -> Explanation {
        let labelled = input
            .features
            .labels
            .as_ref()
            .is_some_and(|l| !l.is_empty());
        let (p25, p50, p75, p90) = if labelled {
            (10, 20, 30, 40)
        } else {
            (600, 900, 1200, 1500)
        };
        let mut e = blank("fixed-subset-test", Kind::Land, input);
        e.result = Some(EstimateResult {
            p25_sec: p25,
            p50_sec: p50,
            p75_sec: p75,
            p90_sec: Some(p90),
            eta_p50_at: input.as_of + Duration::seconds(p50),
            samples_min: 10,
            stage_marks: Vec::new(),
            tail_extrapolated: false,
        });
        e
    }
}

fn case(i: i64, stage: Stage, flags: Option<u8>, actual_sec: i64) -> ReplayCase {
    let t = as_of() + Duration::seconds(i * 60);
    let mut subject = subject();
    subject.issue = 500 + i as u32;
    ReplayCase {
        subject,
        as_of: t,
        stage,
        rework_rounds: 0,
        kind: Kind::Land,
        outcome: OutcomeKind::Landed,
        actual_at: t + Duration::seconds(actual_sec),
        dispatch: None,
        age_sec: 0,
        queue: Vec::new(),
        pr_flags: flags,
        priority: None,
    }
}

fn mixed_cases() -> Vec<ReplayCase> {
    vec![
        // Starred, covered.
        case(0, Stage::MergeWait, Some(FLAG_STARRED), 900),
        // Starred and sequenced, a late surprise.
        case(1, Stage::MergeWait, Some(FLAG_STARRED | FLAG_SEQUENCED), 2000),
        // Held, labels unknown (a sweep case), early.
        case(2, Stage::MergeHold, None, 100),
        // Labels known, no flag, covered.
        case(3, Stage::ReviewWait, Some(0), 900),
        // Labels unknown, covered: in no subset.
        case(4, Stage::ReviewWait, None, 900),
    ]
}

fn run(cases: &[ReplayCase]) -> backtest::BacktestReport {
    backtest::run(&Fixed, &StageSamples::default(), cases, Filter::default(), &provenance())
}

#[test]
fn each_subset_reports_its_own_coverage_and_late_surprise() {
    let report = run(&mixed_cases());
    assert_eq!(report.overall.n, 5);
    let keys: Vec<&str> = report.by_subset.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            SUBSET_HELD,
            SUBSET_LABELS_KNOWN,
            SUBSET_SEQUENCED,
            SUBSET_STARRED
        ]
    );

    let held = &report.by_subset[SUBSET_HELD];
    assert_eq!((held.bucket.n, held.bucket.scored), (1, 1));
    assert_eq!(held.bucket.coverage, Some(0.0));
    assert_eq!((held.late_decided, held.late_rate), (1, Some(0.0)));

    let starred = &report.by_subset[SUBSET_STARRED];
    assert_eq!(starred.bucket.n, 2);
    assert_eq!(starred.bucket.coverage, Some(0.5));
    assert_eq!((starred.late_decided, starred.late_rate), (2, Some(0.5)));

    let sequenced = &report.by_subset[SUBSET_SEQUENCED];
    assert_eq!(sequenced.bucket.n, 1);
    assert_eq!(sequenced.bucket.coverage, Some(0.0));
    assert_eq!(sequenced.late_rate, Some(1.0));

    // The population the label subsets are drawn from: the three cases whose
    // labels are known, not the two sweep cases.
    let known = &report.by_subset[SUBSET_LABELS_KNOWN];
    assert_eq!(known.bucket.n, 3);
    assert_eq!(known.bucket.coverage, Some(2.0 / 3.0));
    assert_eq!((known.late_decided, known.late_rate), (3, Some(1.0 / 3.0)));
}

/// The flags select cases; they never reach the estimator, so the replayed
/// answers (and every pooled figure) are those of the same cases unflagged.
#[test]
fn the_flags_never_change_a_replayed_answer() {
    let flagged = run(&mixed_cases());
    let bare: Vec<ReplayCase> = mixed_cases()
        .into_iter()
        .map(|mut c| {
            c.pr_flags = None;
            c
        })
        .collect();
    let unflagged = run(&bare);
    assert_eq!(flagged.overall, unflagged.overall);
    assert_eq!(flagged.by_horizon, unflagged.by_horizon);
    assert_eq!(flagged.overall.coverage, Some(0.6), "the unlabelled answer, every case");
    // Unflagged, only the held case is left in a subset.
    let keys: Vec<&str> = unflagged.by_subset.keys().map(String::as_str).collect();
    assert_eq!(keys, [SUBSET_HELD]);
}

/// A report with no held case and no known labels has no subset section,
/// and serializes without the key, so every existing report is unchanged.
#[test]
fn a_report_with_no_subset_member_omits_the_section() {
    let cases = vec![
        case(0, Stage::MergeWait, None, 900),
        case(1, Stage::ReviewWait, None, 900),
    ];
    let report = run(&cases);
    assert!(report.by_subset.is_empty());
    let json = serde_json::to_value(&report).unwrap();
    assert!(json.get("by_subset").is_none(), "{json}");

    // With members, the subset serializes its bucket flat beside the late
    // figures, and round-trips.
    let report = run(&mixed_cases());
    let json = serde_json::to_value(&report).unwrap();
    let held = &json["by_subset"][SUBSET_HELD];
    assert_eq!(held["n"], 1);
    assert_eq!(held["late_decided"], 1);
    let back: backtest::BacktestReport = serde_json::from_value(json).unwrap();
    assert_eq!(back, report);
}

// ------------------------------------- linked-issue-aware star subsets (#10508)

/// Answers every case with remaining `(60, 90, 120, 150)`: late on every
/// case [`Fixed`] covers, so a paired subset has something to tell apart.
#[derive(Debug, Clone, Copy)]
struct Short;

impl Heuristic for Short {
    fn id(&self) -> &'static str {
        "short-subset-test"
    }

    fn kind(&self) -> Kind {
        Kind::Land
    }

    fn models_hold(&self) -> bool {
        true
    }

    fn estimate(&self, input: &EstimateInput, _history: &StageSamples) -> Explanation {
        let mut e = blank("short-subset-test", Kind::Land, input);
        e.result = Some(EstimateResult {
            p25_sec: 60,
            p50_sec: 90,
            p75_sec: 120,
            p90_sec: Some(150),
            eta_p50_at: input.as_of + Duration::seconds(90),
            samples_min: 10,
            stage_marks: Vec::new(),
            tail_extrapolated: false,
        });
        e
    }
}

fn with_star(mut c: ReplayCase, starred_any: Option<bool>) -> ReplayCase {
    c.priority = Some(PriorityInputs {
        starred_any,
        priority_level: starred_any.map(u8::from),
        ..PriorityInputs::default()
    });
    c
}

/// No PR label carries a star on any of these: the first two are starred
/// only through their linked issue, which [`SUBSET_STARRED`] cannot see.
fn priority_cases() -> Vec<ReplayCase> {
    vec![
        // Linked-issue star only, covered.
        with_star(case(0, Stage::MergeWait, Some(0), 900), Some(true)),
        // Linked-issue star only, a late surprise for both.
        with_star(case(1, Stage::MergeWait, Some(0), 2000), Some(true)),
        // Known unstarred, covered.
        with_star(case(2, Stage::ReviewWait, Some(0), 900), Some(false)),
        // Star unknown: never counted as unstarred.
        with_star(case(3, Stage::ReviewWait, Some(0), 900), None),
        // No priority inputs at all (another source): in no star_any subset.
        case(4, Stage::ReviewWait, None, 900),
    ]
}

#[test]
fn star_any_subsets_see_the_linked_issue_star_and_keep_unknown_apart() {
    let report = run(&priority_cases());
    let keys: Vec<&str> = report.by_subset.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            SUBSET_LABELS_KNOWN,
            SUBSET_STAR_ANY_UNKNOWN,
            SUBSET_STARRED_ANY,
            SUBSET_UNSTARRED_ANY
        ]
    );
    // The PR-label subset sees none of the linked-issue stars.
    assert!(!report.by_subset.contains_key(SUBSET_STARRED));

    let starred = &report.by_subset[SUBSET_STARRED_ANY];
    assert_eq!(starred.bucket.n, 2);
    assert_eq!((starred.late_decided, starred.late_rate), (2, Some(0.5)));

    let unstarred = &report.by_subset[SUBSET_UNSTARRED_ANY];
    assert_eq!(unstarred.bucket.n, 1, "the unknown case is not unstarred");
    assert_eq!(unstarred.late_rate, Some(0.0));

    let unknown = &report.by_subset[SUBSET_STAR_ANY_UNKNOWN];
    assert_eq!(unknown.bucket.n, 1);
}

/// The star_any subsets select cases only: the pooled figures are those of
/// the same cases without priority inputs (`Fixed` reads no priority).
#[test]
fn star_any_subsets_never_change_a_pooled_figure() {
    let bare: Vec<ReplayCase> = priority_cases()
        .into_iter()
        .map(|mut c| {
            c.priority = None;
            c
        })
        .collect();
    let (with, without) = (run(&priority_cases()), run(&bare));
    assert_eq!(with.overall, without.overall);
    assert_eq!(with.by_horizon, without.by_horizon);
    assert!(!without.by_subset.contains_key(SUBSET_STARRED_ANY));
}

#[test]
fn compare_pairs_the_two_heuristics_inside_each_subset() {
    let cases = priority_cases();
    let history = StageSamples::default();
    let c = backtest::compare(&Fixed, &Short, &history, &cases, Filter::default(), &provenance())
        .unwrap();

    let starred = &c.paired_by_subset[SUBSET_STARRED_ANY];
    assert_eq!((starred.cases, starred.late_pairs), (2, 2));
    // Fixed (a) is late on the 2000 s case only; Short (b) on both.
    assert_eq!(starred.a_late_rate, Some(0.5));
    assert_eq!(starred.b_late_rate, Some(1.0));
    assert_eq!(starred.loss4_pairs, 2);
    assert_eq!(starred.delta4_items, 2, "one issue per case");
    let delta = starred.delta_pinball4_loss_sec.as_ref().unwrap();
    assert_eq!(delta.n, 2);

    let unstarred = &c.paired_by_subset[SUBSET_UNSTARRED_ANY];
    assert_eq!(unstarred.cases, 1);
    assert_eq!((unstarred.a_late_rate, unstarred.b_late_rate), (Some(0.0), Some(1.0)));

    // A subset's pairing is a report, never the ranking: the whole-union
    // `paired` and `better` are those of the same cases without subsets.
    let bare: Vec<ReplayCase> = cases
        .into_iter()
        .map(|mut c| {
            c.priority = None;
            c.pr_flags = None;
            c
        })
        .collect();
    let plain =
        backtest::compare(&Fixed, &Short, &history, &bare, Filter::default(), &provenance())
            .unwrap();
    assert!(plain.paired_by_subset.is_empty());
    assert_eq!(plain.paired, c.paired);
    assert_eq!(plain.better, c.better);
    let json = serde_json::to_value(&plain).unwrap();
    assert!(json.get("paired_by_subset").is_none(), "{json}");

    // With subsets, it round-trips.
    let json = serde_json::to_value(&c).unwrap();
    assert_eq!(json["paired_by_subset"][SUBSET_STARRED_ANY]["late_pairs"], 2);
    let back: backtest::Comparison = serde_json::from_value(json).unwrap();
    assert_eq!(back, c);
}
