//! `eta.stage_attribution` (#10957): the nightly per-heuristic, per-stage
//! rollup of where `land` ETA error comes from.
//!
//! `eta.outcome` carries each resolved estimate's error split by stage
//! (`attribution`, #10929). This kind folds those splits over a trailing
//! window so a systematic per-stage bias shows up without scanning every
//! outcome: for each registered `land` heuristic, one record per [`Stage`]
//! plus one for the `unattributed` remainder. The row count is therefore
//! `heuristics × (7 + 1)` per day, whatever the number of outcomes; there is
//! never a per-item row.
//!
//! Folded from the authority host's local attribution log
//! (`eta::attribution_log`) at the day's cutoff, with no forge call, and
//! emitted by the same job as `eta.backtest.fold`. **OTLP only. Provenance is
//! required.**
//!
//! [`Stage`]: crate::eta::Stage

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::eta::Provenance;

/// The `stage` value of the row for the error no stage explains.
pub const UNATTRIBUTED: &str = "unattributed";

/// One (heuristic, stage) row of a day's rollup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EtaStageAttributionRecord {
    /// Derived, never random: `derived_hex(["loom.eta.stage_attribution",
    /// heuristic, stage, day])`. Stable, so a re-offer dedupes.
    pub row_id: String,
    /// The fold's UTC day, `YYYY-MM-DD`.
    pub day: String,
    /// The window length in days, ending at `cutoff`.
    pub window_days: u32,
    /// The heuristic.
    pub heuristic: String,
    /// `land` today.
    pub kind: String,
    /// A stage wire name, or [`UNATTRIBUTED`].
    pub stage: String,
    /// Outcomes in the window with this stage (all of them for
    /// `unattributed`).
    pub n: u64,
    /// Mean signed error attributed here, seconds: positive is slower than
    /// forecast.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bias_sec: Option<f64>,
    /// Mean absolute error attributed here, seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mean_abs_sec: Option<f64>,
    /// Share of those outcomes whose dominant stage this was. Absent for
    /// `unattributed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dominant_share: Option<f64>,
    /// The end of `day`: nothing observed at or after it was read, and the
    /// record's time.
    pub cutoff: DateTime<Utc>,
    /// The computing build.
    pub loom: Provenance,
}

impl EtaStageAttributionRecord {
    /// Whether the record carries valid provenance.
    #[must_use]
    pub fn has_provenance(&self) -> bool {
        self.loom.is_valid()
    }
}
