//! Frozen-policy evaluation over shadow records (#9787 step 5): compare the
//! cheap Curator baseline and retrieval thresholds at a fixed false-warning
//! budget, grouped to prevent leakage, with an explicit inconclusive verdict
//! when positives are under the pre-registered minimum.
//!
//! Outcome joining: outcomes arrive as #9786 [`OutcomeRecord`]s keyed by
//! directed/unordered pair identity. Records that never received an outcome
//! are reported as pending — never dropped, never counted as negatives.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use super::records::CaptureRecord;
use crate::collision_evidence::OutcomeKind;
use crate::collision_evidence::OutcomeRecord;

/// The frozen policy family (#9787 step 5 / decision memo §C arm 1–3). Each
/// arm fires on its own signal; thresholds were frozen before held-out
/// evaluation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FrozenPolicies {
    /// Cheap Curator baseline: fire when curator Jaccard ≥ threshold.
    pub curator_threshold: f64,
    /// Raw retrieval: fire when retrieval Jaccard ≥ threshold.
    pub retrieval_threshold: f64,
    /// Fixed false-warning budget per arm (share of evaluated pairs).
    pub max_flag_rate: f64,
}

/// Pre-registered budgets the report must be judged against.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StudyBudgets {
    pub min_positives_for_verdict: usize,
    pub max_flag_rate: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PolicyThresholds {
    pub curator: f64,
    pub retrieval: f64,
}

/// The attributed outcome for one captured pair, normalized for evaluation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JoinedOutcome {
    pub unordered_pair_id: String,
    /// Positive = the pair required substantive reconciliation (the memo's
    /// declared event); pending/censored/unknown are tracked separately.
    pub positive: bool,
    pub state: OutcomeState,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeState {
    Resolved,
    Pending,
    Censored,
    Unknown,
}

/// Map #9786 outcome kinds to the evaluation state. Only substantive
/// reconciliation counts as a positive for the shadow verdict; mechanical
/// repair and freshness churn are explicitly non-positive resolved states;
/// clean-merge observations (`Missing`) stay pending until the window
/// closes with evidence.
pub fn join_outcomes(
    captures: &[CaptureRecord],
    outcomes: &[OutcomeRecord],
) -> BTreeMap<String, JoinedOutcome> {
    let by_pair: BTreeMap<&str, &OutcomeRecord> = outcomes
        .iter()
        .map(|o| (o.unordered_pair_id.as_str(), o))
        .collect();
    let mut joined = BTreeMap::new();
    for c in captures {
        let key = c.unordered_pair_id.clone();
        if joined.contains_key(&key) {
            continue;
        }
        let outcome = by_pair.get(key.as_str()).copied();
        let state = match outcome.map(|o| &o.kind) {
            Some(OutcomeKind::SubstantiveReconciliation) => OutcomeState::Resolved,
            Some(OutcomeKind::MechanicalReconciliation)
            | Some(OutcomeKind::FreshnessOnly)
            | Some(OutcomeKind::TextualConflictPinned { .. }) => OutcomeState::Resolved,
            Some(OutcomeKind::Censored { .. }) => OutcomeState::Censored,
            Some(OutcomeKind::Pending) | Some(OutcomeKind::Missing { .. }) => OutcomeState::Pending,
            Some(_) | None => OutcomeState::Unknown,
        };
        let positive = outcome
            .map(|o| matches!(o.kind, OutcomeKind::SubstantiveReconciliation))
            .unwrap_or(false);
        let key_for_record = key.clone();
        joined.insert(
            key_for_record,
            JoinedOutcome {
                unordered_pair_id: key,
                positive,
                state,
            },
        );
    }
    joined
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ArmResult {
    pub arm: String,
    /// Pairs where the arm could produce a signal (feature available).
    pub eligible: usize,
    pub flagged: usize,
    pub tp: usize,
    pub fp: usize,
    pub fn_: usize,
    pub tn: usize,
    pub precision: Option<f64>,
    pub recall: Option<f64>,
    /// Resolution accounting: excluded pairs are pending/censored/unknown,
    /// never negatives.
    pub pending: usize,
    pub censored: usize,
    pub unknown: usize,
    /// True when the arm's flag rate exceeds the fixed false-warning
    /// budget.
    pub over_warning_budget: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvaluationReport {
    /// Published denominators: the full sample-flow accounting.
    pub captured: usize,
    pub resolved_positives: usize,
    pub pending: usize,
    pub censored: usize,
    pub unknown: usize,
    pub arms: Vec<ArmResult>,
    /// Explicit verdict: `pass`/`fail` require ≥ min positives in the
    /// resolved set AND every arm within budget; otherwise `inconclusive`.
    pub verdict: String,
    pub verdict_reason: String,
}

/// Evaluate the frozen policy family. `captures` are the shadow records;
/// `outcomes` the recorded outcomes; pairs without resolved outcomes are
/// excluded from confusion matrices but counted in the denominators.
pub fn evaluate(
    captures: &[CaptureRecord],
    outcomes: &[OutcomeRecord],
    budgets: &StudyBudgets,
    policies: &PolicyThresholds,
) -> EvaluationReport {
    let joined = join_outcomes(captures, outcomes);
    let mut positives = 0usize;
    let mut pending = 0usize;
    let mut censored = 0usize;
    let mut unknown = 0usize;
    for c in captures {
        match joined.get(&c.unordered_pair_id).map(|j| j.state) {
            Some(super::evaluate::OutcomeState::Resolved) => {
                if joined[&c.unordered_pair_id].positive {
                    positives += 1;
                }
            }
            Some(super::evaluate::OutcomeState::Pending) => pending += 1,
            Some(super::evaluate::OutcomeState::Censored) => censored += 1,
            _ => unknown += 1,
        }
    }

    let feature = |c: &CaptureRecord, which: &str| -> Option<f64> {
        match which {
            "curator" => c.features.curator_jaccard,
            "retrieval" => c.features.retrieval_jaccard,
            _ => None,
        }
    };
    let arm_result = |arm: &str, threshold: f64| -> ArmResult {
        let mut eligible = 0usize;
        let mut flagged = 0usize;
        let mut tp = 0usize;
        let mut fp = 0usize;
        let mut fn_ = 0usize;
        let mut tn = 0usize;
        let mut arm_pending = 0usize;
        let mut arm_censored = 0usize;
        let mut arm_unknown = 0usize;
        for c in captures {
            let Some(v) = feature(c, arm) else {
                continue;
            };
            eligible += 1;
            let state = joined
                .get(&c.unordered_pair_id)
                .map(|j| j.state)
                .unwrap_or(super::evaluate::OutcomeState::Unknown);
            let is_positive = joined
                .get(&c.unordered_pair_id)
                .map(|j| j.positive && j.state == super::evaluate::OutcomeState::Resolved)
                .unwrap_or(false);
            match state {
                super::evaluate::OutcomeState::Resolved => {
                    if v >= threshold {
                        flagged += 1;
                        if is_positive {
                            tp += 1
                        } else {
                            fp += 1
                        }
                    } else if is_positive {
                        fn_ += 1
                    } else {
                        tn += 1
                    }
                }
                super::evaluate::OutcomeState::Pending => arm_pending += 1,
                super::evaluate::OutcomeState::Censored => arm_censored += 1,
                _ => arm_unknown += 1,
            }
        }
        let resolved = tp + fp + fn_ + tn;
        ArmResult {
            arm: arm.into(),
            eligible,
            flagged,
            tp,
            fp,
            fn_,
            tn,
            precision: if tp + fp > 0 {
                Some(tp as f64 / (tp + fp) as f64)
            } else {
                None
            },
            recall: if tp + fn_ > 0 {
                Some(tp as f64 / (tp + fn_) as f64)
            } else {
                None
            },
            pending: arm_pending,
            censored: arm_censored,
            unknown: arm_unknown,
            over_warning_budget: resolved > 0
                && (flagged as f64 / resolved as f64) > budgets.max_flag_rate,
        }
    };

    let arms = vec![
        arm_result("curator", policies.curator),
        arm_result("retrieval", policies.retrieval),
    ];

    let enough_positives = positives >= budgets.min_positives_for_verdict;
    let (verdict, reason) = if !enough_positives {
        (
            "inconclusive".into(),
            format!(
                "only {positives} resolved positive(s) versus the pre-registered minimum {} — sparse positives are an inconclusive result, not a safety claim",
                budgets.min_positives_for_verdict
            ),
        )
    } else if arms.iter().any(|a| a.over_warning_budget) {
        ("fail".into(), "at least one arm exceeds the fixed false-warning budget".into())
    } else {
        ("pass".into(), "all arms within budget with enough positives".into())
    };

    EvaluationReport {
        captured: captures.len(),
        resolved_positives: positives,
        pending,
        censored,
        unknown,
        arms,
        verdict,
        verdict_reason: reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collision_shadow::PairFeatures;

    fn capture(pair: (u32, u32), curator: Option<f64>, retrieval: Option<f64>) -> CaptureRecord {
        CaptureRecord {
            id: format!("c-{}-{}", pair.0, pair.1),
            schema_version: 1,
            repo: "o/r".into(),
            unordered_pair_id: super::super::records::unordered_pair_id(pair.0, pair.1),
            tick_id: "t".into(),
            admission_policy: "fifo".into(),
            captured_at: "now".into(),
            issue_a: pair.0.min(pair.1),
            issue_b: pair.1.max(pair.0),
            claim_state: "eligible/eligible".into(),
            dispatch_eligible: true,
            implementation_evidence_disclosed: "a=false/b=false".into(),
            features: PairFeatures {
                curator_jaccard: curator,
                retrieval_jaccard: retrieval,
            },
            exposure: crate::collision_evidence::records::SchedulingExposure::AdvisoryOnly,
            notes: vec![],
        }
    }

    fn outcome(pair: (u32, u32), kind: OutcomeKind) -> OutcomeRecord {
        OutcomeRecord {
            id: format!("o-{}-{}", pair.0, pair.1),
            schema_version: 1,
            directed_eval_id: "e".into(),
            unordered_pair_id: super::super::records::unordered_pair_id(pair.0, pair.1),
            repo: "o/r".into(),
            kind,
            evidence_refs: vec![],
            observation_window: ("a".into(), "b".into()),
            attribution: crate::collision_evidence::records::Attribution {
                event_id: format!("ev-{}", pair.0),
                share: 1.0,
                note: None,
            },
            recorded_at: "t".into(),
        }
    }

    #[test]
    fn pending_and_censored_are_never_negatives() {
        let captures = vec![
            capture((1, 2), Some(0.5), Some(0.5)),
            capture((3, 4), Some(0.5), Some(0.5)),
            capture((5, 6), Some(0.5), Some(0.5)),
        ];
        let outcomes = vec![
            outcome((1, 2), OutcomeKind::SubstantiveReconciliation),
            outcome(
                (3, 4),
                OutcomeKind::Censored {
                    reason: "serialized".into(),
                },
            ),
            outcome((5, 6), OutcomeKind::Pending),
        ];
        let budgets = StudyBudgets {
            min_positives_for_verdict: 30,
            max_flag_rate: 0.9,
        };
        let policies = PolicyThresholds {
            curator: 0.3,
            retrieval: 0.3,
        };
        let report = evaluate(&captures, &outcomes, &budgets, &policies);
        assert_eq!(report.pending, 1);
        assert_eq!(report.censored, 1);
        for arm in &report.arms {
            assert_eq!(arm.pending + arm.censored, 2, "excluded from confusion");
            assert_eq!(arm.tn, 0, "pending/censored are never negatives");
            assert_eq!(arm.tp, 1);
        }
    }

    #[test]
    fn sparse_positives_are_inconclusive() {
        let captures = vec![capture((1, 2), Some(0.5), None)];
        let outcomes = vec![outcome((1, 2), OutcomeKind::SubstantiveReconciliation)];
        let budgets = StudyBudgets {
            min_positives_for_verdict: 30,
            max_flag_rate: 0.5,
        };
        let policies = PolicyThresholds {
            curator: 0.3,
            retrieval: 0.3,
        };
        let report = evaluate(&captures, &outcomes, &budgets, &policies);
        assert_eq!(report.verdict, "inconclusive");
        assert!(report.verdict_reason.contains("only 1 resolved positive"));
    }

    #[test]
    fn over_budget_flag_rate_fails_the_verdict() {
        let mut captures = Vec::new();
        for i in 0..40 {
            captures.push(capture((i, i + 100), Some(0.5), None));
        }
        let mut outcomes = Vec::new();
        // 30 positives (meets the pre-registered minimum) + 10
        // freshness-only, all flagged by the curator arm → over the 10%
        // warning budget → fail.
        for i in 0..30 {
            outcomes.push(outcome((i, i + 100), OutcomeKind::SubstantiveReconciliation));
        }
        for i in 30..40 {
            outcomes.push(outcome((i, i + 100), OutcomeKind::FreshnessOnly));
        }
        let budgets = StudyBudgets {
            min_positives_for_verdict: 30,
            max_flag_rate: 0.1,
        };
        let policies = PolicyThresholds {
            curator: 0.3,
            retrieval: 0.3,
        };
        let report = evaluate(&captures, &outcomes, &budgets, &policies);
        // The retrieval arm has no features in this fixture (eligible == 0),
        // so it cannot be over any budget — only arms with signal are judged.
        assert!(report
            .arms
            .iter()
            .filter(|a| a.eligible > 0)
            .all(|a| a.over_warning_budget));
        assert_eq!(report.verdict, "fail");
    }
}
