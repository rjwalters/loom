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

/// The fact id of a record, or `None` for a kind with no natural key. No key
/// part is a host or an emission time. Only `stage.work_started` carries one now
/// (#12094 retired `pr.resolved` and `eta.stage_outcome`).
pub(super) fn of(record: &TelemetryRecord) -> Option<String> {
    match record {
        // #11641: one agent start. Only the emitting host emits it, so the id
        // dedupes replays: `(repo, issue, pr_number, stage, role, sweep_id,
        // stage_attempt, started_at)`.
        TelemetryRecord::WorkStarted(r) => Some(derive(
            "stage.work_started",
            &[
                &r.repo,
                &r.issue.map(|n| n.to_string()).unwrap_or_default(),
                &r.pr_number.map(|n| n.to_string()).unwrap_or_default(),
                r.stage.as_str(),
                &r.role,
                &r.sweep_id,
                &r.stage_attempt.to_string(),
                &instant(r.started_at),
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
