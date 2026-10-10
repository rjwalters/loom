//! `eta.stage_outcome` (#10929, re-homed by #11126): one stage an item left,
//! with its entry and exit instants and how it left.
//!
//! The wire tag keeps its `eta.` prefix and every `loom.eta.stage_outcome.*`
//! key is kept (the #11098 Stage 1 promise), but nothing here depends on the
//! ETA subsystem. The producer is `observability::fleet_state::outcomes`, a
//! diff of two consecutive `fleet.state` views, so every host emits the
//! transitions it observes and `loom.fact_id` collapses the duplicates.
//!
//! `open_estimates`, `estimate_ids[]` and `repo_id` were dropped with the
//! move (loom-ui sign-off on #11098): they referred to Loom's own estimates.
//!
//! **OTLP only.** **Provenance is required.**

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::telemetry::kinds::fleet_state::FleetStage;
use crate::telemetry::provenance::Provenance;

/// The `event` of every record the `fleet.state` producer builds.
pub const FLEET_STATE_EVENT: &str = "fleet.state";

/// Every log attribute key `pr.resolved` and `eta.stage_outcome` export
/// besides `loom.kind`, `loom.record_id`, `loom.repo`, `loom.issue` and
/// `loom.pr_number`. The collector's `transform/privacy` log `keep_keys` must
/// list each one (contract-tested).
pub const OUTCOME_FACT_LOG_ATTRIBUTE_KEYS: &[&str] = &[
    "loom.fact_id",
    "loom.eta.authority",
    "loom.eta.version",
    "loom.eta.revision",
    "loom.eta.tree_state",
    "loom.eta.provenance_complete",
    "loom.eta.pr.state",
    "loom.eta.pr.resolved_at",
    "loom.eta.pr.observed_at",
    "loom.eta.pr.resolution_sec",
    "loom.eta.stage_outcome.stage",
    "loom.eta.stage_outcome.exit",
    "loom.eta.stage_outcome.next_stage",
    "loom.eta.stage_outcome.entered_at",
    "loom.eta.stage_outcome.left_at",
    "loom.eta.stage_outcome.dwell_sec",
];

/// How the item left the stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageExit {
    /// On to the next stage of the path.
    Advance,
    /// A Judge approval.
    Pass,
    /// A Judge rejection: on to `doctor`.
    Rework,
    /// Approved, and an operator hold landed (`merge_wait` → `merge_hold`).
    Hold,
    /// The hold was released (`merge_hold` → `merge_wait`).
    Released,
    /// The sweep's Judge phase completed, and the verdict was not yet known.
    Judged,
    /// The work landed (a merge).
    Landed,
    /// Cut short: the PR closed unmerged, or the stage ended without
    /// completing. There is no dwell.
    CutShort,
    /// Nothing observed says how.
    Unknown,
}

impl StageExit {
    /// The wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            StageExit::Advance => "advance",
            StageExit::Pass => "pass",
            StageExit::Rework => "rework",
            StageExit::Hold => "hold",
            StageExit::Released => "released",
            StageExit::Judged => "judged",
            StageExit::Landed => "landed",
            StageExit::CutShort => "cut_short",
            StageExit::Unknown => "unknown",
        }
    }

    /// The exit of a move from `from` straight into `to`.
    #[must_use]
    pub fn between(from: FleetStage, to: FleetStage) -> Self {
        match (from, to) {
            (_, FleetStage::Doctor) => StageExit::Rework,
            (_, FleetStage::MergeHold) => StageExit::Hold,
            (FleetStage::MergeHold, FleetStage::MergeWait) => StageExit::Released,
            (FleetStage::ReviewWait, FleetStage::MergeWait) => StageExit::Pass,
            _ => StageExit::Advance,
        }
    }
}

/// One stage an item left.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageOutcomeRecord {
    /// `owner/repo`.
    pub repo: String,
    /// The issue.
    pub issue: u32,
    /// The PR, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_number: Option<u32>,
    /// The stage left.
    pub stage: FleetStage,
    /// When it was entered, when that was observed exactly. Absent for a
    /// stage first seen mid-way (a restart, a first listing).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entered_at: Option<DateTime<Utc>>,
    /// When it was left: the event time.
    pub left_at: DateTime<Utc>,
    /// `left_at − entered_at`, when the stage completed and its entry was
    /// exact. Never a lower bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dwell_sec: Option<i64>,
    /// How it was left.
    pub exit: StageExit,
    /// The stage entered next, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_stage: Option<FleetStage>,
    /// The source that observed the transition (`fleet.state`).
    pub event: String,
    /// When this daemon observed it: the knowable-at time.
    pub observed_at: DateTime<Utc>,
    /// How late `left_at` can be, in seconds: `0` for a forge instant, else
    /// the observing host's listing interval.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution_sec: Option<i64>,
    /// The forge's own instant for the transition (a label event's
    /// `created_at`, a PR's `merged_at` / `closed_at`), identical on every
    /// host. The `loom.fact_id` key; absent when no forge instant is known,
    /// and then the record carries no `loom.fact_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forge_transition_at: Option<DateTime<Utc>>,
    /// The observing build.
    pub loom: Provenance,
}

impl StageOutcomeRecord {
    /// Whether the record carries valid provenance.
    #[must_use]
    pub fn has_provenance(&self) -> bool {
        self.loom.is_valid()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "stage_outcome_tests.rs"]
mod tests;
