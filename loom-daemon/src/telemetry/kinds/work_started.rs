//! `stage.work_started` (Issue #11641): a role agent began working an item in
//! a stage.
//!
//! # Why this kind exists
//!
//! A stage stay (`eta.stage_outcome`, `entered_at` → `left_at`) mixes two
//! durations with different causes: the **queue wait** before an agent picks
//! the item up (fleet load, rank, concurrency) and the **work time** the agent
//! then spends (size, difficulty). Before #11641 nothing said when an agent
//! began: `sweep.started` keys on the issue and fires once per sweep, so the
//! Judge, Doctor and merge phases, and every standalone role-runner tick, had
//! no work-start fact at all. This record is that fact, one per agent start,
//! carrying the keys that join it to the stay it began.
//!
//! # Producers
//!
//! - **A sweep this host runs** (`source` [`source::SWEEP_DISPATCH`] or
//!   [`source::SWEEP_CHECKPOINT`]): its Curator at dispatch, and each later
//!   role when the sweep's checkpoint marker moves the item into that role's
//!   stage ([`sweep_role_for_marker`]). `started_at` is the marker's own
//!   timestamp, the same instant `fleet.state` uses as that stay's
//!   `entered_at`: a sweep dispatches the next phase as soon as it writes the
//!   marker, so a sweep-held stay has no queue wait.
//! - **A role-runner tick** (`source` [`source::ROLE_ACT`]): the first write a
//!   Judge, Doctor or Champion agent issued on a PR (its claim label, verdict
//!   or merge), from the tick's pick journal. `started_at` is when the agent
//!   issued that write.
//!
//! **OTLP only.** **Provenance is required.**

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::telemetry::kinds::fleet_state::FleetStage;
use crate::telemetry::provenance::Provenance;

/// `source` values: what observed the start.
pub mod source {
    /// A sweep's dispatch: its Curator phase begins.
    pub const SWEEP_DISPATCH: &str = "sweep_dispatch";
    /// A sweep's checkpoint marker moved the item into the next role's stage;
    /// `started_at` is the marker's timestamp.
    pub const SWEEP_CHECKPOINT: &str = "sweep_checkpoint";
    /// As [`SWEEP_CHECKPOINT`], but the marker carried no timestamp, so
    /// `started_at` is when the daemon observed it (an upper bound).
    pub const SWEEP_OBSERVED: &str = "sweep_observed";
    /// A role-runner agent's first forge write on the item in its tick.
    pub const ROLE_ACT: &str = "role_act";
}

/// `role` values.
pub mod role {
    pub const CURATOR: &str = "curator";
    pub const BUILDER: &str = "builder";
    pub const JUDGE: &str = "judge";
    pub const DOCTOR: &str = "doctor";
    /// A sweep's own merge phase (the role name `sweep-checkpoint begin` uses).
    pub const MERGE: &str = "merge";
    pub const CHAMPION: &str = "champion";
}

/// The `dispatch_origin` of a role-runner tick's start. A sweep's start carries
/// its `sweep.started` origin (`DispatchOrigin`) instead.
pub const ROLE_RUNNER_ORIGIN: &str = "role_runner";

/// Every log attribute key this kind exports besides `loom.kind`,
/// `loom.record_id`, `loom.fact_id`, `loom.repo`, `loom.issue`,
/// `loom.pr_number`, `loom.role`, `loom.sweep_id`, `loom.attempt_index` and
/// `loom.dispatch_origin` (already kept). The collector's `transform/privacy`
/// log `keep_keys` must list each one (contract-tested).
pub const WORK_STARTED_LOG_ATTRIBUTE_KEYS: &[&str] = &[
    "loom.work_started.stage",
    "loom.work_started.started_at",
    "loom.work_started.source",
    "loom.work_started.stage_attempt",
];

/// The `stage.work_started` record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkStartedRecord {
    /// `owner/repo`, lowercased (as `fleet.state` and `eta.stage_outcome`).
    pub repo: String,
    /// The issue, when known (always for a sweep; absent for a role tick,
    /// which works a PR).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue: Option<u32>,
    /// The PR, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_number: Option<u32>,
    /// The stage the work began in, in the `fleet.state` vocabulary.
    pub stage: FleetStage,
    /// Who began: `curator`, `builder`, `judge`, `doctor`, `merge` (a sweep's
    /// own merge phase) or `champion`.
    pub role: String,
    /// The sweep's id, or a role tick's id (`role-<role>-<instant>`).
    pub sweep_id: String,
    /// The sweep's attempt ordinal for the issue, as `sweep.started` reports
    /// it. Absent for a role tick and for a sweep this daemon did not dispatch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_index: Option<u32>,
    /// 1-based count of starts of this `stage` within this `sweep_id` (a
    /// Doctor loop re-enters `review_wait`); `1` for a role tick.
    pub stage_attempt: u32,
    /// Who asked for the work: the sweep's `dispatch_origin`, or
    /// [`ROLE_RUNNER_ORIGIN`]. Absent for a sweep this daemon did not dispatch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_origin: Option<String>,
    /// When the agent began.
    pub started_at: DateTime<Utc>,
    /// What observed the start (see [`source`]).
    pub source: String,
    /// When this daemon emitted the fact (knowable-at).
    pub observed_at: DateTime<Utc>,
    /// The emitting build.
    pub loom: Provenance,
}

/// The role a sweep checkpoint marker hands the item to, and that role's
/// stage. `None` (no marker) is the Curator in `sweep.curator`; `merge-done`
/// starts no work, and an unknown marker is never guessed at. The stage agrees
/// with `fleet_state::held_stage` for every known marker (tested).
#[must_use]
pub fn sweep_role_for_marker(marker: Option<&str>) -> Option<(FleetStage, &'static str)> {
    Some(match marker {
        None => (FleetStage::SweepCurator, role::CURATOR),
        Some("curator-done") => (FleetStage::SweepBuilder, role::BUILDER),
        Some("builder-done" | "doctor-done") => (FleetStage::ReviewWait, role::JUDGE),
        Some("judge-rejected") => (FleetStage::Doctor, role::DOCTOR),
        Some("judge-done") => (FleetStage::MergeWait, role::MERGE),
        Some(_) => return None,
    })
}

/// The stage a role-runner role works, for the roles whose start this kind
/// records (`judge`, `doctor`, `champion`).
#[must_use]
pub fn role_tick_stage(role_name: &str) -> Option<FleetStage> {
    match role_name {
        role::JUDGE => Some(FleetStage::ReviewWait),
        role::DOCTOR => Some(FleetStage::Doctor),
        role::CHAMPION => Some(FleetStage::MergeWait),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sweep_stage_agrees_with_fleet_state_for_every_marker() {
        let at = Utc::now();
        for marker in [
            None,
            Some("curator-done"),
            Some("builder-done"),
            Some("doctor-done"),
            Some("judge-rejected"),
            Some("judge-done"),
        ] {
            let (stage, _) = sweep_role_for_marker(marker).unwrap_or_else(|| panic!("{marker:?}"));
            let (held, ..) = crate::observability::fleet_state::held_stage(marker, Some(at), at);
            assert_eq!(stage, held, "{marker:?}");
        }
        assert_eq!(sweep_role_for_marker(Some("merge-done")), None);
        assert_eq!(sweep_role_for_marker(Some("future-marker")), None);
    }

    #[test]
    fn role_ticks_map_to_their_pr_stage() {
        assert_eq!(role_tick_stage("judge"), Some(FleetStage::ReviewWait));
        assert_eq!(role_tick_stage("doctor"), Some(FleetStage::Doctor));
        assert_eq!(role_tick_stage("champion"), Some(FleetStage::MergeWait));
        assert_eq!(role_tick_stage("curator"), None);
    }

    #[test]
    fn collector_keeps_every_work_started_attribute() {
        const CONFIG: &str =
            include_str!("../../../../defaults/observability/collector/config.yaml");
        let keep = CONFIG
            .lines()
            .find(|l| l.contains("keep_keys(attributes, [") && l.contains("loom.ci.chunk_index"))
            .expect("the transform/privacy log keep_keys line");
        for key in WORK_STARTED_LOG_ATTRIBUTE_KEYS.iter().chain(&[
            "loom.repo",
            "loom.issue",
            "loom.pr_number",
            "loom.role",
            "loom.sweep_id",
            "loom.attempt_index",
            "loom.dispatch_origin",
            "loom.fact_id",
        ]) {
            assert!(keep.contains(&format!("\"{key}\"")), "collector drops {key}");
        }
    }
}
