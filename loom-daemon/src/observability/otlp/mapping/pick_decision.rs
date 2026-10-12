//! OTLP mapping for `pick.decision` (#10212).
//!
//! One log record per tick. The **body** is the record's JSON, so ClickHouse
//! can `JSONExtract` the ranked candidate list (see `telemetry-schema.md` for
//! the rank query). The one attribute here is `loom.role`; `log_record_for`
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
    let attributes = vec![kv_string("loom.role", r.role.clone())];
    let body = serde_json::to_string(r).unwrap_or_default();
    Some(("pick.decision", SeverityNumber::Info, nanos(r.ended_at), attributes, body))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::log_record_for;
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
                pool: "all".into(),
                candidates: vec![PickDrawCandidate {
                    repo: format!("o/w{d}"),
                    weight: u64::from(10 + d),
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
            },
        ));
        for chunk in split_into_chunks(record, 2_000) {
            let envelope = TelemetryEnvelope::new("h", TelemetryRecord::PickDecision(chunk));
            let log = log_record_for(&envelope).expect("pick.decision exports as a log");
            let body = serde_json::from_str::<serde_json::Value>(&log.body).expect("json body");
            assert!(body["chunk_index"].is_u64(), "{body}");
            assert_eq!(body["tick_id"], "t");
            assert_eq!(body["workspace_draw"]["seed"], "42");
            assert_eq!(body["workspace_draw"]["draws_total"], 200);
        }
    }
}
