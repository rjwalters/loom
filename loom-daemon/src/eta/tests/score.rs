//! Scoring.

use super::{as_of, history_a, input_at};
use crate::eta::explanation::EstimateResult;
use crate::eta::heuristics::LandV1;
use crate::eta::score::{bucket, pinball, score, EstimateSummary, OutcomeKind, StageObservation};
use crate::eta::{Heuristic, Stage};
use chrono::Duration;

fn fixed_estimate() -> EstimateSummary {
    let mut explanation = LandV1.estimate(&input_at(Stage::ReviewWait, 0, 0), &history_a());
    explanation.result = Some(EstimateResult {
        p25_sec: 600,
        p50_sec: 1200,
        p75_sec: 2400,
        p90_sec: Some(3600),
        eta_p50_at: as_of() + Duration::seconds(1200),
        samples_min: 9,
        stage_marks: Vec::new(),
    });
    EstimateSummary::of(&explanation)
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
        .stage_quartiles
        .iter()
        .find(|q| q.stage == Stage::ReviewWait)
        .unwrap();
    assert_eq!(review.predicted_p50, Some(predicted.p50));
    assert_eq!(review.error_sec, Some(900 - predicted.p50));
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
    explanation.p25_sec = None;
    explanation.p50_sec = None;
    explanation.p75_sec = None;
    // `p90_sec` deliberately left set: a p90 alone never makes a refusal
    // scored.
    let s = score(&explanation, OutcomeKind::Landed, as_of() + Duration::seconds(60), &[]);
    assert_eq!(s.error_sec, None);
    assert_eq!(s.horizon_bucket, None);
    assert_eq!(s.above_p90, None);
    assert_eq!(s.pinball4_loss_sec, None);
}

// ------------------------------------------------------------ p90 (#10211)

/// `fixed_estimate` scored at `actual` seconds after its `as_of`.
fn landed_at(actual: i64) -> crate::eta::score::Score {
    score(&fixed_estimate(), OutcomeKind::Landed, as_of() + Duration::seconds(actual), &[])
}

#[test]
fn a_landing_after_p90_is_a_late_surprise() {
    // p25/p50/p75/p90 = 600/1200/2400/3600; landed at 4200.
    let s = landed_at(4200);
    assert_eq!(s.above_p75, Some(true));
    assert_eq!(s.above_p90, Some(true));
    // ρ.25(3600) + ρ.5(3000) + ρ.75(1800) = 900 + 1500 + 1350.
    assert_eq!(s.pinball_loss_sec, Some(3750.0));
    // … plus ρ.9(600) = 540.
    assert_eq!(s.pinball4_loss_sec, Some(4290.0));
    assert_eq!(
        s.pinball4_loss_sec,
        Some(s.pinball_loss_sec.unwrap() + pinball(0.9, 4200.0 - 3600.0)),
        "the four-quantile loss is the three-quantile one plus the p90 term"
    );
}

#[test]
fn landing_exactly_at_p90_is_not_late() {
    // Strict: `actual > p90`, mirroring `above_p75`.
    let s = landed_at(3600);
    assert_eq!(s.above_p90, Some(false));
    assert_eq!(s.above_p75, Some(true));
    // ρ.25(3000) + ρ.5(2400) + ρ.75(1200) = 750 + 1200 + 900, and ρ.9(0) = 0.
    assert_eq!(s.pinball_loss_sec, Some(2850.0));
    assert_eq!(s.pinball4_loss_sec, Some(2850.0));
}

#[test]
fn a_landing_inside_the_band_pays_the_p90_overestimate_term() {
    let s = landed_at(1800);
    assert_eq!(s.above_p90, Some(false));
    assert_eq!(s.pinball_loss_sec, Some(750.0), "the three-quantile loss is unchanged");
    // ρ.9(1800 − 3600) = −1800 · (0.9 − 1) = 180.
    assert_eq!(s.pinball4_loss_sec, Some(930.0));
}

#[test]
fn p90_scores_are_absent_never_false_when_there_is_nothing_to_score() {
    // Abandoned: counted, never scored.
    let abandoned = score(
        &fixed_estimate(),
        OutcomeKind::Abandoned,
        as_of() + Duration::seconds(9999),
        &[],
    );
    assert_eq!((abandoned.above_p90, abandoned.pinball4_loss_sec), (None, None));

    // A summary without a p90 (persisted before #10211): the three-quantile
    // score is unchanged, the p90 fields are absent rather than `false`/0.
    let mut old = fixed_estimate();
    old.p90_sec = None;
    let s = score(&old, OutcomeKind::Landed, as_of() + Duration::seconds(9999), &[]);
    assert_eq!(s.above_p75, Some(true));
    assert!(s.pinball_loss_sec.is_some());
    assert_eq!((s.above_p90, s.pinball4_loss_sec), (None, None));
}

#[test]
fn records_written_before_p90_still_parse() {
    // A pending-estimate line from before the field existed.
    let mut line = serde_json::to_value(fixed_estimate()).unwrap();
    assert!(line.as_object_mut().unwrap().remove("p90_sec").is_some());
    let old: EstimateSummary = serde_json::from_value(line).unwrap();
    assert_eq!(old.p90_sec, None);
    assert_eq!(old.quantiles(), Some((600, 1200, 2400)));

    // A score from before the fields existed (a spooled `eta.outcome`).
    let mut scored = serde_json::to_value(landed_at(4200)).unwrap();
    let object = scored.as_object_mut().unwrap();
    assert!(object.remove("above_p90").is_some());
    assert!(object.remove("pinball4_loss_sec").is_some());
    let parsed: crate::eta::score::Score = serde_json::from_value(scored).unwrap();
    assert_eq!((parsed.above_p90, parsed.pinball4_loss_sec), (None, None));
    assert_eq!(parsed.pinball_loss_sec, Some(3750.0));
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
