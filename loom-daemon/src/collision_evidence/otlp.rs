//! OTLP payload construction for collision-evidence records (#9786): the
//! SigNoz surface consumes the existing OTLP logs pipeline — this module
//! builds the log-record bodies with deterministic trace identity and keeps
//! high-cardinality ids in record fields, never as metric dimensions
//! (defaults/docs/trace-identity.md contract).
//!
//! Delivery is best-effort and outside dispatch authority: a telemetry
//! outage must never change scheduling or invalidate cached results. The
//! actual HTTPS transport reuses Loom's existing OTLP exporter path; this
//! module only builds the payload bodies so tests can assert them offline.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One OTLP log record body for a collision-evidence record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvidenceLogRecord {
    /// Deterministic record id (the record's own hash).
    pub id: String,
    /// `prediction` | `outcome`.
    pub record_kind: String,
    /// `OWNER/REPO`.
    pub repo: String,
    /// RFC3339 observation timestamp.
    pub observed_at: String,
    /// Full canonical record JSON (attributes carry the complete body so a
    /// query can reconstruct without the durable artifact).
    pub body: Value,
    /// Schema version of the record contract.
    pub schema_version: u32,
    /// Source provenance (defaults/docs/trace-identity.md: every span
    /// records Loom version + full SHA).
    pub loom_version: String,
    pub source_sha: String,
}

/// Build log-record bodies for a set of canonical record lines. Records
/// whose `id` fails to parse are skipped and **counted** — never silently
/// dropped (#9786 completeness requirement).
pub fn build_log_records(
    lines: &[(String, String)],
    repo: &str,
    loom_version: &str,
    source_sha: &str,
    now: &str,
) -> (Vec<EvidenceLogRecord>, usize) {
    let mut out = Vec::new();
    let mut skipped = 0usize;
    for (id, line) in lines {
        let Ok(body) = serde_json::from_str::<Value>(line) else {
            skipped += 1;
            continue;
        };
        let Some(record_kind) = body.get("schema_version").and_then(|v| {
            if v.as_u64() == Some(1) {
                body.get("directed_eval_id")
                    .map(|_| "outcome")
                    .or_else(|| body.get("unordered_pair_id").map(|_| "prediction"))
            } else {
                None
            }
        }) else {
            skipped += 1;
            continue;
        };
        let schema_version = body
            .get("schema_version")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        out.push(EvidenceLogRecord {
            id: id.clone(),
            record_kind: record_kind.into(),
            repo: repo.into(),
            observed_at: now.into(),
            body,
            schema_version,
            loom_version: loom_version.into(),
            source_sha: source_sha.into(),
        });
    }
    (out, skipped)
}

/// Build the OTLP JSON payload wrapping the log records (resource + scope
/// shape matches the existing exporter's JSON encoding).
pub fn build_otlp_payload(records: &[EvidenceLogRecord]) -> Value {
    let log_records: Vec<Value> = records
        .iter()
        .map(|r| {
            serde_json::json!({
                "timeUnixNano": r.observed_at.clone(),
                "severityText": "INFO",
                "body": { "stringValue": serde_json::to_string(&r.body).unwrap_or_default() },
                "attributes": [
                    { "key": "collision.evidence.id", "value": { "stringValue": r.id.clone() } },
                    { "key": "collision.evidence.record_kind", "value": { "stringValue": r.record_kind.clone() } },
                    { "key": "collision.evidence.schema_version", "value": { "intValue": r.schema_version } },
                    { "key": "loom.version", "value": { "stringValue": r.loom_version.clone() } },
                    { "key": "loom.source_sha", "value": { "stringValue": r.source_sha.clone() } },
                ],
            })
        })
        .collect();
    serde_json::json!({
        "resourceLogs": [{
            "resource": {
                "attributes": [
                    { "key": "service.name", "value": { "stringValue": "loom-daemon" } },
                ],
            },
            "scopeLogs": [{
                "scope": { "name": "collision-evidence", "version": "v1" },
                "logRecords": log_records,
            }],
        }],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRED: &str = r#"{"schema_version":1,"unordered_pair_id":"p1","id":"i-1"}"#;
    const OUT: &str = r#"{"schema_version":1,"directed_eval_id":"e1","id":"i-2"}"#;

    #[test]
    fn builds_records_and_counts_skips() {
        let lines = vec![
            ("i-1".to_string(), PRED.to_string()),
            ("i-2".to_string(), OUT.to_string()),
            ("i-3".to_string(), "not json".to_string()),
        ];
        let (records, skipped) =
            build_log_records(&lines, "o/r", "v-test", "sha-test", "2026-10-01T00:00:00Z");
        assert_eq!(records.len(), 2);
        assert_eq!(skipped, 1, "skips are counted, never silent");
        assert_eq!(records[0].record_kind, "prediction");
        assert_eq!(records[1].record_kind, "outcome");
    }

    #[test]
    fn payload_has_no_high_cardinality_dimensions() {
        let (records, _) =
            build_log_records(&[("i-1".into(), PRED.into())], "o/r", "v", "sha", "now");
        let payload = build_otlp_payload(&records);
        let text = serde_json::to_string(&payload).unwrap();
        // The pair id / sha must ride in the record body (stringValue of
        // body), not as a metric dimension key.
        assert!(text.contains("collision.evidence.id"));
        assert!(text.contains("loom.version"));
        assert!(!text.contains("collision.pair.jaccard"));
    }
}
