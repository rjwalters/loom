//! OTLP mapping for `operator.presence` (#11563): one log record per presence
//! beat, stamped at the beat's `captured_at`. The `loom.presence.*` scalars
//! ride as attributes ([`OPERATOR_PRESENCE_LOG_ATTRIBUTE_KEYS`], which the
//! collector's log `keep_keys` must list; contract-tested). The body is the
//! record's JSON, which carries the `driving[]` entries. `host` is not an
//! attribute here — it rides on the Resource as `host.id`, like every other
//! kind. No prompt or transcript content can appear: the record has no such
//! field.

use opentelemetry_proto::tonic::common::v1::KeyValue;
use opentelemetry_proto::tonic::logs::v1::SeverityNumber;

use super::{kv_int, kv_string, nanos};
use crate::telemetry::TelemetryRecord;

/// `(event_name, severity, record time, attributes, body)` for an
/// `operator.presence` record; `None` for every other kind.
pub(super) fn log_parts(
    record: &TelemetryRecord,
) -> Option<(&'static str, SeverityNumber, u64, Vec<KeyValue>, String)> {
    let TelemetryRecord::OperatorPresence(r) = record else {
        return None;
    };
    let mut attributes = vec![
        kv_string("loom.presence.session_id", r.session_id.clone()),
        kv_string("loom.presence.last_activity_at", r.last_activity_at.to_rfc3339()),
        kv_int(
            "loom.presence.driving_count",
            i64::try_from(r.driving.len()).unwrap_or(i64::MAX),
        ),
    ];
    if let Some(operator) = &r.operator {
        attributes.push(kv_string("loom.presence.operator", operator.clone()));
    }
    if let Some(at) = r.last_input_at {
        attributes.push(kv_string("loom.presence.last_input_at", at.to_rfc3339()));
    }
    let body = serde_json::to_string(r).unwrap_or_default();
    Some((
        "operator.presence",
        SeverityNumber::Info,
        nanos(r.captured_at),
        attributes,
        body,
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::telemetry::kinds::operator_presence::{
        DrivenItem, OperatorPresenceRecord, OPERATOR_PRESENCE_LOG_ATTRIBUTE_KEYS,
    };

    fn record() -> OperatorPresenceRecord {
        OperatorPresenceRecord {
            captured_at: chrono::Utc::now(),
            host: "host-a".to_string(),
            session_id: "sess-xyz".to_string(),
            operator: Some("alice".to_string()),
            last_input_at: Some(chrono::Utc::now()),
            last_activity_at: chrono::Utc::now(),
            driving: vec![DrivenItem {
                kind: "issue".to_string(),
                number: 11563,
                sweep_id: Some("sweep-insession-1".to_string()),
            }],
        }
    }

    #[test]
    fn attributes_are_declared_and_the_collector_keeps_them() {
        const CONFIG: &str =
            include_str!("../../../../../defaults/observability/collector/config.yaml");
        let (name, _, _, attributes, body) =
            log_parts(&TelemetryRecord::OperatorPresence(record())).unwrap();
        assert_eq!(name, "operator.presence");
        // Scalars ride as attributes; the driving list is in the body only.
        assert!(attributes
            .iter()
            .any(|kv| kv.key == "loom.presence.session_id"));
        assert!(attributes
            .iter()
            .any(|kv| kv.key == "loom.presence.driving_count"));
        assert!(body.contains("\"driving\""), "{body}");
        assert!(body.contains("11563"), "{body}");
        // Every emitted attribute key is declared and admitted by the collector.
        for kv in &attributes {
            assert!(
                OPERATOR_PRESENCE_LOG_ATTRIBUTE_KEYS.contains(&kv.key.as_str()),
                "undeclared attribute key {}",
                kv.key
            );
        }
        let log_keep = CONFIG
            .lines()
            .find(|l| l.contains("keep_keys(attributes, [") && l.contains("loom.eta.pr.state"))
            .expect("the transform/privacy log keep_keys line");
        for key in OPERATOR_PRESENCE_LOG_ATTRIBUTE_KEYS {
            assert!(log_keep.contains(&format!("\"{key}\"")), "collector drops {key}");
        }
    }

    /// No prompt or transcript content can ride: the record has no such field,
    /// so neither the attributes nor the JSON body can carry one.
    #[test]
    fn no_text_content_beyond_ids_and_login() {
        let (_, _, _, attributes, body) =
            log_parts(&TelemetryRecord::OperatorPresence(record())).unwrap();
        // Only ids, a login and timestamps — the attribute values are drawn
        // from exactly those fields.
        for kv in &attributes {
            assert!(kv.key.starts_with("loom.presence."), "{}", kv.key);
        }
        // The body is the record's own serialization; its only string fields
        // are ids, the host, the operator login and the driving entries.
        assert!(body.contains("sess-xyz"));
        assert!(!body.contains("prompt"), "{body}");
    }

    /// Optional fields drop cleanly: no `operator` / `last_input_at` attributes
    /// when the session does not know them.
    #[test]
    fn optional_fields_omitted_when_absent() {
        let mut r = record();
        r.operator = None;
        r.last_input_at = None;
        let (_, _, _, attributes, _) = log_parts(&TelemetryRecord::OperatorPresence(r)).unwrap();
        assert!(!attributes
            .iter()
            .any(|kv| kv.key == "loom.presence.operator"));
        assert!(!attributes
            .iter()
            .any(|kv| kv.key == "loom.presence.last_input_at"));
        // The always-present scalars are still there.
        assert!(attributes
            .iter()
            .any(|kv| kv.key == "loom.presence.last_activity_at"));
    }
}
