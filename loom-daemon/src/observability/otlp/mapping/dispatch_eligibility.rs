//! OTLP mapping for `eta.dispatch_eligibility` (#12790, #12867).
//!
//! One log record per ready item per emission. The **body** is the record's
//! JSON, so ClickHouse can `JSONExtract` the richer fields (`block_start`,
//! `eligible_work_ahead`, `resource_pool`, …). Alongside it ride:
//! - `loom.repo`, `loom.issue` and `loom.repo.visibility` — the join/filter
//!   keys, so a reader can join the stream with the dwell facts
//!   (`eta.stage_outcome`) on `(repo, issue)` without parsing bodies; and
//! - `loom.eta.dispatch_eligibility.{eligibility,reason,rank}`
//!   ([`DISPATCH_ELIGIBILITY_LOG_ATTRIBUTE_KEYS`]) — the eligibility
//!   **verdict**, the dispatcher's **reason** token (the blocking reason) and
//!   the item's **rank**. #12790 left these in the body only, so SigNoz's
//!   `attributes_string` carried nothing but repo/issue/visibility and the fact
//!   read as a hollow shell (#12867); promoting them lets a reader
//!   `GROUP BY`/filter the schedulability signal directly.
//!
//! The repo/issue/visibility trio was already on the gateway's log `keep_keys`;
//! #12867 adds the three `loom.eta.dispatch_eligibility.*` keys to the
//! `transform/privacy` log allowlist (the
//! `attributes_are_declared_and_the_collector_keeps_them` test enforces it).
//! `log_record_for` adds `loom.kind` (`eta.dispatch_eligibility`) centrally
//! (#10899). The record time is the collector sample instant (`sampled_at`).

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
        // The schedulability signal, promoted out of the body so SigNoz can
        // query it (#12867). These three keys are
        // DISPATCH_ELIGIBILITY_LOG_ATTRIBUTE_KEYS, in that order.
        kv_string("loom.eta.dispatch_eligibility.eligibility", r.eligibility.as_str()),
        kv_string("loom.eta.dispatch_eligibility.reason", r.reason.clone()),
        kv_int("loom.eta.dispatch_eligibility.rank", i64::from(r.rank)),
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
    use super::log_parts;
    use crate::telemetry::kinds::dispatch_eligibility::{
        DispatchEligibilityRecord, Eligibility, EligibilityTransition,
        DISPATCH_ELIGIBILITY_LOG_ATTRIBUTE_KEYS, DISPATCH_ELIGIBILITY_SCHEMA_VERSION,
    };
    use crate::telemetry::{RepoVisibility, TelemetryEnvelope, TelemetryRecord};
    use chrono::Utc;

    /// A blocked item (ineligible — an open PR holds it), rank 3.
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

    /// An eligible item (schedulable, held only by capacity), rank 5.
    fn eligible_record() -> DispatchEligibilityRecord {
        DispatchEligibilityRecord {
            issue: 77,
            eligibility: Eligibility::Eligible,
            reason: "deferred_capacity".into(),
            state: "ready".into(),
            rank: 5,
            ..record()
        }
    }

    fn attr_string(log: &opentelemetry_proto::tonic::logs::v1::LogRecord, key: &str) -> String {
        log.attributes
            .iter()
            .find(|a| a.key == key)
            .map(|a| format!("{:?}", a.value))
            .unwrap_or_default()
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
        assert!(attr_string(&log, "loom.repo").contains("rjwalters/loom"));
        assert!(attr_string(&log, "loom.issue").contains("42"));
        assert!(attr_string(&log, "loom.repo.visibility").contains("private"));
        assert!(attr_string(&log, "loom.kind").contains("eta.dispatch_eligibility"));
    }

    /// #12867: the eligibility verdict, blocking reason and rank must ride as
    /// queryable attributes (not body-only), for both a blocked and an eligible
    /// item. Before #12867 these three keys were absent from `log.attributes`,
    /// so each `attr_string(...).contains(...)` below failed (the fact was a
    /// hollow shell in SigNoz's `attributes_string`).
    #[test]
    fn verdict_reason_and_rank_ride_as_attributes_for_blocked_and_eligible() {
        // Blocked item: verdict=blocked, reason=open_pr, rank=3.
        let blocked = log_record_for(&TelemetryEnvelope::new(
            "host-a",
            TelemetryRecord::DispatchEligibility(record()),
        ))
        .expect("blocked record exports as a log");
        assert!(
            attr_string(&blocked, "loom.eta.dispatch_eligibility.eligibility").contains("blocked"),
            "blocked verdict must be a queryable attribute"
        );
        assert!(
            attr_string(&blocked, "loom.eta.dispatch_eligibility.reason").contains("open_pr"),
            "blocking reason must be a queryable attribute"
        );
        assert!(
            attr_string(&blocked, "loom.eta.dispatch_eligibility.rank").contains('3'),
            "candidate rank must be a queryable attribute"
        );

        // Eligible item: verdict=eligible, reason=deferred_capacity, rank=5.
        let eligible = log_record_for(&TelemetryEnvelope::new(
            "host-a",
            TelemetryRecord::DispatchEligibility(eligible_record()),
        ))
        .expect("eligible record exports as a log");
        assert!(
            attr_string(&eligible, "loom.eta.dispatch_eligibility.eligibility")
                .contains("eligible"),
            "eligible verdict must be a queryable attribute"
        );
        assert!(
            attr_string(&eligible, "loom.eta.dispatch_eligibility.reason")
                .contains("deferred_capacity"),
            "reason token must be a queryable attribute for an eligible item too"
        );
        assert!(
            attr_string(&eligible, "loom.eta.dispatch_eligibility.rank").contains('5'),
            "candidate rank must be a queryable attribute"
        );
    }

    /// #12867: every promoted attribute key is declared in
    /// `DISPATCH_ELIGIBILITY_LOG_ATTRIBUTE_KEYS`, and the gateway's
    /// `transform/privacy` log `keep_keys` allowlist retains each one — else the
    /// collector would strip it and SigNoz would be a hollow shell again.
    #[test]
    fn promoted_attributes_are_declared_and_the_collector_keeps_them() {
        const CONFIG: &str =
            include_str!("../../../../../defaults/observability/collector/config.yaml");
        let (_, _, _, attributes, _) =
            log_parts(&TelemetryRecord::DispatchEligibility(record())).unwrap();
        // Every promoted loom.eta.dispatch_eligibility.* attribute is declared.
        for kv in &attributes {
            if kv.key.starts_with("loom.eta.dispatch_eligibility.") {
                assert!(
                    DISPATCH_ELIGIBILITY_LOG_ATTRIBUTE_KEYS.contains(&kv.key.as_str()),
                    "undeclared promoted attribute {}",
                    kv.key
                );
            }
        }
        // Each declared key is actually emitted.
        for key in DISPATCH_ELIGIBILITY_LOG_ATTRIBUTE_KEYS {
            assert!(
                attributes.iter().any(|kv| kv.key == *key),
                "declared key {key} is not emitted"
            );
        }
        // And the gateway keeps each one.
        let log_keep = CONFIG
            .lines()
            .find(|l| l.contains("keep_keys(attributes, [") && l.contains("loom.eta.pr.state"))
            .expect("the transform/privacy log keep_keys line");
        for key in DISPATCH_ELIGIBILITY_LOG_ATTRIBUTE_KEYS {
            assert!(log_keep.contains(&format!("\"{key}\"")), "collector drops {key}");
        }
    }
}
