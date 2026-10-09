//! OTLP mapping for `host.export` (#11124): one log record per `host.health`
//! interval, stamped at the sample time. The body is the record's JSON (the
//! per-exporter entries); the host-wide sums ride as `loom.host_export.*`.

use opentelemetry_proto::tonic::common::v1::KeyValue;
use opentelemetry_proto::tonic::logs::v1::SeverityNumber;

use super::{kv_int, kv_string, nanos};
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
        });
        let (_, _, _, attributes, body) = log_parts(&record).unwrap();
        assert!(body.contains("\"dropped_total\":7"), "{body}");
        for kv in &attributes {
            assert!(HOST_EXPORT_LOG_ATTRIBUTE_KEYS.contains(&kv.key.as_str()), "{}", kv.key);
        }
        let log_keep = CONFIG
            .lines()
            .find(|l| l.contains("keep_keys(attributes, [") && l.contains("loom.eta.estimate_id"))
            .expect("the transform/privacy log keep_keys line");
        for key in HOST_EXPORT_LOG_ATTRIBUTE_KEYS {
            assert!(log_keep.contains(&format!("\"{key}\"")), "collector drops {key}");
        }
    }
}
