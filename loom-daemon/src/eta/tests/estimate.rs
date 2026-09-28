//! The heuristics over fixture and synthetic history.

use super::{as_of, history_a, input_at};
use crate::eta::heuristics::{FinishV1, LandV1};
use crate::eta::history::{SampleSource, StageSample, StageSamples, VerdictSample};
use crate::eta::{CurrentState, Heuristic, NoEstimateReason, Stage, MIN_SAMPLES};
use chrono::Duration;

/// A history of `n` samples per stage (durations `base, 2·base, …`) and
/// `n` first-attempt verdicts, a third of them rejections.
fn synthetic(n: usize, base: i64) -> StageSamples {
    let mut samples = StageSamples::default();
    for stage in Stage::ALL {
        for i in 0..n {
            samples.stages.push(StageSample {
                repo: "rjwalters/loom".to_string(),
                stage,
                duration_sec: base * (i as i64 + 1),
                observed_at: as_of() - Duration::hours(i as i64 + 1),
                source: SampleSource::SweepOutcome,
            });
        }
    }
    for i in 0..n {
        samples.verdicts.push(VerdictSample {
            repo: "rjwalters/loom".to_string(),
            attempt: 1,
            rejected: i % 3 == 0,
            observed_at: as_of() - Duration::hours(i as i64 + 1),
        });
    }
    samples
}

#[test]
fn land_v1_golden_quantiles_fixture_a() {
    let explanation = LandV1.estimate(&input_at(Stage::ReviewWait, 0, 0), &history_a());
    assert_eq!(explanation.no_estimate_reason, None);
    // The immutability guard for `land-v1`: these literals may never change.
    // A different number is a different heuristic, with a new id.
    assert_eq!(explanation.quantiles(), Some(LAND_V1_GOLDEN), "land-v1 output moved");
}

#[test]
fn finish_v1_golden_quantiles_fixture_a() {
    let explanation = FinishV1.estimate(&input_at(Stage::SweepBuilder, 0, 0), &history_a());
    assert_eq!(explanation.no_estimate_reason, None);
    assert_eq!(explanation.quantiles(), Some(FINISH_V1_GOLDEN), "finish-v1 output moved");
}

const LAND_V1_GOLDEN: (i64, i64, i64) = (943, 2004, 3552);
const FINISH_V1_GOLDEN: (i64, i64, i64) = (3686, 5559, 7541);

#[test]
fn history_a_reads_only_the_window_before_as_of() {
    let history = history_a();
    // The fixture carries 99999 s sweeps observed before the window and after
    // as_of; neither may reach a distribution.
    for stage in Stage::ALL {
        if let Some(selection) = history.select(
            "rjwalters/loom",
            stage,
            as_of(),
            &[SampleSource::SweepOutcome, SampleSource::StageJournal],
        ) {
            assert!(
                selection.sorted.iter().all(|&d| d < 99_999),
                "{stage}: {:?}",
                selection.sorted
            );
        }
    }
    // Leak-free: moving as_of past the future sample lets it in.
    let later = as_of() + Duration::days(2);
    let selection = history
        .select("rjwalters/loom", Stage::MergeWait, later, &[SampleSource::SweepOutcome])
        .unwrap();
    assert!(selection.sorted.contains(&99_999));
}

#[test]
fn journal_fallback_record_is_not_a_stage_sample() {
    // history-a's issue 8998 record has one `builder` entry spanning the whole
    // sweep: the journal's no-phase-sampled fallback, not a builder duration.
    let history = history_a();
    assert!(!history
        .stages
        .iter()
        .any(|s| s.stage == Stage::SweepBuilder && s.duration_sec == 5400));
}

#[test]
fn conditioning_truncates_and_never_goes_negative() {
    let history = synthetic(40, 60); // review_wait samples 60..=2400 s
    let fresh = LandV1.estimate(&input_at(Stage::ReviewWait, 0, 0), &history);
    let aged = LandV1.estimate(&input_at(Stage::ReviewWait, 1200, 0), &history);
    let (f25, f50, f75) = fresh.quantiles().unwrap();
    let (a25, a50, a75) = aged.quantiles().unwrap();
    assert!(a25 >= 0 && a50 >= 0 && a75 >= 0);
    assert!(f25 <= f50 && f50 <= f75 && a25 <= a50 && a50 <= a75);
    let conditioning = aged.stages[0].conditioning.as_ref().unwrap();
    assert_eq!(conditioning.age_sec, 1200);
    assert_eq!(conditioning.n_above, 20); // 1260..=2400
    assert!(conditioning.f_age > 0.45 && conditioning.f_age < 0.55, "{}", conditioning.f_age);
    assert!(fresh.stages[0].conditioning.is_none(), "age 0 is unconditioned");

    // Conditioning at an age near the top of the history: every remaining
    // draw is still ≥ 0 (the conditioned inverse CDF is ≥ the age).
    let late = LandV1.estimate(&input_at(Stage::MergeWait, 2100, 0), &history);
    let (l25, l50, l75) = late.quantiles().unwrap();
    assert!(l25 >= 0 && l50 >= 0 && l75 >= 0);
    assert!(l75 <= 2400 - 2100 + 1, "remaining never exceeds max − age: {l75}");
}

#[test]
fn beyond_history_returns_none() {
    let history = synthetic(40, 60);
    // 2400 s is the longest review_wait; at 2350 s only 1 sample is longer.
    let explanation = LandV1.estimate(&input_at(Stage::ReviewWait, 2350, 0), &history);
    assert_eq!(explanation.no_estimate_reason, Some(NoEstimateReason::BeyondHistory));
    assert_eq!(explanation.quantiles(), None);
    assert_eq!(explanation.result, None);
    // The evaluated stage is still explained.
    assert_eq!(explanation.stages.len(), 1);
    assert_eq!(explanation.stages[0].conditioning.as_ref().unwrap().n_above, 1);
}

#[test]
fn insufficient_samples_returns_none() {
    let below = synthetic(MIN_SAMPLES - 1, 60);
    assert_eq!(MIN_SAMPLES, 8);
    let explanation = LandV1.estimate(&input_at(Stage::ReviewWait, 0, 0), &below);
    assert_eq!(explanation.no_estimate_reason, Some(NoEstimateReason::InsufficientSamples));
    assert_eq!(explanation.quantiles(), None);

    let at_floor = synthetic(MIN_SAMPLES, 60);
    let explanation = LandV1.estimate(&input_at(Stage::ReviewWait, 0, 0), &at_floor);
    assert_eq!(explanation.no_estimate_reason, None);
    assert!(explanation.quantiles().is_some());
    assert_eq!(explanation.result.as_ref().unwrap().samples_min, MIN_SAMPLES);
}

#[test]
fn a_refused_state_carries_its_reason_and_no_numbers() {
    let mut input = input_at(Stage::ReviewWait, 0, 0);
    input.current = CurrentState::Refused(NoEstimateReason::Blocked);
    let explanation = LandV1.estimate(&input, &history_a());
    assert_eq!(explanation.no_estimate_reason, Some(NoEstimateReason::Blocked));
    assert_eq!(explanation.result, None);
    assert!(explanation.stages.is_empty());
    assert_eq!(explanation.loom, input.provenance, "a refusal still names its build");
}

#[test]
fn host_level_fallback_is_recorded() {
    // rjwalters/other has 6 sweeps: below the floor at repo level.
    let mut input = input_at(Stage::SweepBuilder, 0, 0);
    input.subject = crate::eta::Subject::new("rjwalters/other", None, 101);
    let explanation = FinishV1.estimate(&input, &history_a());
    assert_eq!(explanation.no_estimate_reason, None);
    for entry in &explanation.stages {
        assert_eq!(entry.distribution.filters.level, "host");
        assert_eq!(entry.distribution.filters.repo, None);
    }
}

#[test]
fn at_the_rework_cap_no_verdict_is_drawn() {
    let history = synthetic(40, 60);
    let explanation =
        LandV1.estimate(&input_at(Stage::Doctor, 0, crate::eta::MAX_REWORK_ROUNDS), &history);
    let branches = explanation.branches.as_ref().unwrap();
    assert_eq!(branches.changes_requested.expected_rework_rounds, Some(0.0));
    let stages: Vec<Stage> = explanation.stages.iter().map(|e| e.stage).collect();
    assert_eq!(stages, vec![Stage::Doctor, Stage::ReviewWait, Stage::MergeWait]);
}

#[test]
fn finish_ends_at_the_verdict_when_sweeps_do_not_merge() {
    let mut history = synthetic(40, 60);
    for i in 0..10 {
        history.paths.push(crate::eta::history::SweepPathSample {
            repo: "rjwalters/loom".to_string(),
            merged_in_sweep: i < 3,
            observed_at: as_of() - Duration::hours(i + 1),
        });
    }
    let explanation = FinishV1.estimate(&input_at(Stage::SweepBuilder, 0, 0), &history);
    let path = explanation.path.as_ref().unwrap();
    assert!(!path.include_merge);
    assert_eq!(path.terminal, Stage::ReviewWait);
    assert_eq!(path.merge_share, Some(0.3));
    assert!(!explanation
        .stages
        .iter()
        .any(|e| e.stage == Stage::MergeWait));
}
