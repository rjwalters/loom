//! OTLP mapping for `hold.transition` (#12665): one INFO log record per hold
//! label write, stamped at the write. The body is the record's JSON; the
//! scalars ride as `loom.hold.*` attributes.

use opentelemetry_proto::tonic::common::v1::{any_value, AnyValue, KeyValue};
use opentelemetry_proto::tonic::logs::v1::SeverityNumber;

use super::session_output::LogParts;
use super::{kv, kv_string, nanos};
use crate::telemetry::TelemetryRecord;

fn kv_bool(key: &str, value: bool) -> KeyValue {
    kv(
        key,
        AnyValue {
            value: Some(any_value::Value::BoolValue(value)),
        },
    )
}

pub(super) fn log_parts(record: &TelemetryRecord) -> Option<LogParts> {
    let TelemetryRecord::HoldTransition(r) = record else {
        return None;
    };
    if !r.loom.is_valid() {
        return None;
    }
    let mut attributes = vec![
        kv_string("loom.repo", &r.repo),
        kv(
            "loom.pr_number",
            AnyValue {
                value: Some(any_value::Value::IntValue(i64::from(r.pr))),
            },
        ),
        kv_string("loom.hold.transition", &r.transition),
        kv_string("loom.hold.reason_code", &r.reason_code),
        kv_string("loom.hold.trigger", &r.trigger),
        kv_bool("loom.hold.confirmed", r.confirmed),
        kv_bool("loom.hold.entered_at_lower_bound", r.entered_at_lower_bound),
        kv_string("loom.hold.version", &r.loom.version),
        kv_string("loom.hold.revision", &r.loom.revision),
        kv_bool("loom.hold.provenance_complete", r.loom.complete),
    ];
    if let Some(id) = &r.episode_id {
        attributes.push(kv_string("loom.hold.episode_id", id));
    }
    if let Some(at) = r.entered_at {
        attributes.push(kv_string("loom.hold.entered_at", at.to_rfc3339()));
    }
    Some(LogParts {
        event_name: "hold.transition",
        severity: SeverityNumber::Info,
        source_at: nanos(r.event_at),
        observed_at: nanos(r.observed_at),
        attributes,
        body: serde_json::to_string(r).ok()?,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::super::log_record_for;
    use super::*;
    use crate::telemetry::kinds::hold_transition::{
        HoldTransitionRecord, HOLD_TRANSITION_LOG_ATTRIBUTE_KEYS,
    };
    use crate::telemetry::provenance::Provenance;
    use crate::telemetry::TelemetryEnvelope;

    fn record() -> HoldTransitionRecord {
        let at = Utc.with_ymd_and_hms(2026, 10, 17, 12, 0, 0).unwrap();
        HoldTransitionRecord {
            repo: "o/r".into(),
            pr: 42,
            transition: "add".into(),
            reason_code: "merge-risk-hold".into(),
            trigger: "champion_role".into(),
            confirmed: true,
            episode_id: Some("o/r#42@1792238400".into()),
            entered_at: Some(at),
            entered_at_lower_bound: false,
            event_at: at,
            observed_at: at + chrono::Duration::seconds(30),
            loom: Provenance {
                version: "0.19.1".into(),
                revision: "9d8e226ce0123456789abcdef0123456789abcde".into(),
                tree_state: "clean".into(),
                complete: true,
            },
        }
    }

    #[test]
    fn every_attribute_is_declared_and_stamped_at_the_write() {
        let r = record();
        let envelope = TelemetryEnvelope::new("host-1", TelemetryRecord::HoldTransition(r.clone()));
        let log = log_record_for(&envelope).unwrap();
        assert_eq!(log.event_name, "hold.transition");
        assert_eq!(log.time_unix_nano, nanos(r.event_at));
        assert_eq!(log.observed_time_unix_nano, nanos(r.observed_at));
        let keys: Vec<&str> = log.attributes.iter().map(|kv| kv.key.as_str()).collect();
        for key in HOLD_TRANSITION_LOG_ATTRIBUTE_KEYS {
            assert!(keys.contains(key), "{key} is emitted");
        }
        for key in keys {
            assert!(
                HOLD_TRANSITION_LOG_ATTRIBUTE_KEYS.contains(&key)
                    || ["loom.record_id", "loom.kind", "loom.pr_number", "loom.repo"]
                        .contains(&key),
                "{key} is not allowlisted"
            );
        }
    }

    #[test]
    fn an_unknown_entry_is_flagged_not_stamped() {
        let mut r = record();
        r.transition = "remove".into();
        r.episode_id = None;
        r.entered_at = None;
        r.entered_at_lower_bound = true;
        let envelope = TelemetryEnvelope::new("host-1", TelemetryRecord::HoldTransition(r));
        let log = log_record_for(&envelope).unwrap();
        for key in ["loom.hold.entered_at", "loom.hold.episode_id"] {
            assert!(!log.attributes.iter().any(|kv| kv.key == key), "{key}");
        }
        assert!(log
            .attributes
            .iter()
            .any(|kv| kv.key == "loom.hold.entered_at_lower_bound"));
    }

    #[test]
    fn the_collector_keeps_every_hold_attribute() {
        const CONFIG: &str =
            include_str!("../../../../../defaults/observability/collector/config.yaml");
        let keep = CONFIG
            .lines()
            .find(|l| l.contains("keep_keys(attributes, [") && l.contains("loom.ci.chunk_index"))
            .unwrap();
        for key in HOLD_TRANSITION_LOG_ATTRIBUTE_KEYS {
            assert!(keep.contains(&format!("\"{key}\"")), "collector drops {key}");
        }
    }
}
