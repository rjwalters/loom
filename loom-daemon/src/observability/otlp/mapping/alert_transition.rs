//! OTLP mapping for `alert.transition` (#12806): one log record per alert
//! transition, in loom-ui's schema (`docs/alerts/external.md` § "As telemetry
//! (shadow)" there). The body and the `event.name` attribute are both the
//! kind; the fields ride as `alert.*` string attributes; the record is stamped
//! at `observed_at`.
//!
//! Severity: `open` maps to WARN or ERROR by the alert's severity, `resolved`
//! to INFO. The reader keys on the attributes, never on the log severity.

use opentelemetry_proto::tonic::common::v1::KeyValue;
use opentelemetry_proto::tonic::logs::v1::SeverityNumber;

use super::{kv_string, nanos};
use crate::telemetry::kinds::alert_transition::ALERT_TRANSITION_EVENT_NAME;
use crate::telemetry::TelemetryRecord;

/// `(event_name, severity, record time, attributes, body)`; `None` for every
/// other kind.
pub(super) fn log_parts(
    record: &TelemetryRecord,
) -> Option<(&'static str, SeverityNumber, u64, Vec<KeyValue>, String)> {
    let TelemetryRecord::AlertTransition(r) = record else {
        return None;
    };
    let mut attributes = vec![
        kv_string("event.name", ALERT_TRANSITION_EVENT_NAME),
        kv_string("alert.source", r.source.clone()),
        kv_string("alert.rule", r.rule.clone()),
        kv_string("alert.key", r.key.clone()),
        kv_string("alert.state", r.state.clone()),
        kv_string("alert.severity", r.severity.clone()),
        kv_string("alert.title", r.title.clone()),
        kv_string("alert.observed_at", r.observed_at_wire()),
    ];
    if let Some(detail) = &r.detail {
        attributes.push(kv_string("alert.detail", detail.clone()));
    }
    if let Some(link) = &r.link {
        attributes.push(kv_string("alert.link", link.clone()));
    }
    let severity = match (r.state.as_str(), r.severity.as_str()) {
        ("open", "critical") => SeverityNumber::Error,
        ("open", _) => SeverityNumber::Warn,
        _ => SeverityNumber::Info,
    };
    Some((
        ALERT_TRANSITION_EVENT_NAME,
        severity,
        nanos(r.observed_at),
        attributes,
        ALERT_TRANSITION_EVENT_NAME.to_string(),
    ))
}

/// The generic bound ([`super::metadata::bounded`]) drops any string over 256
/// bytes or with a control character, which would drop a long title or a
/// multi-line detail, and the schema requires the title. The `alert.*` values
/// are already fitted to the schema's own limits
/// ([`crate::alerts::sanitize`]), so they bypass it; everything else on the
/// record still goes through it.
pub(super) fn bounded(attributes: Vec<KeyValue>) -> Vec<KeyValue> {
    let (alert, rest): (Vec<KeyValue>, Vec<KeyValue>) = attributes
        .into_iter()
        .partition(|kv| kv.key.starts_with("alert."));
    let mut out = super::metadata::bounded(rest);
    out.extend(alert);
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use chrono::{TimeZone, Utc};
    use opentelemetry_proto::tonic::common::v1::any_value;

    use super::super::log_record_for;
    use super::*;
    use crate::telemetry::kinds::alert_transition::{
        AlertTransitionRecord, ALERT_TRANSITION_LOG_ATTRIBUTE_KEYS,
    };
    use crate::telemetry::TelemetryEnvelope;

    fn record(state: &str) -> AlertTransitionRecord {
        AlertTransitionRecord {
            source: "loom-worker-1".into(),
            rule: "disk-low".into(),
            key: "var".into(),
            state: state.into(),
            severity: "warning".into(),
            title: "Disk /var is 91% full".into(),
            observed_at: Utc.with_ymd_and_hms(2026, 10, 19, 12, 0, 0).unwrap(),
            detail: None,
            link: None,
        }
    }

    fn attrs(r: AlertTransitionRecord) -> Vec<(String, String)> {
        let envelope = TelemetryEnvelope::new("loom-worker-1", TelemetryRecord::AlertTransition(r));
        let log = log_record_for(&envelope).unwrap();
        log.attributes
            .into_iter()
            .map(|kv| {
                let value = match kv.value.unwrap().value.unwrap() {
                    any_value::Value::StringValue(s) => s,
                    other => panic!("{} is not a string: {other:?}", kv.key),
                };
                (kv.key, value)
            })
            .collect()
    }

    fn keys(attributes: &[(String, String)]) -> Vec<&str> {
        let mut keys: Vec<&str> = attributes
            .iter()
            .map(|(k, _)| k.as_str())
            .filter(|k| !k.starts_with("loom."))
            .collect();
        keys.sort_unstable();
        keys
    }

    fn sorted(list: &[&'static str]) -> Vec<&'static str> {
        let mut v = list.to_vec();
        v.sort_unstable();
        v
    }

    const REQUIRED: &[&str] = &[
        "event.name",
        "alert.source",
        "alert.rule",
        "alert.key",
        "alert.state",
        "alert.severity",
        "alert.title",
        "alert.observed_at",
    ];

    #[test]
    fn body_and_event_name_are_the_kind_stamped_at_observed_at() {
        let r = record("open");
        let envelope =
            TelemetryEnvelope::new("loom-worker-1", TelemetryRecord::AlertTransition(r.clone()));
        let log = log_record_for(&envelope).unwrap();
        assert_eq!(log.event_name, "alert.transition");
        let body = match log.body.unwrap().value.unwrap() {
            any_value::Value::StringValue(s) => s,
            other => panic!("body is not a string: {other:?}"),
        };
        assert_eq!(body, "alert.transition");
        assert_eq!(log.time_unix_nano, nanos(r.observed_at));
        let a = attrs(r);
        let get = |k: &str| a.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
        assert_eq!(get("event.name"), Some("alert.transition"));
        assert_eq!(get("alert.observed_at"), Some("2026-10-19T12:00:00.000Z"));
        assert_eq!(get("alert.state"), Some("open"));
        assert_eq!(get("alert.severity"), Some("warning"));
    }

    #[test]
    fn open_and_resolved_carry_exactly_the_documented_keys() {
        // Bare: the required keys only, on open and on resolve alike.
        for state in ["open", "resolved"] {
            assert_eq!(keys(&attrs(record(state))), sorted(REQUIRED), "{state}");
        }
        // With the optional keys: exactly the documented list.
        let mut full = record("open");
        full.detail = Some("91% used,\n4.2 GB free".into());
        full.link = Some("https://github.com/o/r/issues/1".into());
        assert_eq!(keys(&attrs(full)), sorted(ALERT_TRANSITION_LOG_ATTRIBUTE_KEYS));
        for key in REQUIRED {
            assert!(ALERT_TRANSITION_LOG_ATTRIBUTE_KEYS.contains(key), "{key} is declared");
        }
    }

    #[test]
    fn a_long_title_and_a_multi_line_detail_survive_the_attribute_bound() {
        let mut r = record("open");
        r.title = "é".repeat(200);
        r.detail = Some(format!("{}\nsecond line", "x".repeat(1900)));
        let a = attrs(r.clone());
        let get = |k: &str| a.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone());
        assert_eq!(get("alert.title"), Some(r.title));
        assert_eq!(get("alert.detail"), r.detail);
    }

    #[test]
    fn the_collector_keeps_every_alert_attribute() {
        const CONFIG: &str =
            include_str!("../../../../../defaults/observability/collector/config.yaml");
        let keep = CONFIG
            .lines()
            .find(|l| l.contains("keep_keys(attributes, [") && l.contains("loom.ci.chunk_index"))
            .unwrap();
        for key in ALERT_TRANSITION_LOG_ATTRIBUTE_KEYS {
            assert!(keep.contains(&format!("\"{key}\"")), "collector drops {key}");
        }
    }
}
