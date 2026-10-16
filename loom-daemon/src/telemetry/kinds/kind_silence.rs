//! Per-kind silence thresholds for `otlp_export` health (Issue #12120).
//!
//! One static table: how long an exported record kind may go without a
//! successful export before the host is flagged. No per-kind config keys.
//! `Some(hours)` is the threshold; `None` is exempt, for kinds that only fire
//! on activity or failure (an idle host legitimately never emits them). A
//! kind with no row defaults to [`DEFAULT_SILENCE_HOURS`], so a newly
//! registered kind is watched until someone decides it is rare.

use chrono::Duration;

/// Threshold for a kind with no row in [`KIND_SILENCE`].
pub const DEFAULT_SILENCE_HOURS: i64 = 24;

/// `(wire kind, threshold hours)`; `None` = never flagged.
pub const KIND_SILENCE: &[(&str, Option<i64>)] = &[
    // Emitted on every collector tick, whatever the host is doing.
    ("host.health", Some(24)),
    ("host.export", Some(24)),
    ("host.status", Some(24)),
    ("tokens.snapshot", Some(24)),
    // Activity-driven or failure-only: an idle host emits none of these.
    ("sweep.started", None),
    ("sweep.identity", None),
    ("sweep.phase", None),
    ("sweep.completed", None),
    ("sweep.outcome", None),
    ("role_tick.outcome", None),
    ("session.summary", None),
    ("session.analysis", None),
    ("session.output", None),
    ("daemon.event", None),
    ("trace.span", None),
    ("ci.run", None),
    ("ci.job", None),
    ("ci.duration", None),
    ("ci.job.log", None),
    ("metric.points", None),
    ("queue.snapshot", None),
    ("auto_update.tick", None),
    ("fleet.state", None),
    ("pick.decision", None),
    ("pr.resolved", None),
    ("eta.stage_outcome", None),
    ("pass.summary", None),
    ("pass.verdict", None),
    ("token_ranking.refresh", None),
    ("operator.presence", None),
    ("checkout.external_change", None),
    ("issue.estimate", None),
    ("stage.work_started", None),
    ("write_scope.refused", None),
    ("daemon.exit", None),
    ("scope.memory", None),
];

/// How long `kind` may stay silent; `None` when it is exempt.
#[must_use]
pub fn silence_threshold(kind: &str) -> Option<Duration> {
    let hours = KIND_SILENCE
        .iter()
        .find(|(k, _)| *k == kind)
        .map_or(Some(DEFAULT_SILENCE_HOURS), |(_, h)| *h);
    hours.map(Duration::hours)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::kinds::{exported_kinds_for, TELEMETRY_KINDS};

    #[test]
    fn every_row_names_a_registered_kind() {
        for (kind, _) in KIND_SILENCE {
            assert!(TELEMETRY_KINDS.iter().any(|k| k.kind == *kind), "unregistered: {kind}");
        }
    }

    #[test]
    fn every_otlp_exported_kind_resolves_to_a_threshold_or_exemption() {
        for kind in exported_kinds_for(&["otlp".to_string()]) {
            let has_row = KIND_SILENCE.iter().any(|(k, _)| *k == kind);
            let resolved = silence_threshold(&kind);
            // No row => the 24h default; a row => its own value.
            assert!(has_row || resolved == Some(Duration::hours(DEFAULT_SILENCE_HOURS)), "{kind}");
        }
    }

    #[test]
    fn unclassified_defaults_and_exempt_is_none() {
        assert_eq!(silence_threshold("brand.new"), Some(Duration::hours(24)));
        assert_eq!(silence_threshold("host.health"), Some(Duration::hours(24)));
        assert_eq!(silence_threshold("sweep.phase"), None);
    }
}
