//! Scoring.

use super::{as_of, history_a, input_at};
use crate::eta::explanation::EstimateResult;
use crate::eta::heuristics::LandV1;
use crate::eta::score::{bucket, pinball, score, OutcomeKind, StageObservation};
use crate::eta::{Heuristic, Stage};
use chrono::Duration;

fn fixed_estimate() -> crate::eta::Explanation {
    let mut explanation = LandV1.estimate(&input_at(Stage::ReviewWait, 0, 0), &history_a());
    explanation.result = Some(EstimateResult {
        p25_sec: 600,
        p50_sec: 1200,
        p75_sec: 2400,
        eta_p50_at: as_of() + Duration::seconds(1200),
        samples_min: 9,
    });
    explanation
}

fn observed() -> Vec<StageObservation> {
    vec![
        StageObservation {
            stage: Stage::ReviewWait,
            entered_at: as_of(),
            left_at: as_of() + Duration::seconds(900),
            source: "label_event".to_string(),
        },
        StageObservation {
            stage: Stage::MergeWait,
            entered_at: as_of() + Duration::seconds(900),
            left_at: as_of() + Duration::seconds(1800),
            source: "bus".to_string(),
        },
    ]
}

#[test]
fn pinball_loss_and_coverage_golden() {
    let explanation = fixed_estimate();
    let s = score(
        &explanation,
        OutcomeKind::Landed,
        as_of() + Duration::seconds(1800),
        &observed(),
    );
    assert_eq!(s.lead_sec, 1800);
    assert_eq!(s.error_sec, Some(600));
    assert_eq!(s.abs_error_sec, Some(600));
    assert_eq!(s.covered, Some(true));
    assert_eq!(s.below_p25, Some(false));
    assert_eq!(s.above_p75, Some(false));
    // ρ.25(1200) + ρ.5(600) + ρ.75(−600) = 300 + 300 + 150.
    assert_eq!(s.pinball_loss_sec, Some(750.0));
    assert_eq!(s.horizon_bucket.as_deref(), Some("15m_1h"));
    assert_eq!(s.age_bucket.as_deref(), Some("lt_15m"));
    assert_eq!(s.stage_at_estimate, Some(Stage::ReviewWait));
    assert_eq!(s.samples_min, Some(9));

    // Per-stage actuals are populated against the estimate's own distributions.
    assert_eq!(s.stages_actual.len(), 2);
    let review = &s.stages_actual[0];
    assert_eq!(review.duration_sec, 900);
    let predicted = explanation
        .stages
        .iter()
        .find(|e| e.stage == Stage::ReviewWait)
        .unwrap();
    assert_eq!(review.predicted_p50, Some(predicted.distribution.p50));
    assert_eq!(review.error_sec, Some(900 - predicted.distribution.p50));
    assert_eq!(s.stages_actual[1].source, "bus");
    assert_eq!(s.rework_rounds_actual, 0);

    // Outside the band.
    let late = score(&explanation, OutcomeKind::Landed, as_of() + Duration::seconds(3600), &[]);
    assert_eq!(late.covered, Some(false));
    assert_eq!(late.above_p75, Some(true));
    // ρ.25(3000) + ρ.5(2400) + ρ.75(1200) = 750 + 1200 + 900.
    assert_eq!(late.pinball_loss_sec, Some(2850.0));
}

#[test]
fn abandoned_is_counted_not_scored() {
    let s = score(
        &fixed_estimate(),
        OutcomeKind::Abandoned,
        as_of() + Duration::seconds(99),
        &observed(),
    );
    assert_eq!(s.outcome, OutcomeKind::Abandoned);
    assert_eq!(s.lead_sec, 99);
    assert_eq!(s.error_sec, None);
    assert_eq!(s.covered, None);
    assert_eq!(s.pinball_loss_sec, None);
    // The per-stage record is still kept: it is history, not a score.
    assert_eq!(s.stages_actual.len(), 2);
}

#[test]
fn a_refusal_scores_nothing() {
    let mut explanation = fixed_estimate();
    explanation.result = None;
    let s = score(&explanation, OutcomeKind::Landed, as_of() + Duration::seconds(60), &[]);
    assert_eq!(s.error_sec, None);
    assert_eq!(s.horizon_bucket, None);
}

#[test]
fn buckets_and_pinball_edges() {
    assert_eq!(bucket(0), "lt_15m");
    assert_eq!(bucket(899), "lt_15m");
    assert_eq!(bucket(900), "15m_1h");
    assert_eq!(bucket(3600), "1h_4h");
    assert_eq!(bucket(4 * 3600), "4h_24h");
    assert_eq!(bucket(24 * 3600), "gt_24h");
    assert_eq!(pinball(0.5, 0.0), 0.0);
    assert_eq!(pinball(0.25, -4.0), 3.0);
    assert_eq!(pinball(0.75, 4.0), 3.0);
}
