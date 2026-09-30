//! Scoring an estimate against what happened.
//!
//! The deciding metric is the pinball (quantile) loss, the proper scoring
//! rule for quantile forecasts: `Σ_{q ∈ {.25, .5, .75}} ρ_q(actual − q̂)` with
//! `ρ_q(u) = u · (q − 1[u < 0])`. Error, coverage and the buckets ride beside
//! it for readability.
//!
//! An `abandoned` outcome (the issue closed as not planned) is counted but
//! never scored: its error fields are absent, not zero.

use super::explanation::Explanation;
use super::{Kind, NoEstimateReason, Provenance, Stage};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// How the predicted event resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeKind {
    /// `start`: the issue's sweep was dispatched (#9326).
    Started,
    /// `land`: PR merged, or issue closed as completed.
    Landed,
    /// `finish`: the sweep reached a terminal state (any result).
    Finished,
    /// `land`: the issue closed as not planned. Counted, never scored. A PR
    /// closed unmerged is **not** this: the issue decides.
    Abandoned,
}

/// One stage transition the tracker observed for the subject.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageObservation {
    /// The stage.
    pub stage: Stage,
    /// When it was entered.
    pub entered_at: DateTime<Utc>,
    /// When it was left.
    pub left_at: DateTime<Utc>,
    /// Where the transition was seen (`bus`, `checkpoint`, `label_event`, …).
    pub source: String,
}

/// Per-stage actual vs. predicted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageActual {
    /// The stage.
    pub stage: Stage,
    /// Entered.
    pub entered_at: DateTime<Utc>,
    /// Left.
    pub left_at: DateTime<Utc>,
    /// Whole seconds in the stage.
    pub duration_sec: i64,
    /// The stage distribution's quartiles, when the estimate had the stage.
    pub predicted_p25: Option<i64>,
    /// Median.
    pub predicted_p50: Option<i64>,
    /// Upper quartile.
    pub predicted_p75: Option<i64>,
    /// `duration − predicted_p50`.
    pub error_sec: Option<i64>,
    /// Where the transition was seen.
    pub source: String,
}

/// An estimate's score.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Score {
    /// How it resolved.
    pub outcome: OutcomeKind,
    /// When.
    pub actual_at: DateTime<Utc>,
    /// Seconds from `as_of` to `actual_at`.
    pub lead_sec: i64,
    /// `actual − p50`. Absent when abandoned or refused.
    pub error_sec: Option<i64>,
    /// `|error|`.
    pub abs_error_sec: Option<i64>,
    /// `p25 ≤ actual ≤ p75`.
    pub covered: Option<bool>,
    /// `actual < p25`.
    pub below_p25: Option<bool>,
    /// `actual > p75`.
    pub above_p75: Option<bool>,
    /// Summed pinball loss over the three quantiles, seconds.
    pub pinball_loss_sec: Option<f64>,
    /// Bucket of the predicted p50.
    pub horizon_bucket: Option<String>,
    /// Bucket of the current stage's age at the estimate.
    pub age_bucket: Option<String>,
    /// The stage at the estimate.
    pub stage_at_estimate: Option<Stage>,
    /// Smallest distribution `n` the estimate used.
    pub samples_min: Option<usize>,
    /// Per-stage actuals.
    pub stages_actual: Vec<StageActual>,
    /// Rework rounds actually taken after the estimate.
    pub rework_rounds_actual: u32,
}

/// `<15m`, `15m–1h`, `1–4h`, `4–24h`, `>24h`.
#[must_use]
pub fn bucket(seconds: i64) -> &'static str {
    match seconds {
        s if s < 15 * 60 => "lt_15m",
        s if s < 3600 => "15m_1h",
        s if s < 4 * 3600 => "1h_4h",
        s if s < 24 * 3600 => "4h_24h",
        _ => "gt_24h",
    }
}

/// `ρ_q(u) = u · (q − 1[u < 0])`.
#[must_use]
pub fn pinball(q: f64, u: f64) -> f64 {
    u * (q - if u < 0.0 { 1.0 } else { 0.0 })
}

/// What scoring needs from an estimate — the slim form pending estimates
/// persist, so a restart does not lose the join (and so a pending estimate
/// does not keep its whole explanation in memory).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EstimateSummary {
    /// Derived id.
    pub estimate_id: String,
    /// What was predicted.
    pub kind: Kind,
    /// Heuristic id.
    pub heuristic: String,
    /// The estimating build (required).
    pub loom: Provenance,
    /// `owner/repo`.
    pub repo: String,
    /// GitHub numeric repo id, when known.
    pub repo_id: Option<u64>,
    /// Issue.
    pub issue: u32,
    /// The PR, when one existed.
    pub pr_number: Option<u32>,
    /// The instant estimated.
    pub as_of: DateTime<Utc>,
    /// The stage at the estimate.
    pub stage: Option<Stage>,
    /// Its age.
    pub age_sec: Option<i64>,
    /// Remaining seconds, 25th percentile. Absent on a refusal.
    pub p25_sec: Option<i64>,
    /// Median.
    pub p50_sec: Option<i64>,
    /// 75th percentile.
    pub p75_sec: Option<i64>,
    /// Smallest distribution `n`.
    pub samples_min: Option<usize>,
    /// Why there was no estimate.
    pub no_estimate_reason: Option<NoEstimateReason>,
    /// Each path stage's `(p25, p50, p75)` duration.
    pub stage_quartiles: Vec<StageQuartiles>,
}

/// One stage's predicted duration quartiles.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageQuartiles {
    /// The stage.
    pub stage: Stage,
    /// 25th percentile.
    pub p25: i64,
    /// Median.
    pub p50: i64,
    /// 75th percentile.
    pub p75: i64,
}

impl EstimateSummary {
    /// The summary of `explanation`.
    #[must_use]
    pub fn of(explanation: &Explanation) -> Self {
        let quantiles = explanation.quantiles();
        EstimateSummary {
            estimate_id: explanation.estimate_id.clone(),
            kind: explanation.kind,
            heuristic: explanation.heuristic.clone(),
            loom: explanation.loom.clone(),
            repo: explanation.subject.repo.clone(),
            repo_id: explanation.subject.repo_id,
            issue: explanation.subject.issue,
            pr_number: explanation.subject.pr_number,
            as_of: explanation.as_of,
            stage: explanation.current_stage.as_ref().map(|c| c.stage),
            age_sec: explanation.current_stage.as_ref().map(|c| c.age_sec),
            p25_sec: quantiles.map(|q| q.0),
            p50_sec: quantiles.map(|q| q.1),
            p75_sec: quantiles.map(|q| q.2),
            samples_min: explanation.result.as_ref().map(|r| r.samples_min),
            no_estimate_reason: explanation.no_estimate_reason,
            stage_quartiles: explanation
                .stages
                .iter()
                .map(|e| StageQuartiles {
                    stage: e.stage,
                    p25: e.distribution.p25,
                    p50: e.distribution.p50,
                    p75: e.distribution.p75,
                })
                .collect(),
        }
    }

    /// `(p25, p50, p75)` when there was an estimate.
    #[must_use]
    pub fn quantiles(&self) -> Option<(i64, i64, i64)> {
        Some((self.p25_sec?, self.p50_sec?, self.p75_sec?))
    }
}

/// Score `estimate` against its resolution at `actual_at`, with the stages
/// the tracker observed after the estimate.
#[must_use]
pub fn score(
    estimate: &EstimateSummary,
    outcome: OutcomeKind,
    actual_at: DateTime<Utc>,
    observed: &[StageObservation],
) -> Score {
    let lead_sec = (actual_at - estimate.as_of).num_seconds();
    let stages_actual: Vec<StageActual> = observed
        .iter()
        .map(|o| {
            let duration_sec = (o.left_at - o.entered_at).num_seconds().max(0);
            let predicted = estimate
                .stage_quartiles
                .iter()
                .find(|q| q.stage == o.stage)
                .map(|q| (q.p25, q.p50, q.p75));
            StageActual {
                stage: o.stage,
                entered_at: o.entered_at,
                left_at: o.left_at,
                duration_sec,
                predicted_p25: predicted.map(|p| p.0),
                predicted_p50: predicted.map(|p| p.1),
                predicted_p75: predicted.map(|p| p.2),
                error_sec: predicted.map(|p| duration_sec - p.1),
                source: o.source.clone(),
            }
        })
        .collect();
    let rework_rounds_actual = observed.iter().filter(|o| o.stage == Stage::Doctor).count() as u32;

    let scored = match outcome {
        OutcomeKind::Abandoned => None,
        OutcomeKind::Started | OutcomeKind::Landed | OutcomeKind::Finished => estimate.quantiles(),
    };
    let age_bucket = estimate.age_sec.map(|a| bucket(a).to_string());
    let stage_at_estimate = estimate.stage;
    let samples_min = estimate.samples_min;

    let mut result = Score {
        outcome,
        actual_at,
        lead_sec,
        error_sec: None,
        abs_error_sec: None,
        covered: None,
        below_p25: None,
        above_p75: None,
        pinball_loss_sec: None,
        horizon_bucket: estimate.p50_sec.map(|p50| bucket(p50).to_string()),
        age_bucket,
        stage_at_estimate,
        samples_min,
        stages_actual,
        rework_rounds_actual,
    };
    if let Some((p25, p50, p75)) = scored {
        let actual = lead_sec;
        let error = actual - p50;
        result.error_sec = Some(error);
        result.abs_error_sec = Some(error.abs());
        result.covered = Some(p25 <= actual && actual <= p75);
        result.below_p25 = Some(actual < p25);
        result.above_p75 = Some(actual > p75);
        let a = actual as f64;
        result.pinball_loss_sec = Some(
            pinball(0.25, a - p25 as f64)
                + pinball(0.5, a - p50 as f64)
                + pinball(0.75, a - p75 as f64),
        );
    }
    result
}
