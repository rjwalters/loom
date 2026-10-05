//! `eta.fit` (#10391): one record per daily-fit check, whether it fitted or
//! not. **OTLP only**, like the other `eta.*` log kinds.
//!
//! The daily refit used to leave no trace of its own: whether a host fitted,
//! why it did not, and which coefficient file it served from were inferred
//! from estimates appearing. Each caller of the fit check (the fleet refresh
//! tick's end-of-cycle check, and the standalone daily task) now emits exactly
//! one record per check — skips and stand-down hosts included — so the records
//! double as the fit loop's heartbeat (about 24 per host per day).
//!
//! The scalar fields ride as `loom.eta.fit.*` attributes
//! ([`super::eta::ETA_LOG_ATTRIBUTE_KEYS`]); the body is the record's JSON,
//! which also carries the per-stage and per-repo detail that would not fit
//! an attribute. **Absent is never zero**: every optional field is omitted when
//! it does not apply.
//!
//! **Provenance is required**: a record whose build provenance does not
//! validate is never emitted.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::eta::Provenance;

/// The closed `skip_reason` vocabulary.
pub const SKIP_REASONS: [&str; 5] = [
    "disabled",
    "held",
    "today_exists",
    "no_snapshots",
    "stale_before_grace",
];

/// One fit stage's share of a fit (the body's `stages` object).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EtaFitStage {
    /// Training rows.
    pub rows: u64,
    /// Rows whose exit label is `true`.
    pub exits: u64,
    /// Rows whose exit label is censored (`exit == None`).
    pub exit_censored: u64,
    /// Rows whose merge label is an observed merge.
    pub merge_events: u64,
    /// `rows - merge_events`.
    pub merge_censored: u64,
    /// The stage has a fitted exit hazard.
    pub hazard: bool,
    /// The stage is in the direct model.
    pub aft: bool,
}

/// One fit check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EtaFitRecord {
    /// Derived, never random: `derived_hex(["loom.eta.fit_check", host_id,
    /// started_at])` (a different domain from the coefficient file's own id).
    pub check_id: String,
    /// `fleet_refresh` or `daily_task`.
    pub trigger: String,
    /// When the check started — the record's time.
    pub started_at: DateTime<Utc>,
    /// `written`, `skipped`, `error` or `panic`.
    pub outcome: String,
    /// Present exactly when `outcome` is `skipped`; one of [`SKIP_REASONS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip_reason: Option<String>,
    /// `{e:#}`, at most 512 bytes; `error` only. Daemon-authored text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The coefficient file's content id: on `written`, and on `today_exists`
    /// (the existing file's id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fit_id: Option<String>,
    /// The cutoff `T`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cutoff: Option<DateTime<Utc>>,
    /// The training window's start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_start: Option<DateTime<Utc>>,
    /// The training window's length in days.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_days: Option<i64>,
    /// The data horizon `H`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_through: Option<DateTime<Utc>>,
    /// Snapshots read; `0` on `no_snapshots`.
    pub snapshots: u64,
    /// The oldest snapshot's `as_of`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_oldest_as_of: Option<DateTime<Utc>>,
    /// The newest snapshot's `as_of`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_newest_as_of: Option<DateTime<Utc>>,
    /// Body only: each snapshot's repo and `as_of`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub snapshot_as_of: BTreeMap<String, DateTime<Utc>>,
    /// Body only: per fit stage.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub stages: BTreeMap<String, EtaFitStage>,
    /// Training rows over every stage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows_total: Option<u64>,
    /// Rows with a censored exit label, over every stage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows_censored: Option<u64>,
    /// Rows dropped for a missing queue feature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows_dropped_missing: Option<u64>,
    /// Rows dropped for want of a flag timeline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows_dropped_no_flags: Option<u64>,
    /// Rows whose PR-or-issue star is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows_star_unknown: Option<u64>,
    /// Dwells (path-statistics episodes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dwells: Option<u64>,
    /// Old coefficient files removed by retention.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pruned: Option<u64>,
    /// The coefficient file's name, never a path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coeff_file: Option<String>,
    /// Its size as written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coeff_bytes: Option<u64>,
    /// Its sha256 (lowercase hex).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coeff_sha256: Option<String>,
    /// Wall time of the check, milliseconds.
    pub duration_ms: u64,
    /// The computing build.
    pub loom: Provenance,
}

impl EtaFitRecord {
    /// Whether the record carries valid provenance.
    #[must_use]
    pub fn has_provenance(&self) -> bool {
        self.loom.is_valid()
    }
}
