use super::*;
use crate::telemetry::{HostHealthRecord, SweepStartedRecord, TelemetryRecord};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::sync::{Arc, Mutex};

/// Every request the mock sink saw, as `(path, body)` — shared between the
/// accept-loop thread and the asserting test. Aliased rather than written
/// inline so `clippy::type_complexity` stays satisfied under
/// `--all-targets --features otlp`.
type RecordedRequests = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

/// A minimal two-path mock sink: records every request's path + body so
/// tests can assert `OtlpExporter` posts to `/v1/logs` and `/v1/metrics`
/// separately. Deliberately smaller than `exporter::tests::MockSink`
/// (single status code, no kill/revive) — the retry/backoff behavior is
/// already covered end-to-end for any `Exporter` by `sender.rs`'s tests.
struct MockSink {
    addr: SocketAddr,
    requests: RecordedRequests,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl MockSink {
    fn start() -> Self {
        Self::with_responses(Vec::new())
    }

    fn with_responses(responses: Vec<(u16, String, Duration)>) -> Self {
        Self::with_content_type(responses, "application/json")
    }

    fn with_content_type(
        responses: Vec<(u16, String, Duration)>,
        content_type: impl Into<String>,
    ) -> Self {
        let content_type = content_type.into();
        let mut responses = std::collections::VecDeque::from(responses);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handle = {
            let requests = requests.clone();
            let shutdown = shutdown.clone();
            std::thread::spawn(move || {
                while !shutdown.load(std::sync::atomic::Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            stream.set_nonblocking(false).ok();
                            if let Some((path, body)) = read_request(&mut stream) {
                                // Record the request *before* writing the
                                // response — same ordering, same reason, as
                                // `exporter::tests::MockSink` (#7256).
                                // `emit_batch(...).await` on the client side
                                // unblocks as soon as the response bytes are
                                // visible, which can race ahead of this
                                // thread's next line; recording first
                                // guarantees any caller that has observed the
                                // response also observes the recorded request.
                                requests.lock().unwrap().push((path, body));
                                let (status, body, delay) = responses.pop_front().unwrap_or((
                                    200,
                                    "{}".to_string(),
                                    Duration::ZERO,
                                ));
                                std::thread::sleep(delay);
                                let response = format!("HTTP/1.1 {status} response\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                                let _ = stream.write_all(response.as_bytes());
                            }
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                        Err(_) => break,
                    }
                }
            })
        };
        MockSink {
            addr,
            requests,
            shutdown,
            handle: Some(handle),
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn requests(&self) -> Vec<(String, Vec<u8>)> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for MockSink {
    fn drop(&mut self) {
        self.shutdown
            .store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn read_request(stream: &mut std::net::TcpStream) -> Option<(String, Vec<u8>)> {
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .ok();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end;
    loop {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|window| window == b"\r\n\r\n") {
            header_end = pos + 4;
            break;
        }
    }
    let header_text = String::from_utf8_lossy(&buf[..header_end]).to_string();
    assert!(header_text
        .to_ascii_lowercase()
        .contains("content-type: application/json"));
    assert!(header_text
        .to_ascii_lowercase()
        .contains("authorization: bearer "));
    let path = header_text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("")
        .to_string();
    let content_length: usize = header_text
        .lines()
        .find(|line| line.to_ascii_lowercase().starts_with("content-length:"))
        .and_then(|line| line.split_once(':').map(|x| x.1))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    while buf.len() < header_end + content_length {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let body = buf[header_end..(header_end + content_length).min(buf.len())].to_vec();
    Some((path, body))
}

fn sweep_started_envelope() -> TelemetryEnvelope {
    TelemetryEnvelope::new(
        "host-otlp-test",
        TelemetryRecord::SweepStarted(SweepStartedRecord {
            repo: "rjwalters/loom".to_string(),
            visibility: crate::telemetry::RepoVisibility::Public,
            issue: 4858,
            sweep_id: "sweep-issue-4858-0".to_string(),
            started_at: chrono::Utc::now(),
            model: None,
            effort: None,
        }),
    )
}

fn host_health_envelope() -> TelemetryEnvelope {
    TelemetryEnvelope::new(
        "host-otlp-test",
        TelemetryRecord::HostHealth(HostHealthRecord {
            captured_at: chrono::Utc::now(),
            daemon_version: "0.17.0".to_string(),
            build_commit: "deadbeef".to_string(),
            built_at: None,
            uptime_sec: 10,
            logical_cpus: 8,
            cpu_idle_fraction: None,
            load_per_core: None,
            worktree_root_free_gb: None,
            worktree_root_total_gb: None,
            active_sweep_ids: Vec::new(),
            dispatch_halted: false,
            halt_reason: None,
            managed_repos: Vec::new(),
            roles: crate::telemetry::RoleTickHealth::default(),
            protection: None,
            admission_brake: None,
        }),
    )
}

#[tokio::test]
async fn emit_batch_posts_logs_and_metrics_to_their_own_paths_with_bearer_auth() {
    let sink = MockSink::start();
    let exporter = OtlpExporter::new(sink.base_url(), "s3cr3t-ingest-key".to_string()).unwrap();
    let batch = vec![sweep_started_envelope(), host_health_envelope()];
    exporter.emit_batch(&batch).await.unwrap();

    let requests = sink.requests();
    assert_eq!(requests.len(), 2, "one lifecycle + one host-level envelope ⇒ two POSTs");
    let paths: Vec<&str> = requests.iter().map(|(path, _)| path.as_str()).collect();
    assert!(paths.contains(&"/v1/logs"));
    assert!(paths.contains(&"/v1/metrics"));
}

#[tokio::test]
async fn emit_batch_skips_the_metrics_post_for_an_all_lifecycle_batch() {
    let sink = MockSink::start();
    let exporter = OtlpExporter::new(sink.base_url(), "key".to_string()).unwrap();
    exporter
        .emit_batch(&[sweep_started_envelope()])
        .await
        .unwrap();

    let requests = sink.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].0, "/v1/logs");
}

#[test]
fn base_endpoint_trailing_slash_is_tolerated() {
    let exporter =
        OtlpExporter::new("https://collector.example.com/".to_string(), "key".to_string()).unwrap();
    assert_eq!(exporter.logs_endpoint, "https://collector.example.com/v1/logs");
    assert_eq!(exporter.metrics_endpoint, "https://collector.example.com/v1/metrics");
}

#[tokio::test]
async fn non_2xx_status_from_either_endpoint_is_a_rejected_error() {
    // Bind-then-drop to get a loopback port nothing is listening on —
    // reuses `exporter.rs`'s unreachable-sink pattern for the transport
    // error path (rejected-status is already covered by
    // `HttpsExporter`'s own tests at the `post` granularity).
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let exporter = OtlpExporter::new(format!("http://{addr}"), "key".to_string()).unwrap();
    let error = exporter
        .emit_batch(&[sweep_started_envelope()])
        .await
        .unwrap_err();
    assert!(matches!(error, ExportError::Transport(_)));
}
#[tokio::test]
async fn partial_logs_are_acknowledged_before_metrics_retry() {
    use crate::observability::{
        queue::DurableQueue,
        sender::{try_flush, FlushOutcome},
        ExportStatus,
    };
    let sink = MockSink::with_responses(vec![
        (
            200,
            r#"{"partialSuccess":{"rejectedLogRecords":"1"}}"#.to_string(),
            Duration::ZERO,
        ),
        (503, "secret-key".to_string(), Duration::ZERO),
        (200, "{}".to_string(), Duration::ZERO),
    ]);
    let exporter = OtlpExporter::new(sink.base_url(), "secret-key".to_string()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let queue = DurableQueue::open(dir.path().join("q.jsonl"), 10);
    queue.push(sweep_started_envelope());
    queue.push(sweep_started_envelope());
    queue.push(host_health_envelope());
    let status = ExportStatus::started("host", &sink.base_url(), "otlp", 30);
    assert_eq!(try_flush(&queue, &exporter, 10, &status).await, FlushOutcome::Failed);
    assert_eq!(queue.len(), 1);
    let snapshot = status.snapshot();
    assert_eq!(snapshot.signal_counts["log_records"].accepted, 1);
    assert_eq!(snapshot.signal_counts["log_records"].rejected, 1);
    assert!(snapshot.signal_counts["metric_data_points"].retry_scheduled > 0);
    assert!(!snapshot.last_failure_detail.unwrap().contains("secret-key"));
    // Restart recovery sees only the unacknowledged metric envelope.
    let reopened = DurableQueue::open(dir.path().join("q.jsonl"), 10);
    assert_eq!(reopened.len(), 1);
    assert_eq!(try_flush(&reopened, &exporter, 10, &status).await, FlushOutcome::Sent(1));
    assert_eq!(
        sink.requests()
            .iter()
            .map(|r| r.0.as_str())
            .collect::<Vec<_>>(),
        vec!["/v1/logs", "/v1/metrics", "/v1/metrics"]
    );
}

#[tokio::test]
async fn http_policy_drops_poison_batches_and_continues() {
    for status_code in [400, 401, 403, 404, 413, 500] {
        let sink =
            MockSink::with_responses(vec![(status_code, "secret-key".to_string(), Duration::ZERO)]);
        let exporter = OtlpExporter::new(sink.base_url(), "secret-key".to_string()).unwrap();
        let result = exporter
            .emit_batch_outcome(&[sweep_started_envelope(), host_health_envelope()])
            .await;
        assert_eq!(result.acknowledged, 2);
        assert_eq!(result.signals["log_records"].dropped, 1);
        assert!(result.signals["metric_data_points"].accepted > 0);
        assert!(!result.error.unwrap().to_string().contains("secret-key"));
    }
    for status_code in [429, 502, 503, 504] {
        let sink =
            MockSink::with_responses(vec![(status_code, "secret-key".to_string(), Duration::ZERO)]);
        let exporter = OtlpExporter::new(sink.base_url(), "secret-key".to_string()).unwrap();
        let result = exporter
            .emit_batch_outcome(&[sweep_started_envelope()])
            .await;
        assert_eq!(result.acknowledged, 0);
        assert_eq!(result.signals["log_records"].retry_scheduled, 1);
    }
}

#[tokio::test]
async fn malformed_and_oversized_acknowledgments_are_not_success() {
    for body in ["html not JSON".to_string(), "x".repeat(4097)] {
        let sink = MockSink::with_responses(vec![(200, body, Duration::ZERO)]);
        let exporter = OtlpExporter::new(sink.base_url(), "key".to_string()).unwrap();
        let result = exporter
            .emit_batch_outcome(&[sweep_started_envelope()])
            .await;
        assert_eq!(result.acknowledged, 1);
        assert_eq!(result.exported, 0);
        assert_eq!(result.signals["log_records"].dropped, 1);
    }
}

#[tokio::test]
async fn timeout_leaves_request_pending() {
    let sink = MockSink::with_responses(vec![(200, "{}".to_string(), Duration::from_millis(100))]);
    let mut exporter = OtlpExporter::new(sink.base_url(), "key".to_string()).unwrap();
    exporter.client = reqwest::Client::builder()
        .timeout(Duration::from_millis(20))
        .build()
        .unwrap();
    let result = exporter
        .emit_batch_outcome(&[sweep_started_envelope()])
        .await;
    assert_eq!(result.acknowledged, 0);
    assert_eq!(result.signals["log_records"].retry_scheduled, 1);
    assert!(result.error.unwrap().to_string().contains("timed out"));
}

#[tokio::test]
async fn empty_token_snapshot_advances_without_fabricating_metrics() {
    let sink = MockSink::start();
    let exporter = OtlpExporter::new(sink.base_url(), "key".to_string()).unwrap();
    let empty = TelemetryEnvelope::new(
        "host",
        TelemetryRecord::TokensSnapshot(crate::telemetry::TokenSnapshotRecord {
            captured_at: chrono::Utc::now(),
            accounts: Vec::new(),
        }),
    );
    let result = exporter
        .emit_batch_outcome(&[empty, sweep_started_envelope()])
        .await;
    assert_eq!(result.acknowledged, 2);
    assert_eq!(sink.requests().len(), 1);
}

#[test]
fn endpoint_rejects_credentials_query_and_fragment_without_echoing_them() {
    for endpoint in [
        "ftp://host",
        "https://user:secret@host",
        "https://host?key=secret",
        "https://host/#secret",
    ] {
        let error = OtlpExporter::new(endpoint.to_string(), "key".to_string())
            .err()
            .unwrap();
        assert!(!error.to_string().contains("secret"));
    }
    let exporter =
        OtlpExporter::new("http://localhost:4318/prefix/".to_string(), "key".to_string()).unwrap();
    assert_eq!(exporter.logs_endpoint, "http://localhost:4318/prefix/v1/logs");
}

#[tokio::test]
async fn response_media_type_is_required() {
    let sink = MockSink::with_content_type(Vec::new(), "text/plain");
    let exporter = OtlpExporter::new(sink.base_url(), "key".to_string()).unwrap();
    let outcome = exporter
        .emit_batch_outcome(&[sweep_started_envelope()])
        .await;
    assert_eq!(outcome.signals["log_records"].dropped, 1);
    assert_eq!(outcome.exported, 0);
    let sink = MockSink::with_content_type(Vec::new(), "application/json; charset=utf-8");
    let exporter = OtlpExporter::new(sink.base_url(), "key".to_string()).unwrap();
    assert_eq!(
        exporter
            .emit_batch_outcome(&[sweep_started_envelope()])
            .await
            .exported,
        1
    );
}

#[tokio::test]
async fn redirects_are_permanent_responses_not_hidden_reposts() {
    let target = MockSink::start();
    let sink = MockSink::with_content_type(
        vec![(307, "{}".to_string(), Duration::ZERO)],
        format!("application/json\r\nLocation: {}/v1/logs", target.base_url()),
    );
    let exporter = OtlpExporter::new(sink.base_url(), "secret".to_string()).unwrap();
    let outcome = exporter
        .emit_batch_outcome(&[sweep_started_envelope()])
        .await;
    assert_eq!(outcome.signals["log_records"].dropped, 1);
    assert_eq!(outcome.exported, 0);
    assert!(target.requests().is_empty());
}
