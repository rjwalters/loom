use super::*;
use crate::telemetry::trace::{SpanName, SpanRecord, SpanStatus, TraceContext};
fn envelope(sampled: bool) -> TelemetryEnvelope {
    let now = chrono::Utc::now();
    TelemetryEnvelope::new(
        "host",
        TelemetryRecord::Span(SpanRecord {
            context: TraceContext::root(sampled),
            parent_span_id: None,
            name: SpanName::Sweep,
            started_at: now,
            ended_at: now,
            status: SpanStatus::Ok,
            attributes: Default::default(),
            events: vec![],
            links: vec![],
        }),
    )
}
#[tokio::test]
async fn trace_transport_posts_valid_spans_and_counts_filtered_spans_separately() {
    let sink = MockSink::start();
    let exporter = OtlpExporter::new(sink.base_url(), "key".into()).unwrap();
    let outcome = exporter
        .emit_batch_outcome(&[envelope(true), envelope(false)])
        .await;
    assert_eq!(outcome.acknowledged, 2);
    assert_eq!(outcome.exported, 1);
    assert_eq!(outcome.signals["spans"].accepted, 1);
    assert_eq!(outcome.signals["spans"].dropped, 1);
    let requests = sink.requests();
    assert_eq!(requests[0].0, "/v1/traces");
    let json: serde_json::Value = serde_json::from_slice(&requests[0].1).unwrap();
    assert_eq!(
        json["resourceSpans"][0]["scopeSpans"][0]["spans"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}
#[tokio::test]
async fn successful_trace_prefix_is_not_retried_after_log_failure() {
    let sink = MockSink::with_responses(vec![
        (200, "{}".into(), Duration::ZERO),
        (503, "{}".into(), Duration::ZERO),
    ]);
    let exporter = OtlpExporter::new(sink.base_url(), "key".into()).unwrap();
    let log = sweep_started_envelope();
    let outcome = exporter
        .emit_batch_outcome(&[envelope(true), log.clone()])
        .await;
    assert_eq!(outcome.acknowledged, 1);
    assert_eq!(outcome.exported, 1);
    assert!(outcome.error.is_some());
    assert_eq!(exporter.emit_batch_outcome(&[log]).await.acknowledged, 1);
    assert_eq!(
        sink.requests()
            .iter()
            .filter(|(path, _)| path == "/v1/traces")
            .count(),
        1
    );
}
#[tokio::test]
async fn native_exporter_drops_trace_records_without_sending_or_counting_as_exported() {
    use crate::observability::exporter::HttpsExporter;
    use crate::observability::HostIdStatus;
    let sink = MockSink::start();
    let exporter = HttpsExporter::new(
        sink.base_url(),
        "key".into(),
        "host".into(),
        Arc::new(HostIdStatus::default()),
    )
    .unwrap();
    let outcome = exporter.emit_batch_outcome(&[envelope(true)]).await;
    assert_eq!(outcome.acknowledged, 1);
    assert_eq!(outcome.exported, 0);
    assert_eq!(outcome.signals["spans"].dropped, 1);
    assert!(sink.requests().is_empty());
}
