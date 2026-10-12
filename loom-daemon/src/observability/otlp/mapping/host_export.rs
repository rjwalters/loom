//! OTLP mapping for `host.export` (#11124): one log record per `host.health`
//! interval, stamped at the sample time. The body is the record's JSON (the
//! per-exporter entries); the host-wide sums ride as `loom.host_export.*`.
//!
//! `host.lease_id` (#11530) is [`opaque_host_id`] of the record's `host`
//! (which is `host.id`), the `host-XXXXXXXX` form `loom:lease` comments
//! publish, so a SigNoz row can be joined to a lease record. It is derived
//! here, not carried on the wire, so older records still decode. The join
//! only matches when the lease chain (`LOOM_HOST_ID` > `$HOSTNAME`) resolves
//! to the same string as `host.id`; see `telemetry-schema.md`.

use opentelemetry_proto::tonic::common::v1::KeyValue;
use opentelemetry_proto::tonic::logs::v1::SeverityNumber;

use super::{kv_int, kv_string, nanos};
use crate::sweep_registry::opaque_host_id;
use crate::telemetry::TelemetryRecord;

/// `(event_name, severity, record time, attributes, body)` for a `host.export`
/// record; `None` for every other kind.
pub(super) fn log_parts(
    record: &TelemetryRecord,
) -> Option<(&'static str, SeverityNumber, u64, Vec<KeyValue>, String)> {
    let TelemetryRecord::HostExport(r) = record else {
        return None;
    };
    let to_i64 = |n: u64| i64::try_from(n).unwrap_or(i64::MAX);
    let mut attributes = vec![
        kv_int("loom.host_export.exporters", to_i64(r.exporters.len() as u64)),
        kv_int("loom.host_export.queue_depth", to_i64(r.queue_depth_sum())),
        kv_int("loom.host_export.dropped_total", to_i64(r.dropped_total_sum())),
        kv_int("loom.host_export.fact_id_missing_total", to_i64(r.fact_id_missing_total)),
        kv_string("host.lease_id", opaque_host_id(&r.host)),
    ];
    if let Some(at) = r.last_flush_ok_at() {
        attributes.push(kv_string("loom.host_export.last_flush_ok_at", at.to_rfc3339()));
    }
    let severity = if r.exporters.is_empty() {
        SeverityNumber::Warn
    } else {
        SeverityNumber::Info
    };
    let body = serde_json::to_string(r).unwrap_or_default();
    Some(("host.export", severity, nanos(r.captured_at), attributes, body))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::telemetry::kinds::host_export::{
        ExporterExport, HostExportRecord, HOST_EXPORT_LOG_ATTRIBUTE_KEYS,
    };

    #[test]
    fn attributes_are_declared_and_the_collector_keeps_them() {
        const CONFIG: &str =
            include_str!("../../../../../defaults/observability/collector/config.yaml");
        let record = TelemetryRecord::HostExport(HostExportRecord {
            captured_at: chrono::Utc::now(),
            host: "h".to_string(),
            exporters: vec![ExporterExport {
                name: "otlp".to_string(),
                queue_depth: 4,
                dropped_total: 7,
                last_flush_ok_at: Some(chrono::Utc::now()),
            }],
            fact_id_missing_total: 3,
        });
        let (_, _, _, attributes, body) = log_parts(&record).unwrap();
        assert!(body.contains("\"dropped_total\":7"), "{body}");
        assert!(body.contains("\"fact_id_missing_total\":3"), "{body}");
        assert!(
            attributes
                .iter()
                .any(|kv| kv.key == "loom.host_export.fact_id_missing_total"
                    && kv.value.as_ref().and_then(|v| v.value.as_ref())
                        == Some(
                            &opentelemetry_proto::tonic::common::v1::any_value::Value::IntValue(3)
                        )),
            "the count rides as an attribute"
        );
        for kv in &attributes {
            assert!(HOST_EXPORT_LOG_ATTRIBUTE_KEYS.contains(&kv.key.as_str()), "{}", kv.key);
        }
        let log_keep = CONFIG
            .lines()
            .find(|l| l.contains("keep_keys(attributes, [") && l.contains("loom.eta.pr.state"))
            .expect("the transform/privacy log keep_keys line");
        for key in HOST_EXPORT_LOG_ATTRIBUTE_KEYS {
            assert!(log_keep.contains(&format!("\"{key}\"")), "collector drops {key}");
        }
    }

    /// #11530: `host.lease_id` is `opaque_host_id(host.id)`, and the Resource
    /// still carries the raw `host.id`.
    #[test]
    fn lease_id_is_the_opaque_form_of_host_id() {
        use opentelemetry_proto::tonic::common::v1::any_value::Value;
        let record = TelemetryRecord::HostExport(HostExportRecord {
            captured_at: chrono::Utc::now(),
            host: "host-a".to_string(),
            exporters: vec![],
            fact_id_missing_total: 0,
        });
        let (_, _, _, attributes, _) = log_parts(&record).unwrap();
        let lease_id = attributes
            .iter()
            .find(|kv| kv.key == "host.lease_id")
            .and_then(|kv| kv.value.as_ref()?.value.clone());
        assert_eq!(lease_id, Some(Value::StringValue(opaque_host_id("host-a"))));

        let envelope = crate::telemetry::TelemetryEnvelope::new("host-a", record);
        let request = super::super::build_logs_request(&[envelope]).unwrap();
        let resource = request.resource_logs[0].resource.as_ref().unwrap();
        let host_id = resource
            .attributes
            .iter()
            .find(|kv| kv.key == "host.id")
            .and_then(|kv| kv.value.as_ref()?.value.clone());
        assert_eq!(host_id, Some(Value::StringValue("host-a".to_string())));
    }
}
