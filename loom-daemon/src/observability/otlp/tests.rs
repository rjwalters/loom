use super::*;
use crate::observability::test_sink::{HttpSink, Reply};
use crate::telemetry::{HostHealthRecord, SweepStartedRecord, TelemetryRecord};
use std::io::Write;
use std::net::TcpListener;

/// Constructors for the sink these tests post to: it records every request's
/// path + body so tests can assert `OtlpExporter` posts to `/v1/logs` and
/// `/v1/metrics` separately, and can script per-request status/body/delay.
/// Every request must carry the OTLP JSON content type and a Bearer
/// `Authorization` header, or the sink's thread panics and records nothing.
struct MockSink;

impl MockSink {
    fn start() -> HttpSink {
        Self::with_responses(Vec::new())
    }

    fn with_responses(responses: Vec<Reply>) -> HttpSink {
        Self::with_content_type(responses, "application/json")
    }

    fn with_content_type(responses: Vec<Reply>, content_type: impl Into<String>) -> HttpSink {
        HttpSink::scripted(responses, &content_type.into(), (200, "{}".to_string()), |request| {
            assert_eq!(request.values("content-type"), vec!["application/json"]);
            assert!(request
                .values("authorization")
                .iter()
                .any(|v| v.to_ascii_lowercase().starts_with("bearer ")));
        })
    }
}

fn sweep_started_envelope() -> TelemetryEnvelope {
    TelemetryEnvelope::new(
        "host-otlp-test",
        TelemetryRecord::SweepStarted(SweepStartedRecord {
            story_points: None,
            repo: "rjwalters/loom".to_string(),
            visibility: crate::telemetry::RepoVisibility::Public,
            issue: 4858,
            sweep_id: "sweep-issue-4858-0".to_string(),
            started_at: chrono::Utc::now(),
            facts: Default::default(),
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
            managed_repos: Vec::new(),
            roles: crate::telemetry::RoleTickHealth::default(),
            protection: None,
            is_captain: None,
            armed_singleton_jobs: Vec::new(),
            captainless_singleton_jobs: Vec::new(),
            exported_kinds: Vec::new(),
            exporters: Vec::new(),
            otlp_export: None,
            memory: None,
            agent_scopes: Vec::new(),
            agent_scopes_total: None,
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
    let paths: Vec<&str> = requests
        .iter()
        .map(|request| request.path.as_str())
        .collect();
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
    assert_eq!(requests[0].path, "/v1/logs");
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
    // covers the transport error path (rejected-status is covered at the
    // `post` granularity by the tests below).
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
    // The partially rejected log record is a permanent, counted loss in
    // signal units (Issue #11527); the 503-held metrics batch is not.
    assert_eq!(
        queue.dropped_by_reason(),
        std::collections::BTreeMap::from([("receiver_rejected".to_string(), 1)])
    );
    // Restart recovery sees only the unacknowledged metric envelope.
    let reopened = DurableQueue::open(dir.path().join("q.jsonl"), 10);
    assert_eq!(reopened.len(), 1);
    assert_eq!(try_flush(&reopened, &exporter, 10, &status).await, FlushOutcome::Sent(1));
    assert_eq!(
        sink.requests()
            .iter()
            .map(|r| r.path.as_str())
            .collect::<Vec<_>>(),
        vec!["/v1/logs", "/v1/metrics", "/v1/metrics"]
    );
}

#[tokio::test]
async fn http_401_403_hold_the_batch() {
    for status_code in [401, 403] {
        let sink =
            MockSink::with_responses(vec![(status_code, "secret-key".to_string(), Duration::ZERO)]);
        let exporter = OtlpExporter::new(sink.base_url(), "secret-key".to_string()).unwrap();
        let result = exporter
            .emit_batch_outcome(&[sweep_started_envelope()])
            .await;
        // A credential refusal is a hold, not a drop (Issue #11527): the
        // batch stays queued (retry), nothing is lost, and the detail names
        // the credential so an operator knows what to fix.
        assert_eq!(result.acknowledged, 0, "nothing acked under {status_code}");
        assert_eq!(result.signals["log_records"].retry_scheduled, 1);
        assert!(result.lost.is_empty(), "a held batch loses nothing");
        let detail = result.error.unwrap().to_string();
        assert!(detail.contains("credential refused"), "{detail}");
        assert!(detail.contains(&status_code.to_string()), "{detail}");
        assert!(!detail.contains("secret-key"));
    }
}

#[tokio::test]
async fn http_policy_drops_and_counts_poison_batches() {
    for status_code in [400, 404, 413, 500] {
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
        // The refused logs group (the mock answers only the first request
        // with the status; metrics falls through to the default 200) counts
        // its envelope under `status_<code>` in the durable loss ledger's
        // feed (Issue #11527).
        assert_eq!(
            result.lost,
            std::collections::BTreeMap::from([(format!("status_{status_code}"), 1)])
        );
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
        assert!(result.lost.is_empty(), "a retried batch loses nothing");
    }
}

#[tokio::test]
async fn credential_refusal_holds_then_recovers_in_fifo_order() {
    use crate::observability::{
        queue::DurableQueue, sender::try_flush, sender::FlushOutcome, ExportStatus,
    };
    // Two credential refusals, then the default 200: the sink's key was
    // fixed between flushes without restarting the daemon.
    let sink = MockSink::with_responses(vec![
        (403, "secret-key".to_string(), Duration::ZERO),
        (403, "secret-key".to_string(), Duration::ZERO),
    ]);
    let exporter = OtlpExporter::new(sink.base_url(), "secret-key".to_string()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let queue = DurableQueue::open(dir.path().join("q.jsonl"), 100);
    let status = ExportStatus::started("host", &sink.base_url(), "otlp", 30);
    let first = sweep_started_issue(1);
    let second = sweep_started_issue(2);
    let third = sweep_started_issue(3);
    queue.push(first.clone());
    queue.push(second.clone());
    assert_eq!(try_flush(&queue, &exporter, 10, &status).await, FlushOutcome::Failed);
    assert_eq!(queue.len(), 2, "the held batch stays queued");
    queue.push(third.clone());
    assert_eq!(try_flush(&queue, &exporter, 10, &status).await, FlushOutcome::Failed);
    assert_eq!(queue.len(), 3, "depth keeps growing under a held 403");
    assert_eq!(queue.dropped_total(), 0, "a held batch is never a loss");
    assert!(queue.dropped_by_reason().is_empty());
    // The credential is fixed: everything held arrives, oldest first.
    assert_eq!(try_flush(&queue, &exporter, 10, &status).await, FlushOutcome::Sent(3));
    assert!(queue.is_empty());
    assert_eq!(queue.dropped_total(), 0);
    let delivered = sink.requests();
    assert_eq!(delivered.len(), 3, "one logs POST per attempt");
    let last = &delivered[2].body;
    let first_at = find_substring(last, &sweep_id_of(&first)).unwrap_or(usize::MAX);
    let second_at = find_substring(last, &sweep_id_of(&second)).unwrap_or(usize::MAX);
    let third_at = find_substring(last, &sweep_id_of(&third)).unwrap_or(usize::MAX);
    assert!(
        first_at < second_at && second_at < third_at,
        "held records are delivered in FIFO order"
    );
}

/// A `sweep.started` envelope whose `sweep_id` encodes `issue`, so a delivery
/// test can assert FIFO order from the request body alone.
fn sweep_started_issue(issue: u32) -> TelemetryEnvelope {
    TelemetryEnvelope::new(
        "host-otlp-test",
        TelemetryRecord::SweepStarted(SweepStartedRecord {
            story_points: None,
            repo: "rjwalters/loom".to_string(),
            visibility: crate::telemetry::RepoVisibility::Public,
            issue,
            sweep_id: format!("sweep-issue-{issue}-0"),
            started_at: chrono::Utc::now(),
            facts: Default::default(),
        }),
    )
}

fn sweep_id_of(envelope: &TelemetryEnvelope) -> String {
    match &envelope.record {
        TelemetryRecord::SweepStarted(record) => record.sweep_id.clone(),
        other => panic!("unexpected record {other:?}"),
    }
}

fn find_substring(haystack: &[u8], needle: &str) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle.as_bytes())
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
        assert!(!result.lost.is_empty(), "a permanent drop always has a reason");
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
    assert!(
        result.lost.is_empty(),
        "kinds OTLP deliberately does not send are routing, not loss"
    );
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
mod headers_file;
mod trace_transport;
