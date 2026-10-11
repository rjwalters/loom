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

use std::sync::atomic::{AtomicU64, Ordering};

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
    "loom.fact_id_missing_reason",
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
    "loom.eta.stage_outcome.entered_at_source",
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

/// Where a record's `entered_at` came from (#11367).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnteredAtSource {
    /// The forge's own event: the label that put the item in the stage.
    Forge,
    /// This host's sweep checkpoint.
    Checkpoint,
    /// Not known: `entered_at` is absent.
    Unknown,
}

impl EnteredAtSource {
    /// The wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            EnteredAtSource::Forge => "forge",
            EnteredAtSource::Checkpoint => "checkpoint",
            EnteredAtSource::Unknown => "unknown",
        }
    }
}

/// Why an `eta.stage_outcome` carries no `loom.fact_id` (#11401).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactIdMissingReason {
    /// A forge read this pass needed (the item's label history, or the PR)
    /// failed.
    ForgeReadFailed,
    /// Every read succeeded, and no forge instant dates the exit or the entry.
    NoForgeInstant,
}

impl FactIdMissingReason {
    /// The wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            FactIdMissingReason::ForgeReadFailed => "forge_read_failed",
            FactIdMissingReason::NoForgeInstant => "no_forge_instant",
        }
    }
}

/// Exits emitted without a `loom.fact_id` this process (#11401), reported by
/// `host.export`. Cumulative per process lifetime.
static FACT_ID_MISSING_TOTAL: AtomicU64 = AtomicU64::new(0);

/// Count one emitted exit that carries no `loom.fact_id`.
pub fn note_fact_id_missing() {
    FACT_ID_MISSING_TOTAL.fetch_add(1, Ordering::Relaxed);
}

/// Exits emitted without a `loom.fact_id` since this process started.
#[must_use]
pub fn fact_id_missing_total() -> u64 {
    FACT_ID_MISSING_TOTAL.load(Ordering::Relaxed)
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
    /// When it was entered: the forge's label event, or this host's sweep
    /// checkpoint. Absent when neither dates it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entered_at: Option<DateTime<Utc>>,
    /// Where `entered_at` came from; `unknown` when it is absent. Absent only
    /// on records from before #11367.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entered_at_source: Option<EnteredAtSource>,
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
    /// host. The primary `loom.fact_id` key; absent when no forge instant is
    /// known, and then the fact id falls back to `sweep_id`, else a
    /// forge-dated `entered_at` (#11401).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forge_transition_at: Option<DateTime<Utc>>,
    /// The id of this host's sweep that held the item in the stage (#11401):
    /// the `loom.fact_id` key of an exit with no forge instant. Only the
    /// sweep's host emits a sweep row's exit, so it is stable. Absent for an
    /// item no sweep of this host held.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sweep_id: Option<String>,
    /// Why no forge instant dates the exit, as the producer saw it (#11401).
    /// Exported as `loom.fact_id_missing_reason` only when the record has no
    /// fact id ([`StageOutcomeRecord::missing_fact_id_reason`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fact_id_missing_reason: Option<FactIdMissingReason>,
    /// The observing build.
    pub loom: Provenance,
}

impl StageOutcomeRecord {
    /// Whether the record carries valid provenance.
    #[must_use]
    pub fn has_provenance(&self) -> bool {
        self.loom.is_valid()
    }

    /// The forge-dated entry, when `entered_at` came from the forge.
    #[must_use]
    pub fn forge_entered_at(&self) -> Option<DateTime<Utc>> {
        self.entered_at
            .filter(|_| self.entered_at_source == Some(EnteredAtSource::Forge))
    }

    /// Whether some `loom.fact_id` key is known: a forge transition instant,
    /// the holding sweep's id, or a forge-dated entry. Mirrors the mapper's
    /// `fact_id::of`, so a record has exactly one of a fact id and a
    /// [`missing_fact_id_reason`](Self::missing_fact_id_reason).
    #[must_use]
    pub fn has_fact_key(&self) -> bool {
        self.forge_transition_at.is_some()
            || self.sweep_id.as_deref().is_some_and(|id| !id.is_empty())
            || self.forge_entered_at().is_some()
    }

    /// Why the record carries no `loom.fact_id`; `None` when it carries one.
    /// Never `None` without a fact id: an unset reason reads as
    /// `no_forge_instant`.
    #[must_use]
    pub fn missing_fact_id_reason(&self) -> Option<FactIdMissingReason> {
        (!self.has_fact_key()).then(|| {
            self.fact_id_missing_reason
                .unwrap_or(FactIdMissingReason::NoForgeInstant)
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "stage_outcome_tests.rs"]
mod tests;
