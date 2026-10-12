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

use crate::telemetry::kinds::stage_outcome::EnteredAtSource;
use crate::telemetry::trace::{derived_hex, instant};
use crate::telemetry::TelemetryRecord;

/// The id for `kind` and its natural-key parts.
pub(super) fn derive(kind: &str, key: &[&str]) -> String {
    let mut parts = vec!["loom.fact", kind];
    parts.extend_from_slice(key);
    derived_hex(&parts, 16)
}

/// The fact id of an outcome record, or `None` for a kind that is not an
/// outcome fact or a record with nothing stable to key on. No key part is a
/// host or an emission time.
///
/// - `pr.resolved`: `(repo, pr_number, state, closed_at)`
/// - `eta.stage_outcome`, first match (#11401):
///   1. `(repo, issue, stage, next_stage, forge_transition_at)`: the forge's
///      instant for the move, identical on every host. Unchanged since
///      #11125, so no existing fact is re-keyed.
///   2. `(repo, issue, stage, next_stage, "sweep", sweep_id, entered_at)`:
///      a stage this host's sweep held. Only that host emits it; `entered_at`
///      (when the sweep's checkpoint dates it) tells a re-entry within one
///      sweep (`doctor` → `review_wait` → `doctor`) apart.
///   3. `(repo, issue, stage, next_stage, "entry", entered_at)`: a
///      forge-dated entry, when the exit's own forge read failed or no forge
///      event dates it.
///
///   `next_stage` is empty when the item left the listings. With none of
///   these the record carries `loom.fact_id_missing_reason` instead
///   ([`StageOutcomeRecord::missing_fact_id_reason`]).
///
/// [`StageOutcomeRecord::missing_fact_id_reason`]: crate::telemetry::kinds::stage_outcome::StageOutcomeRecord::missing_fact_id_reason
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
        TelemetryRecord::StageOutcome(r) => {
            let issue = r.issue.to_string();
            let next = r.next_stage.map_or("", |s| s.as_str());
            let head = [r.repo.as_str(), issue.as_str(), r.stage.as_str(), next];
            let with = |tail: &[&str]| {
                let mut key = head.to_vec();
                key.extend_from_slice(tail);
                derive("eta.stage_outcome", &key)
            };
            if let Some(at) = r.forge_transition_at {
                return Some(with(&[&instant(at)]));
            }
            if let Some(sweep) = r.sweep_id.as_deref().filter(|id| !id.is_empty()) {
                let checkpoint = r
                    .entered_at
                    .filter(|_| r.entered_at_source == Some(EnteredAtSource::Checkpoint));
                return Some(with(&["sweep", sweep, &checkpoint.map(instant).unwrap_or_default()]));
            }
            r.forge_entered_at()
                .map(|entered| with(&["entry", &instant(entered)]))
        }
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
