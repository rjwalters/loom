//! The backtest's starred / held / sequenced breakdown (#10524): each case's
//! PR labels at `as_of`, point-in-time, and the per-subset coverage and late
//! surprise.

use super::{as_of, provenance, subject};
use crate::eta::backtest::subsets::{
    SUBSET_HELD, SUBSET_LABELS_KNOWN, SUBSET_SEQUENCED, SUBSET_STARRED,
};
use crate::eta::backtest::{self, pr_case_entries, Filter, ReplayCase};
use crate::eta::explanation::EstimateResult;
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
