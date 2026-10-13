//! Per-phase tap usage on `sweep.outcome` (Issue #11488).
//!
//! The host-local outcome journal carries one [`PhaseTapAccounting`] row per
//! `(role, tap, model)` for out-of-process phases (`worker run`, Issue
//! #11360). The
//! paired telemetry record used to carry only a `tap_phase_keys` flag, which
//! reaches SigNoz but cannot be counted or priced. [`PhaseTapUsageRow`] is the
//! same row, flattened and with the launch record's model, built *from* the
//! journal's rows so the two cannot disagree.
//!
//! A counter the harness did not report is absent, never `0`; an unmeasured
//! row (`usage_events == 0`) keeps `role` and `tap` and carries no counters.

use serde::{Deserialize, Serialize};

use crate::tap_usage::PhaseTapAccounting;

/// One phase role's usage on one tap, as carried by `sweep.outcome`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PhaseTapUsageRow {
    /// The phase role (`builder`, `doctor`).
    pub role: String,
    /// The tap accounting key (`<runtime>[:<profile>]@<credential wire>`).
    pub tap: String,
    pub runtime: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_profile: Option<String>,
    /// The model the `# LOOM_LAUNCH` record named, when it recorded one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<u64>,
    /// The harness's own estimate, not a measured charge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_estimate: Option<f64>,
    /// Absent (not `0`) for an unmeasured row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_events: Option<u64>,
}

impl From<&PhaseTapAccounting> for PhaseTapUsageRow {
    fn from(phase: &PhaseTapAccounting) -> Self {
        let tap = &phase.accounting.tap;
        let usage = &phase.accounting.usage;
        // An unmeasured row keeps its identity and drops every counter, so a
        // stray counter on a zero-event usage can never read as a measurement.
        let counters = usage.is_measured();
        let keep = |value: Option<u64>| value.filter(|_| counters);
        Self {
            role: phase.role.clone(),
            tap: phase.accounting.key(),
            runtime: tap.runtime.clone(),
            model_profile: tap.model_profile.clone(),
            model: phase.model.clone(),
            input: keep(usage.input),
            output: keep(usage.output),
            reasoning: keep(usage.reasoning),
            cache_read: keep(usage.cache_read),
            cache_write: keep(usage.cache_write),
            cost_estimate: usage.cost_estimate.filter(|_| counters),
            usage_events: counters.then_some(usage.usage_events as u64),
        }
    }
}

/// The journal's phase rows as telemetry rows, in the same order.
#[must_use]
pub fn rows_from(phases: &[PhaseTapAccounting]) -> Vec<PhaseTapUsageRow> {
    phases.iter().map(PhaseTapUsageRow::from).collect()
}
