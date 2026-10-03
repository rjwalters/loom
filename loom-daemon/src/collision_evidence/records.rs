//! The #9786 record schemas. Deterministic ids are computed by
//! [`crate::collision_evidence::record_id`] over the canonical form (the
//! `id` field excluded).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const PREDICTION_SCHEMA_VERSION: u32 = 1;
pub const OUTCOME_SCHEMA_VERSION: u32 = 1;

pub trait HasId {
    fn set_id(&mut self, id: String);
}

/// Unordered pair identity: the same two issues in either order produce the
/// same id (#9786: "maintain an unordered pair identity and a directed
/// evaluation identity; deterministic replay/export must not inflate
/// counts").
pub fn unordered_pair_id(a: u32, b: u32) -> String {
    use sha2::{Digest, Sha256};
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    let mut h = Sha256::new();
    h.update(format!("collision-pair-v1:{lo}:{hi}").as_bytes());
    hex::encode(h.finalize())
}

/// Directed evaluation identity: orientation + base, so two landing orders
/// or two bases are distinct observations that never conflate.
pub fn directed_eval_id(a: u32, b: u32, base_sha: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(format!("collision-eval-v1:{a}:{b}:{base_sha}").as_bytes());
    hex::encode(h.finalize())
}

/// Resolvable reference into the durable artifact cache/export bundle
/// (#9786: "full raw responses remain in the durable artifact cache;
/// provide a resolvable artifact reference and clearly report
/// expiry/unavailability").
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ArtifactRef {
    /// `retrieval` | `footprint` | `index-manifest` | …
    pub kind: String,
    pub sha256: String,
    /// Resolvable URI or cache key at publication time.
    pub uri: String,
    /// Explicit availability note — expiry/unavailable is data, never a
    /// silent gap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub availability: Option<String>,
}

/// How the prediction was exposed to scheduling at capture time. Shadow
/// records must never be able to claim enforcement.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SchedulingExposure {
    /// Recorded only; no scheduling path read it.
    AdvisoryOnly,
    /// Surfaced to a human/agent advisory surface (report/comment).
    AdvisorySurfaced,
    /// An existing non-collision rule (dependencies, sequencing) already
    /// gated this pair — recorded so confounders are visible.
    ExistingRuleApplied,
}

/// One frozen prediction for a directed evaluation of an issue pair.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PredictionRecord {
    /// Deterministic id (computed at publish time).
    pub id: String,
    pub schema_version: u32,
    /// `OWNER/REPO`.
    pub repo: String,
    /// Forge identity (`github`/`gitea`).
    pub forge: String,
    /// Unordered pair identity — invariant to side order.
    pub unordered_pair_id: String,
    /// Directed evaluation identity (orientation + base).
    pub directed_eval_id: String,
    /// Issue number → canonical content hash at prediction time.
    pub issue_content_hashes: BTreeMap<u32, String>,
    /// Retrieval artifact reference + manifest hash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retrieval_artifact: Option<ArtifactRef>,
    /// Footprint artifact reference + manifest hash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub footprint_artifact: Option<ArtifactRef>,
    /// The pinned source revision retrieval ran against.
    pub source_sha: String,
    pub index_identity: String,
    /// The recorded base the prediction assumed.
    pub base_sha: String,
    /// Actual PR heads joined later (kept optional — predictions are frozen
    /// before outcomes exist).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_pr_heads: Option<BTreeMap<u32, String>>,
    pub predicted_at: String,
    pub scorer_version: String,
    /// Frozen query-policy version.
    pub policy_version: String,
    /// The intended landing order the evaluation assumes.
    pub intended_order: Vec<u32>,
    pub exposure: SchedulingExposure,
    /// Named feature values at prediction time (e.g. file_jaccard).
    pub features: BTreeMap<String, f64>,
}

impl HasId for PredictionRecord {
    fn set_id(&mut self, id: String) {
        self.id = id;
    }
}

/// Explicit outcome taxonomy (#9786: distinguish textual conflict on pinned
/// trees, evidenced semantic interaction, mechanical/substantive
/// reconciliation, extra rounds, CI work, freshness churn, and
/// pending/censored/unknown attribution). A clean textual merge does not
/// prove semantic compatibility, and a missing marker is missing
/// observation — never zero work.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum OutcomeKind {
    /// Textual conflict on pinned trees (counterfactual replay).
    TextualConflictPinned {
        conflicted_files: Vec<String>,
        replay_provenance: String,
    },
    /// Evidenced semantic interaction (component controls or adjudicated
    /// reconciliation evidence — never a failed combined CI run alone).
    SemanticInteractionEvidenced { evidence_refs: Vec<String> },
    /// Mechanical reconciliation only (hub-file churn etc.).
    MechanicalReconciliation,
    /// Substantive reconciliation happened (non-hub repair attributable to
    /// the pair).
    SubstantiveReconciliation,
    /// Extra review/Judge rounds attributed to the pair.
    ExtraReviewRounds { rounds: u32 },
    /// CI rerun/work attributed to the pair.
    CiRerun {
        jobs: u32,
        runner_minutes: Option<f64>,
    },
    /// Pure freshness invalidation (rebase churn, no substantive repair).
    FreshnessOnly,
    /// The pair never reached integration in the window.
    Pending,
    /// Observation censored (intervention applied, e.g. serialized by an
    /// existing rule — the concurrent outcome is not observable).
    Censored { reason: String },
    /// Observation window closed without a definite marker.
    Missing {
        /// The window in which the observation was sought.
        window: String,
    },
    /// Unknown attribution — evidence present but not attributable to this
    /// pair with confidence.
    Unknown { reason: String },
}

/// Cost attribution: a repair/CI event has a **unique event id** and an
/// explicit share, so one repair is never fully charged to every
/// overlapping pair (#9786: "use unique event identities and explicit
/// attribution/unknown shares"). Shares referencing the same event across
/// pairs must sum to ≤ 1; the remainder is the unknown share.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Attribution {
    pub event_id: String,
    pub share: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// One attributed outcome for a directed evaluation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OutcomeRecord {
    /// Deterministic id (computed at publish time).
    pub id: String,
    pub schema_version: u32,
    pub directed_eval_id: String,
    pub unordered_pair_id: String,
    pub repo: String,
    pub kind: OutcomeKind,
    /// Links to evidence artifacts (replay JSON, component controls,
    /// adjudication notes).
    #[serde(default)]
    pub evidence_refs: Vec<ArtifactRef>,
    /// (start, end) of the observation window.
    pub observation_window: (String, String),
    pub attribution: Attribution,
    pub recorded_at: String,
}

impl HasId for OutcomeRecord {
    fn set_id(&mut self, id: String) {
        self.id = id;
    }
}

/// Validate outcome records against the attribution invariant: shares
/// referencing the same event id sum to ≤ 1 (+#9786's multi-pair cost
/// deduplication test).
pub fn validate_attribution(outcomes: &[OutcomeRecord]) -> Result<(), String> {
    let mut per_event: BTreeMap<&str, f64> = BTreeMap::new();
    for o in outcomes {
        let share = o.attribution.share;
        if !(0.0..=1.0).contains(&share) {
            return Err(format!("outcome {} has an out-of-range attribution share {share}", o.id));
        }
        *per_event
            .entry(o.attribution.event_id.as_str())
            .or_insert(0.0) += share;
    }
    for (event, total) in &per_event {
        if *total > 1.0 + 1e-9 {
            return Err(format!(
                "event {event} is charged {total} across pairs — shares must sum to ≤ 1"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(event: &str, share: f64) -> OutcomeRecord {
        OutcomeRecord {
            id: String::new(),
            schema_version: OUTCOME_SCHEMA_VERSION,
            directed_eval_id: "e".into(),
            unordered_pair_id: "p".into(),
            repo: "o/r".into(),
            kind: OutcomeKind::CiRerun {
                jobs: 1,
                runner_minutes: Some(2.0),
            },
            evidence_refs: vec![],
            observation_window: ("a".into(), "b".into()),
            attribution: Attribution {
                event_id: event.into(),
                share,
                note: None,
            },
            recorded_at: "t".into(),
        }
    }

    #[test]
    fn shares_over_one_are_rejected() {
        assert!(validate_attribution(&[outcome("e1", 0.5), outcome("e1", 0.5)]).is_ok());
        assert!(validate_attribution(&[outcome("e1", 0.6), outcome("e1", 0.5)]).is_err());
        assert!(validate_attribution(&[outcome("e1", 0.5), outcome("e2", 0.5)]).is_ok());
        assert!(validate_attribution(&[outcome("e1", 1.5)]).is_err());
    }

    #[test]
    fn pair_identity_is_order_invariant() {
        assert_eq!(unordered_pair_id(3, 1), unordered_pair_id(1, 3));
    }
}
