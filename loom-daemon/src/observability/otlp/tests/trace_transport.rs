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

#[tokio::test]
#[serial_test::serial]
async fn shutdown_does_not_repost_partial_logs_while_metrics_are_in_flight() {
    use crate::observability::{queue::DurableQueue, sender, shutdown, ExportStatus};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    // The sink accepts later requests while withholding the metrics response.
    // A serial mock blocked inside metrics would hide an erroneous log replay.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let stop = Arc::new(AtomicBool::new(false));
    let log_requests = Arc::new(AtomicUsize::new(0));
    let entered_metrics = Arc::new(tokio::sync::Notify::new());
    let server = {
        let stop = stop.clone();
        let logs = log_requests.clone();
        let metrics = entered_metrics.clone();
        std::thread::spawn(move || {
            let mut held_metrics = Vec::new();
            while !stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_read_timeout(Some(Duration::from_secs(5)))
                            .unwrap();
                        let Some((path, _)) = read_request(&mut stream) else {
                            continue;
                        };
                        if path == "/v1/metrics" {
                            held_metrics.push(stream);
                            metrics.notify_one();
                        } else {
                            logs.fetch_add(1, Ordering::SeqCst);
                            let body = r#"{"partialSuccess":{"rejectedLogRecords":"1"}}"#;
                            let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                            let _ = stream.write_all(response.as_bytes());
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1))
                    }
                    Err(_) => break,
                }
            }
        })
    };
    struct ServerGuard(Arc<AtomicBool>, Option<std::thread::JoinHandle<()>>);
    impl Drop for ServerGuard {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
            if let Some(thread) = self.1.take() {
                let _ = thread.join();
            }
        }
    }
    let _server = ServerGuard(stop, Some(server));
    let exporter = OtlpExporter::new(endpoint.clone(), "key".into()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let queue = Arc::new(DurableQueue::open(dir.path().join("queue.jsonl"), 10));
    queue.push(sweep_started_envelope());
    queue.push(sweep_started_envelope());
    queue.push(host_health_envelope());
    let task = sender::spawn_task(
        queue.clone(),
        exporter,
        10,
        Duration::from_millis(1),
        Arc::new(ExportStatus::started("host", &endpoint, "otlp", 1)),
    );
    tokio::time::timeout(Duration::from_secs(10), entered_metrics.notified())
        .await
        .unwrap();
    shutdown::flush_before_shutdown(Duration::from_millis(100)).await;
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(log_requests.load(Ordering::SeqCst), 1);
    assert_eq!(queue.len(), 3, "in-flight uncommitted outcome survives shutdown timeout");
}
