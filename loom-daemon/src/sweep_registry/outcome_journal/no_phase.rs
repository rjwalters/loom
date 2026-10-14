//! The cause on an `unclassified:no-phase-signal` record (Issue #10642).
//!
//! The pure derivation and the field vocabulary live in
//! [`crate::telemetry::no_phase_cause`]; this module supplies the registry
//! side: the per-sweep log read that says whether the harness started, and
//! the span attribute names.

use super::*;
use crate::telemetry::NoPhaseCause;

/// Span / OTLP attribute: the exit code, or `none_observed`.
pub(crate) const ATTR_EXIT: &str = "loom.no_phase.exit";
/// Span / OTLP attribute: the last step reached.
pub(crate) const ATTR_LAST_STEP: &str = "loom.no_phase.last_step";
/// Span / OTLP attribute: the bounded reason.
pub(crate) const ATTR_REASON: &str = "loom.no_phase.reason";

/// Stamp `cause` onto the terminal `loom.sweep` span's metadata.
pub(crate) fn insert_span_attributes(
    metadata: &mut crate::telemetry::trace::TraceAttributes,
    cause: &NoPhaseCause,
) {
    metadata.insert(ATTR_EXIT.into(), cause.exit.clone());
    metadata.insert(ATTR_LAST_STEP.into(), cause.last_step.clone());
    metadata.insert(ATTR_REASON.into(), cause.reason.clone());
}

impl SweepRegistry {
    /// Derive the [`NoPhaseCause`] for `sweep_id`'s terminal transition.
    ///
    /// Reads this dispatch's region of the per-sweep log once (the same region
    /// the #4386 pre-flight classifier reads) to say whether the harness
    /// started. A missing or unreadable log yields `last_step = none`,
    /// `reason = log_unreadable` rather than a guess.
    pub(crate) fn no_phase_cause_for(
        &self,
        sweep_id: &str,
        issue: u32,
        exit_code: Option<i32>,
        last_phase: Option<&str>,
    ) -> NoPhaseCause {
        let log_path = self
            .entries
            .get(sweep_id)
            .map_or_else(|| self.compute_log_path(issue), |i| i.log_path.clone());
        let cli_started = dispatch_scoped_tail(&log_path, usize::MAX)
            .ok()
            .map(|region| classify_preflight_death(&region).is_none());
        NoPhaseCause::derive(exit_code, cli_started, last_phase)
    }
}
