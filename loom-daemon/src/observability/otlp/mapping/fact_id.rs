//! `loom.fact_id` -- the cross-host identity of an outcome fact (Issue #11125,
//! `#10196` R4).
//!
//! `loom.record_id` hashes `host_id` and `emitted_at`, so it dedupes repeated
//! deliveries from ONE host only. An outcome fact (a PR merged, a stage left)
//! is the same fact whichever host observed it, so it needs an id built from
//! its natural key alone: no host, no emission time. Readers dedupe facts with
//! `LIMIT 1 BY loom.fact_id`, keeping the earliest knowable-at.
//!
//! Lives outside the ETA modules so it survives the ETA removal (#11098).

use crate::telemetry::trace::{derived_hex, instant};
use crate::telemetry::TelemetryRecord;

/// The id for `kind` and its natural-key parts.
pub(super) fn derive(kind: &str, key: &[&str]) -> String {
    let mut parts = vec!["loom.fact", kind];
    parts.extend_from_slice(key);
    derived_hex(&parts, 16)
}

/// The fact id of an outcome record, or `None` for a kind that is not an
/// outcome fact or a record with no forge instant to key on. Every key part
/// is a forge-observed fact, identical on every host that saw it.
///
/// - `pr.resolved`: `(repo, pr_number, state, closed_at)`
/// - `eta.stage_outcome`: `(repo, issue, stage, next_stage,
///   forge_transition_at)`; `next_stage` is empty when the item left the
///   listings
pub(super) fn of(record: &TelemetryRecord) -> Option<String> {
    match record {
        TelemetryRecord::PrResolved(r) => Some(derive(
            "pr.resolved",
            &[
                &r.repo,
                &r.pr_number.to_string(),
                r.state.as_str(),
                &instant(r.closed_at?),
            ],
        )),
        TelemetryRecord::StageOutcome(r) => Some(derive(
            "eta.stage_outcome",
            &[
                &r.repo,
                &r.issue.to_string(),
                r.stage.as_str(),
                r.next_stage.map_or("", |s| s.as_str()),
                &instant(r.forge_transition_at?),
            ],
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_parts_decide_the_id() {
        let a = derive("pr.resolved", &["o/r", "1", "merged"]);
        assert_eq!(a, derive("pr.resolved", &["o/r", "1", "merged"]));
        assert_eq!(a.len(), 16);
        assert_ne!(a, derive("pr.resolved", &["o/r", "1", "closed"]));
        assert_ne!(a, derive("pr.resolved", &["o/r", "2", "merged"]));
        assert_ne!(a, derive("eta.stage_outcome", &["o/r", "1", "merged"]));
    }
}
