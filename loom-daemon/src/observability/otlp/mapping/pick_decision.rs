//! OTLP mapping for `pick.decision` (#10212).
//!
//! One log record per chunk: a tick whose JSON exceeds `CHUNK_BYTES` is
//! split into bounded chunks, each exported as its own log record (see
//! `telemetry-kind-pick-decision.md`). The **body** is the chunk's JSON, so
//! ClickHouse
//! can `JSONExtract` the ranked candidate list (see `telemetry-schema.md` for
//! the rank query). The attributes here are `loom.role` and, on a role
//! record, `loom.repo` (#11858); `log_record_for`
//! adds `loom.kind` (`pick.decision`, the key the SigNoz queries filter on,
//! #9881) to every log kind centrally (#10899). Both are already on the
//! collector's `keep_keys` allowlist, so no collector change is needed.
//! The record time is the tick's `ended_at`.

use opentelemetry_proto::tonic::common::v1::KeyValue;
use opentelemetry_proto::tonic::logs::v1::SeverityNumber;

use super::{kv_string, nanos};
use crate::telemetry::TelemetryRecord;

/// `(event_name, severity, record time, attributes, body)` for a
/// `pick.decision`; `None` for every other kind.
pub(super) fn log_parts(
    record: &TelemetryRecord,
) -> Option<(&'static str, SeverityNumber, u64, Vec<KeyValue>, String)> {
    let TelemetryRecord::PickDecision(r) = record else {
        return None;
    };
    let mut attributes = vec![kv_string("loom.role", r.role.clone())];
    // #11858: a role record names its repo, so "which repos have empty
    // queues" is an attribute filter, not a body parse of empty candidates.
    if let Some(repo) = &r.repo {
        attributes.push(kv_string("loom.repo", repo.clone()));
    }
    let body = serde_json::to_string(r).unwrap_or_default();
    Some(("pick.decision", SeverityNumber::Info, nanos(r.ended_at), attributes, body))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::{any_value, log_record_for};
    use crate::telemetry::kinds::pick_decision::{
        split_into_chunks, PickCandidate, PickDecisionRecord, PickDraw, PickDrawCandidate,
        PickTick, PickVerdict, PickWorkspaceDraw,
    };
    use crate::telemetry::{TelemetryEnvelope, TelemetryRecord};
    use chrono::Utc;

    fn tick() -> PickTick {
        let now = Utc::now();
        PickTick {
            role: "judge".into(),
            host: "h".into(),
            tick_id: "t".into(),
            started_at: now,
            ended_at: now,
            outcome: "success".into(),
        }
    }

    #[test]
    fn maps_to_a_log_whose_body_is_the_record_json() {
        let record = PickDecisionRecord::build(
            tick(),
            vec![(
                PickCandidate {
                    rank: 1,
                    repo: "o/r".into(),
                    number: 7,
                    stage: "loom:review-requested".into(),
                    sort_key: None,
                },
                PickVerdict::Undecided,
            )],
        );
        let envelope = TelemetryEnvelope::new("h", TelemetryRecord::PickDecision(record));
        let log = log_record_for(&envelope).expect("pick.decision exports as a log");
        assert_eq!(log.event_name, "pick.decision");
        let body = format!("{:?}", log.body);
        assert!(body.contains("candidates_total"), "{body}");
        assert!(log.attributes.iter().any(|a| a.key == "loom.role"));
        let kind = log
            .attributes
            .iter()
            .find(|a| a.key == "loom.kind")
            .expect("loom.kind");
        assert!(format!("{:?}", kind.value).contains("pick.decision"), "{kind:?}");
    }

    /// #11567: a chunk's body carries `chunk_index` / `chunk_count` and the
    /// tick header, so ClickHouse can reassemble and filter per tick.
    #[test]
    fn a_chunked_record_keeps_the_chunk_fields_and_tick_id_in_its_body() {
        let draws = (0u32..200)
            .map(|d| PickDraw {
                candidates: vec![PickDrawCandidate {
                    repo: format!("o/w{d}"),
                    weight: u64::from(10 + d),
                    base_weight: None,
                    bonus: None,
                }],
                total_weight: u64::from(10 + d),
                roll: u64::from(d),
                picked: format!("o/w{d}"),
                number: d,
            })
            .collect();
        let record = PickDecisionRecord::build(tick(), Vec::new()).with_workspace_draw(Some(
            PickWorkspaceDraw {
                seed: "42".into(),
                draws_total: 200,
                draws,
                seed_scheme: Some("loom-draw/v1".into()),
                seed_minute: Some("29000000".into()),
                seed_host: Some("host-a".into()),
            },
        ));
        for chunk in split_into_chunks(record, 2_000) {
            let envelope = TelemetryEnvelope::new("h", TelemetryRecord::PickDecision(chunk));
            let log = log_record_for(&envelope).expect("pick.decision exports as a log");
            let Some(any_value::Value::StringValue(json)) =
                log.body.as_ref().and_then(|b| b.value.as_ref())
            else {
                panic!("pick.decision body is a string: {:?}", log.body);
            };
            let body: serde_json::Value = serde_json::from_str(json).expect("json body");
            assert!(body["chunk_index"].is_u64(), "{body}");
            assert_eq!(body["tick_id"], "t");
            assert_eq!(body["workspace_draw"]["seed"], "42");
            assert_eq!(body["workspace_draw"]["draws_total"], 200);
            assert_eq!(body["workspace_draw"]["seed_scheme"], "loom-draw/v1");
            assert_eq!(body["workspace_draw"]["seed_minute"], "29000000");
            assert_eq!(body["workspace_draw"]["seed_host"], "host-a");
        }
    }

    /// #11858: a role record carries `loom.repo` even with no candidates, and
    /// the log carries the trace its envelope was joined to.
    #[test]
    fn a_role_record_names_its_repo_and_trace() {
        let mut record = PickDecisionRecord::build(tick(), Vec::new());
        record.repo = Some("o/r".into());
        let mut envelope = TelemetryEnvelope::new("h", TelemetryRecord::PickDecision(record));
        let context = crate::telemetry::trace::TraceContext::derived("t", &["11858"]);
        envelope.trace_context = Some(context.clone());
        let log = log_record_for(&envelope).expect("pick.decision exports as a log");
        let repo = log.attributes.iter().find(|a| a.key == "loom.repo");
        assert!(format!("{repo:?}").contains("o/r"), "{repo:?}");
        assert_eq!(log.trace_id, context.trace_id.bytes());
        let work_finder = PickDecisionRecord::build(tick(), Vec::new());
        let envelope = TelemetryEnvelope::new("h", TelemetryRecord::PickDecision(work_finder));
        let log = log_record_for(&envelope).expect("pick.decision exports as a log");
        assert!(!log.attributes.iter().any(|a| a.key == "loom.repo"));
    }

    /// #12083: a skipped row's disposition, halt cause and hold expiry reach
    /// the OTLP log body, the surface the consumer reads.
    #[test]
    fn a_skipped_row_exports_disposition_halt_cause_and_held_until() {
        use crate::telemetry::kinds::pick_decision::{PickQueueSkip, PickSkipReason};
        use crate::types::QueueDisposition;
        use chrono::TimeZone;
        let until = Utc.with_ymd_and_hms(2026, 10, 14, 12, 30, 0).unwrap();
        let skipped = |number, disposition, detail: Option<&str>, held_until| {
            (
                PickCandidate {
                    rank: number,
                    repo: "o/r".into(),
                    number,
                    stage: "loom:issue".into(),
                    sort_key: None,
                },
                PickVerdict::SkippedQueue(PickQueueSkip {
                    reason: PickSkipReason::from_disposition(disposition)
                        .expect("a skip disposition"),
                    disposition,
                    detail: detail.map(str::to_string),
                    held_until,
                }),
            )
        };
        let record = PickDecisionRecord::build(
            tick(),
            vec![
                skipped(1, QueueDisposition::WorkspaceHalted, Some("drain"), None),
                skipped(2, QueueDisposition::DispatchBackoff, None, Some(until)),
            ],
        );
        let envelope = TelemetryEnvelope::new("h", TelemetryRecord::PickDecision(record));
        let log = log_record_for(&envelope).expect("pick.decision exports as a log");
        let body = format!("{:?}", log.body);
        for needle in [
            r#"disposition\":\"workspace_halted"#,
            r#"detail\":\"drain"#,
            r#"disposition\":\"dispatch_backoff"#,
            "2026-10-14T12:30:00Z",
        ] {
            assert!(body.contains(needle), "{needle} missing from {body}");
        }
    }
}
