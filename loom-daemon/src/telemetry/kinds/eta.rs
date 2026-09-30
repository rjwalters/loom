//! `eta.estimate` / `eta.outcome` (#9289): per-issue ETA estimates and their
//! scored outcomes.
//!
//! **OTLP only** (operator decision on #9289): explanations and outcomes live
//! in SigNoz, and loom-ui reads them there. The dashboard's live list is a
//! separate host-scoped record (#9329). Per-issue rows are high-cardinality,
//! so they are log records, never `metric.points`.
//!
//! **Provenance is required.** Both kinds carry the computing build's
//! version, full git revision and tree state as non-optional fields, sourced
//! from `telemetry::trace::provenance::daemon()` like every span. An outcome
//! carries both the estimating build's provenance (`estimate.loom`) and its
//! own (`loom`), so a daemon roll between the two is visible. A record whose
//! provenance does not validate is never emitted (`observability::eta`). A
//! build with an `unknown` revision or tree state is emitted with
//! `complete: false`, and accuracy queries exclude it.

use crate::eta::emit::Trigger;
use crate::eta::score::{EstimateSummary, Score};
use crate::eta::{Explanation, Provenance};
use serde::{Deserialize, Serialize};

/// Every log attribute key the ETA kinds export besides the generic
/// `loom.repo` / `loom.issue` / `loom.pr_number`. The collector's
/// `transform/privacy` log `keep_keys` must list each one
/// (`defaults/observability/collector/config.yaml`, contract-tested).
pub const ETA_LOG_ATTRIBUTE_KEYS: &[&str] = &[
    "loom.story",
    "loom.eta.estimate_id",
    "loom.eta.kind",
    "loom.eta.heuristic",
    "loom.eta.primary",
    "loom.eta.trigger",
    "loom.eta.version",
    "loom.eta.revision",
    "loom.eta.tree_state",
    "loom.eta.stage",
    "loom.eta.age_sec",
    "loom.eta.p25_sec",
    "loom.eta.p50_sec",
    "loom.eta.p75_sec",
    "loom.eta.samples_min",
    "loom.eta.horizon_bucket",
    "loom.eta.no_estimate_reason",
    "loom.eta.outcome",
    "loom.eta.outcome_source",
    "loom.eta.lead_sec",
    "loom.eta.rework_rounds_actual",
    "loom.eta.outcome_version",
    "loom.eta.outcome_revision",
    "loom.eta.outcome_tree_state",
    "loom.eta.error_sec",
    "loom.eta.abs_error_sec",
    "loom.eta.outcome_resolution_sec",
    "loom.eta.covered",
    "loom.eta.pinball_loss_sec",
    "loom.eta.age_bucket",
    "loom.eta.result",
    "loom.eta.provenance_complete",
    "loom.eta.outcome_provenance_complete",
];

/// One estimate, with its full `eta-explanation/v1` record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EtaEstimateRecord {
    /// Why it was emitted now.
    pub trigger: Trigger,
    /// Whether this is the `current` heuristic's estimate for its kind
    /// (#9328). A `false` is a **shadow** estimate: a registered candidate
    /// computed beside `current` so it accumulates a live record, never the
    /// subject's answer.
    ///
    /// Queries that want "the ETA" must filter `loom.eta.primary = true`;
    /// accuracy and promotion queries deliberately want both sides.
    /// `#[serde(default)]` to `true`: every record written before shadow mode
    /// existed was, by definition, the only and therefore primary one.
    #[serde(default = "primary_default")]
    pub primary: bool,
    /// The estimate. Its `loom` field is the computing build (required),
    /// and its `result` is absent on a refusal. Boxed: it is by far the
    /// largest payload of any record kind.
    pub explanation: Box<Explanation>,
}

fn primary_default() -> bool {
    true
}

/// One estimate's outcome, scored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EtaOutcomeRecord {
    /// The estimate as emitted, including the estimating build's
    /// provenance (`estimate.loom`, required).
    pub estimate: EstimateSummary,
    /// The build that observed the outcome (required).
    pub loom: Provenance,
    /// The score. Error fields are absent for `abandoned` and for refusals.
    pub score: Score,
    /// What resolved it: `bus`, `pulls_read`, `sweep_terminal`.
    pub outcome_source: String,
    /// How late the resolution may be, in seconds.
    pub outcome_resolution_sec: Option<i64>,
    /// `finish`: the sweep's terminal class (`exited`, `crashed`).
    pub result: Option<String>,
}

impl EtaEstimateRecord {
    /// Whether the record carries valid provenance.
    #[must_use]
    pub fn has_provenance(&self) -> bool {
        self.explanation.loom.is_valid() && !self.explanation.heuristic.is_empty()
    }
}

impl EtaOutcomeRecord {
    /// Whether both provenances are valid.
    #[must_use]
    pub fn has_provenance(&self) -> bool {
        self.loom.is_valid() && self.estimate.loom.is_valid() && !self.estimate.heuristic.is_empty()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "eta_tests.rs"]
mod tests;
