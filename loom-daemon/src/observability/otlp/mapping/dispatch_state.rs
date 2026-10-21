//! OTLP mapping for `eta.dispatch_state` (#12918).
//!
//! One log record per classified item per emission. The **body** is the
//! record's JSON (all fields, including `iw`/`weight`/`host`). Alongside it
//! ride:
//! - `loom.repo`, `loom.repo.visibility`, and `loom.issue` or `loom.pr_number`
//!   (by the item's artifact) — the join keys shared with the other `eta.*`
//!   facts; and
//! - `loom.eta.dispatch_state.*` ([`DISPATCH_STATE_LOG_ATTRIBUTE_KEYS`]) — the
//!   gate itself, so SigNoz can filter on it: artifact, step, `in_pool`,
//!   `exclusion`, and (only when known) `gate_until`, `reeval_rung`,
//!   `reeval_next`. An absent value is an absent attribute, never `0`/`""`.
//!
//! Every promoted key is on the gateway's `transform/privacy` log `keep_keys`
//! (the `attributes_are_declared_and_the_collector_keeps_them` test enforces
//! it). The record time is the emission instant (`sampled_at`).

use opentelemetry_proto::tonic::common::v1::KeyValue;
use opentelemetry_proto::tonic::logs::v1::SeverityNumber;

use super::{kv_int, kv_string, nanos};
use crate::telemetry::kinds::dispatch_state::Artifact;
use crate::telemetry::TelemetryRecord;

fn rfc3339(at: chrono::DateTime<chrono::Utc>) -> String {
    at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// `(event_name, severity, record time, attributes, body)` for an
/// `eta.dispatch_state` record; `None` for every other kind.
pub(super) fn log_parts(
    record: &TelemetryRecord,
) -> Option<(&'static str, SeverityNumber, u64, Vec<KeyValue>, String)> {
    let TelemetryRecord::DispatchState(r) = record else {
        return None;
    };
    let mut attributes = vec![
        kv_string("loom.repo", r.repo.clone()),
        kv_string("loom.repo.visibility", r.visibility.as_str()),
        match r.artifact {
            Artifact::Issue => kv_int("loom.issue", i64::from(r.number)),
            Artifact::Pr => kv_int("loom.pr_number", i64::from(r.number)),
        },
        kv_string("loom.eta.dispatch_state.artifact", r.artifact.as_str()),
        kv_string("loom.eta.dispatch_state.in_pool", if r.in_pool { "true" } else { "false" }),
    ];
    if let Some(step) = &r.step {
        attributes.push(kv_string("loom.eta.dispatch_state.step", step.clone()));
    }
    if let Some(exclusion) = &r.exclusion {
        attributes.push(kv_string("loom.eta.dispatch_state.exclusion", exclusion.clone()));
    }
    if let Some(until) = r.gate_until {
        attributes.push(kv_string("loom.eta.dispatch_state.gate_until", rfc3339(until)));
    }
    if let Some(rung) = r.reeval_rung {
        attributes.push(kv_int("loom.eta.dispatch_state.reeval_rung", i64::from(rung)));
    }
    if let Some(next) = r.reeval_next {
        attributes.push(kv_string("loom.eta.dispatch_state.reeval_next", rfc3339(next)));
    }
    let body = serde_json::to_string(r).unwrap_or_default();
    Some((
        "eta.dispatch_state",
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
    use crate::telemetry::kinds::dispatch_state::{
        Artifact, DispatchStateRecord, StateTransition, DISPATCH_STATE_LOG_ATTRIBUTE_KEYS,
        DISPATCH_STATE_SCHEMA_VERSION,
    };
    use crate::telemetry::{RepoVisibility, TelemetryEnvelope, TelemetryRecord};
    use chrono::Utc;

    /// A cooling-down merge step on a PR, with a ladder marker: every
    /// optional field present.
    fn record() -> DispatchStateRecord {
        let now = Utc::now();
        DispatchStateRecord {
            schema_version: DISPATCH_STATE_SCHEMA_VERSION,
            repo: "rjwalters/loom".into(),
            host: "host-a".into(),
            number: 42,
            artifact: Artifact::Pr,
            step: Some("champion_merge".into()),
            in_pool: false,
            exclusion: Some("cooling_down".into()),
            gate_until: Some(now),
            reeval_rung: Some(2),
            reeval_next: Some(now),
            iw: None,
            weight: None,
            transition: StateTransition::Changed,
            sampled_at: now,
            visibility: RepoVisibility::Private,
        }
    }

    fn attr(log: &opentelemetry_proto::tonic::logs::v1::LogRecord, key: &str) -> Option<String> {
        log.attributes
            .iter()
            .find(|a| a.key == key)
            .map(|a| format!("{:?}", a.value))
    }

    #[test]
    fn maps_to_a_log_with_the_gate_as_attributes_and_the_record_as_body() {
        let log = log_record_for(&TelemetryEnvelope::new(
            "host-a",
            TelemetryRecord::DispatchState(record()),
        ))
        .expect("eta.dispatch_state exports as a log");
        assert_eq!(log.event_name, "eta.dispatch_state");
        let body = format!("{:?}", log.body);
        assert!(body.contains("cooling_down"), "{body}");
        assert!(attr(&log, "loom.kind")
            .unwrap()
            .contains("eta.dispatch_state"));
        assert!(attr(&log, "loom.repo").unwrap().contains("rjwalters/loom"));
        assert!(attr(&log, "loom.pr_number").unwrap().contains("42"));
        assert!(attr(&log, "loom.issue").is_none(), "a PR row has no loom.issue");
        assert!(attr(&log, "loom.eta.dispatch_state.exclusion")
            .unwrap()
            .contains("cooling_down"));
        assert!(attr(&log, "loom.eta.dispatch_state.in_pool")
            .unwrap()
            .contains("false"));
        assert!(attr(&log, "loom.eta.dispatch_state.reeval_rung")
            .unwrap()
            .contains('2'));
    }

    #[test]
    fn absent_gate_values_are_absent_attributes() {
        let mut r = record();
        r.artifact = Artifact::Issue;
        r.in_pool = true;
        r.exclusion = None;
        r.gate_until = None;
        r.reeval_rung = None;
        r.reeval_next = None;
        let log =
            log_record_for(&TelemetryEnvelope::new("host-a", TelemetryRecord::DispatchState(r)))
                .unwrap();
        assert!(attr(&log, "loom.issue").unwrap().contains("42"));
        for key in ["exclusion", "gate_until", "reeval_rung", "reeval_next"] {
            assert!(
                attr(&log, &format!("loom.eta.dispatch_state.{key}")).is_none(),
                "{key} must be absent, not zero"
            );
        }
    }

    #[test]
    fn attributes_are_declared_and_the_collector_keeps_them() {
        const CONFIG: &str =
            include_str!("../../../../../defaults/observability/collector/config.yaml");
        let (_, _, _, attributes, _) =
            log_parts(&TelemetryRecord::DispatchState(record())).unwrap();
        for kv in &attributes {
            if kv.key.starts_with("loom.eta.dispatch_state.") {
                assert!(
                    DISPATCH_STATE_LOG_ATTRIBUTE_KEYS.contains(&kv.key.as_str()),
                    "undeclared promoted attribute {}",
                    kv.key
                );
            }
        }
        for key in DISPATCH_STATE_LOG_ATTRIBUTE_KEYS {
            assert!(
                attributes.iter().any(|kv| kv.key == *key),
                "declared key {key} is not emitted"
            );
        }
        let log_keep = CONFIG
            .lines()
            .find(|l| l.contains("keep_keys(attributes, [") && l.contains("loom.eta.pr.state"))
            .expect("the transform/privacy log keep_keys line");
        for key in DISPATCH_STATE_LOG_ATTRIBUTE_KEYS {
            assert!(log_keep.contains(&format!("\"{key}\"")), "collector drops {key}");
        }
    }
}
