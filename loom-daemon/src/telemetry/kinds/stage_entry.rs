//! `eta.stage_entry` (#12869): an item's arrival in a pipeline stage.
//!
//! # Why this exists
//!
//! `eta.stage_outcome` reconstructed a stay's `entered_at` from forge label
//! history (`entered_at_source='forge'`); nothing said *when the item arrived*,
//! so the arrival (inflow) process was lossy and an item's queue position at
//! arrival could not be computed. #11301 removed that producer and left dwell
//! to consumers. This fact restores the one input a consumer cannot recover
//! cheaply: a stage-entry record, emitted when the stage label is first
//! **observed** on an item.
//!
//! # How it is produced
//!
//! By diffing the step driver's per-tick discovery snapshot
//! (`observability::stage_entry_fact`): an item whose stage label is present
//! now and was not on the previous tick's read of the same repo has arrived.
//! No new forge call and no write chokepoint (label writes come from many
//! scripts and agents). Arrival is **tick-granular** (operator ruling,
//! #12869): `entered_at` is the mirror's `labels_at` time for the label when
//! the snapshot carries it, else the observing tick (an upper bound).
//!
//! A repo's first read after a daemon start only seeds the baseline: items
//! already in a stage emit nothing, because their arrival is unknown.
//!
//! **OTLP only.** The body is the record's JSON; `loom.repo`, `loom.issue` and
//! `loom.repo.visibility` ride as attributes (already on the gateway
//! `keep_keys`). The `eta.` prefix keeps it in the `loom.kind LIKE 'eta%'`
//! query family.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::telemetry::RepoVisibility;

/// Payload version carried in the record's own `schema_version` field.
pub const STAGE_ENTRY_SCHEMA_VERSION: u32 = 1;

/// The stage labels whose arrival is recorded (the issue's four).
pub const STAGE_LABELS: &[&str] = &["loom:triage", "loom:curated", "loom:issue", "loom:operator"];

/// Where `entered_at` came from (the wire value of `entered_at_source`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnteredAtSource {
    /// The state mirror's `labels_at` time for the label.
    Mirror,
    /// The tick that first saw the label; an upper bound on the arrival.
    Observed,
}

/// One item's arrival in one stage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageEntryRecord {
    /// [`STAGE_ENTRY_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Forge `owner/repo`.
    pub repo: String,
    /// The issue number.
    pub issue: u32,
    /// The stage label that was observed arriving (e.g. `loom:issue`).
    pub stage: String,
    /// When the item entered the stage.
    pub entered_at: DateTime<Utc>,
    /// Where `entered_at` came from.
    pub entered_at_source: EnteredAtSource,
    /// When this daemon observed the arrival (knowable-at).
    pub observed_at: DateTime<Utc>,
    /// The repo's visibility, for the redaction layer.
    #[serde(default)]
    pub visibility: RepoVisibility,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn record_round_trips_through_serde() {
        let at = "2026-10-20T12:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let record = StageEntryRecord {
            schema_version: STAGE_ENTRY_SCHEMA_VERSION,
            repo: "rjwalters/loom".into(),
            issue: 7,
            stage: "loom:issue".into(),
            entered_at: at,
            entered_at_source: EnteredAtSource::Mirror,
            observed_at: at,
            visibility: RepoVisibility::Public,
        };
        let value = serde_json::to_value(&record).unwrap();
        assert_eq!(value["entered_at_source"], "mirror");
        let back: StageEntryRecord = serde_json::from_value(value).unwrap();
        assert_eq!(back, record);
    }
}
