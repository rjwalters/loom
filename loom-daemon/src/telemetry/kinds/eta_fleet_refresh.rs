//! `eta.fleet_refresh` (#10263): one repo's outcome in one cycle of the
//! daemon's fleet snapshot refresh task (`observability::eta_fleet_refresh`).
//!
//! **OTLP only**, like the other `eta.*` log kinds: one record per repo per
//! cycle, skipped repos included, so a repo that never gets refreshed (no
//! reader App, a coverage gap, a budget that never reaches it) is visible as
//! such rather than as silence. The scalar fields ride as
//! `loom.eta.fleet.*` attributes ([`super::eta::ETA_LOG_ATTRIBUTE_KEYS`]); the
//! body is the record's JSON.
//!
//! **Provenance is required**, the same rule as `eta.estimate` /
//! `eta.outcome`: a record whose build provenance does not validate is never
//! emitted.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::eta::Provenance;

/// One repo, one cycle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EtaFleetRefreshRecord {
    /// `owner/repo`.
    pub repo: String,
    /// Derived, never random: `derived_hex(["loom.eta.fleet_refresh", host,
    /// cycle start])`. Every repo's record in one cycle shares it.
    pub cycle_id: String,
    /// When the cycle started — the record's time.
    pub started_at: DateTime<Utc>,
    /// `backfill`, `refresh`, or `none` (skipped before a pass was chosen).
    pub pass: String,
    /// Why the repo's cycle ended (`complete`, `not_modified`, `budget`, …).
    pub stop_reason: String,
    /// The pass completed and its staging snapshot was published.
    pub promoted: bool,
    /// PRs whose timeline was read this cycle.
    pub prs_read: u64,
    /// PRs the pass has read in total (`done`).
    pub pass_done: u64,
    /// Timelines that did not parse: read, counted, contributing nothing.
    pub timelines_incomplete: u64,
    /// Published samples after minus before — the issue's "events added".
    pub samples_added: i64,
    /// Raw event rows appended to the #10250 cache, when that sync ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_events_added: Option<u64>,
    /// Forge requests, `304`s and failures included.
    pub forge_calls: u64,
    /// Of which `304`s.
    pub not_modified_calls: u64,
    /// The lowest `x-ratelimit-remaining` any response reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ratelimit_remaining_min: Option<u64>,
    /// The reader App's id (not a secret).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reader_app: Option<String>,
    /// The published snapshot after the cycle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_id: Option<String>,
    /// Its `as_of`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub as_of: Option<DateTime<Utc>>,
    /// Wall time spent on this repo, milliseconds.
    pub duration_ms: u64,
    /// Forge reads made while SigNoz is the history source (#10520): `0`
    /// when SigNoz covered the pass; absent when SigNoz history is off.
    /// Body-only, like `history_source`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gap_fill_calls: Option<u64>,
    /// `signoz`, `signoz_gap_fill`, `forge_uncovered` or `forge_unavailable`
    /// (#10520); absent when SigNoz history is off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_source: Option<String>,
    /// The computing build.
    pub loom: Provenance,
}

impl EtaFleetRefreshRecord {
    /// Whether the record carries valid provenance.
    #[must_use]
    pub fn has_provenance(&self) -> bool {
        self.loom.is_valid()
    }
}
