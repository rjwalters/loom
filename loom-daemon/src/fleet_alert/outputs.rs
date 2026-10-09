//! Bridge from the singleton output watchdog to fleet alerts (#10916, slice 2a).
//!
//! [`crate::fleet_outputs::evaluate`] is pure and judges output freshness; this
//! maps its conditions onto [`Condition`]s so they ride the same debounce /
//! reminder / clear state and the same sinks (event bus -> Matrix, loom-ui
//! inbox) as the other fleet alerts. Output is judged, never job ownership.

use chrono::{DateTime, Utc};

use super::Condition;
use crate::fleet_outputs::{self, OutputSource, Severity, SINGLETON_OUTPUTS};

/// Key prefix shared by every output-watchdog condition.
pub const KEY_PREFIX: &str = "output-missing:";

/// The data an output-watchdog pass reads.
pub struct OutputWatch<'a> {
    pub source: &'a dyn OutputSource,
    /// Fleet roster (repos), for per-repo coverage.
    pub roster: &'a [String],
}

/// Whether an alert key belongs to the output watchdog.
#[must_use]
pub fn is_output_key(key: &str) -> bool {
    key.starts_with(KEY_PREFIX)
}

/// Judge the registry and return alert conditions.
#[must_use]
pub fn conditions(watch: &OutputWatch<'_>, now: DateTime<Utc>) -> Vec<Condition> {
    fleet_outputs::evaluate(SINGLETON_OUTPUTS, watch.source, watch.roster, now)
        .into_iter()
        .map(|c| {
            let level = match c.severity {
                Severity::Critical => "CRITICAL",
                Severity::Warning => "WARNING",
            };
            Condition {
                key: c.key,
                headline: format!(
                    "{level}: fleet singleton '{}' output missing: {}.",
                    c.job, c.headline
                ),
                fix: format!(
                    "Check which host owns '{}' (the output is judged regardless of owner): \
                     it may have moved to a host that does not produce {} or have stopped \
                     emitting. See `fleet_outputs::SINGLETON_OUTPUTS`.",
                    c.job, c.record_kind
                ),
                critical: c.severity == Severity::Critical,
            }
        })
        .collect()
}
