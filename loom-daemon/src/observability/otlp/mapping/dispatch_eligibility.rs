//! OTLP mapping for `eta.dispatch_eligibility` (#12790).
//!
//! One log record per ready item per emission. The **body** is the record's
//! JSON, so ClickHouse can `JSONExtract` the eligibility fields (eligibility,
//! reason, `block_start`, `eligible_work_ahead`, `resource_pool`, …). Three
//! attributes ride alongside — `loom.repo`, `loom.issue` and
//! `loom.repo.visibility` — so a reader can filter the stream and join it with
//! the dwell facts (`eta.stage_outcome`) on `(repo, issue)` without parsing
//! bodies. All three are already on the gateway's log `keep_keys` (both the
//! main allowlist and the independent `transform/privacy` `ln`), so no
//! collector change is needed. `log_record_for` adds `loom.kind`
//! (`eta.dispatch_eligibility`) centrally (#10899). The record time is the
//! collector sample instant (`sampled_at`).

use opentelemetry_proto::tonic::common::v1::KeyValue;
use opentelemetry_proto::tonic::logs::v1::SeverityNumber;

use super::{kv_int, kv_string, nanos};
use crate::telemetry::{RepoVisibility, TelemetryRecord};

fn visibility_str(visibility: RepoVisibility) -> &'static str {
    match visibility {
        RepoVisibility::Public => "public",
        RepoVisibility::Private => "private",
    }
}

/// `(event_name, severity, record time, attributes, body)` for an
/// `eta.dispatch_eligibility` record; `None` for every other kind.
pub(super) fn log_parts(
    record: &TelemetryRecord,
) -> Option<(&'static str, SeverityNumber, u64, Vec<KeyValue>, String)> {
    let TelemetryRecord::DispatchEligibility(r) = record else {
        return None;
    };
    let attributes = vec![
        kv_string("loom.repo", r.repo.clone()),
        kv_string("loom.repo.visibility", visibility_str(r.visibility)),
        kv_int("loom.issue", i64::from(r.issue)),
    ];
    let body = serde_json::to_string(r).unwrap_or_default();
    Some((
        "eta.dispatch_eligibility",
        SeverityNumber::Info,
        nanos(r.sampled_at),
        attributes,
        body,
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::log_record_for;
    use crate::telemetry::kinds::dispatch_eligibility::{
        DispatchEligibilityRecord, Eligibility, EligibilityTransition,
        DISPATCH_ELIGIBILITY_SCHEMA_VERSION,
    };
    use crate::telemetry::{RepoVisibility, TelemetryEnvelope, TelemetryRecord};
    use chrono::Utc;

    fn record() -> DispatchEligibilityRecord {
        let now = Utc::now();
        DispatchEligibilityRecord {
            schema_version: DISPATCH_ELIGIBILITY_SCHEMA_VERSION,
            repo: "rjwalters/loom".into(),
            issue: 42,
            host: "host-a".into(),
            eligibility: Eligibility::Blocked,
            reason: "open_pr".into(),
            state: "blocked".into(),
            block_start: now,
            transition: EligibilityTransition::Changed,
            rank: 3,
            eligible_work_ahead: 2,
            eligible_work_ahead_repo: 1,
            resource_pool: "rjwalters/loom".into(),
            max_concurrent: 2,
            level: "default".into(),
            sampled_at: now,
            tick_at: now,
            visibility: RepoVisibility::Private,
        }
    }

    #[test]
    fn maps_to_a_log_whose_body_is_the_record_json_with_repo_issue_attributes() {
        let envelope =
            TelemetryEnvelope::new("host-a", TelemetryRecord::DispatchEligibility(record()));
        let log = log_record_for(&envelope).expect("eta.dispatch_eligibility exports as a log");
        assert_eq!(log.event_name, "eta.dispatch_eligibility");
        let body = format!("{:?}", log.body);
        assert!(body.contains("eligible_work_ahead"), "{body}");
        assert!(body.contains("open_pr"), "{body}");
        let attr = |key: &str| {
            log.attributes
                .iter()
                .find(|a| a.key == key)
                .map(|a| format!("{:?}", a.value))
        };
        assert!(attr("loom.repo").unwrap().contains("rjwalters/loom"));
        assert!(attr("loom.issue").unwrap().contains("42"));
        assert!(attr("loom.repo.visibility").unwrap().contains("private"));
        assert!(attr("loom.kind")
            .unwrap()
            .contains("eta.dispatch_eligibility"));
    }
}
