//! OTLP exporter (Epic #4702, Phase 4 — issue #4858): a drop-in second
//! [`super::exporter::Exporter`] implementation translating
//! [`crate::telemetry::TelemetryEnvelope`] batches into the OTLP wire format,
//! for operators with an existing OpenTelemetry stack (a self-hosted
//! collector, Grafana, Honeycomb, …) who want to skip the native Cloudflare
//! backend entirely. Selected via `observability.exporter = "otlp"`
//! (`observability::resolve_exporter`) — [`super::exporter::HttpsExporter`]
//! stays the default (`"https"`).
//!
//! Gated behind the `otlp` Cargo feature (see `loom-daemon/Cargo.toml`): a
//! default build never compiles `opentelemetry_proto` in, so choosing this
//! sink costs nothing for operators who stick with the HTTPS exporter.
//!
//! # Transport, not the OTel SDK
//!
//! This is deliberately a thin translator, not an embedding of the
//! `opentelemetry-otlp` SDK exporter: [`OtlpExporter::emit_batch`] posts the
//! mapped OTLP request(s) over the *same kind* of `reqwest` client
//! [`super::exporter::HttpsExporter`] already depends on, and
//! [`super::sender`]'s drain/retry/backoff loop — generic over any
//! `E: `[`super::exporter::Exporter`] — governs retries exactly as it does
//! for the HTTPS sink. **No OTLP-specific retry/backoff logic exists here.**
//! `opentelemetry-proto`'s `gen-tonic-messages` feature buys only the
//! generated message *types* (`prost`-derived structs); `tonic`/gRPC
//! transport is deliberately never enabled, so this feature adds no gRPC
//! stack — just JSON-serializable Rust structs for the OTLP wire shapes.
//!
//! # `TelemetryEnvelope` → OTLP mapping
//!
//! | Envelope field | OTLP destination |
//! |---|---|
//! | `host_id` | `Resource` attribute `service.instance.id` (and `host.id`) — one `ResourceLogs`/`ResourceMetrics` entry per distinct `host_id` in a batch |
//! | `emitted_at` | `LogRecord.time_unix_nano` / `.observed_time_unix_nano`, or `NumberDataPoint.time_unix_nano` |
//! | a record's repo-visibility tag (when present) | **not** a `Resource` attribute — a `Resource` describes the emitting *host*, and one host's batch can reference many repos, so visibility is a per-`LogRecord` attribute `loom.repo.visibility` (alongside `loom.repo`) instead |
//!
//! [`TelemetryRecord`](crate::telemetry::TelemetryRecord) kinds split into
//! two OTLP signals:
//!
//! - **Logs** (`ExportLogsServiceRequest`, one `LogRecord` per envelope): the
//!   four sweep-lifecycle records — `sweep.started`, `sweep.phase`,
//!   `sweep.completed`, `sweep.outcome` — become `LogRecord`s. Each record's
//!   own `kind` tag is carried as `LogRecord.event_name`, its fields flatten
//!   into `LogRecord.attributes` under a `loom.` prefix (`sweep.outcome`'s
//!   `config` map becomes `loom.config.<key>` attributes and its
//!   `phase_durations` becomes a nested `loom.phase_durations` array
//!   attribute), and `severity_number` reflects `SweepResult` where one is
//!   present (`Failure` → `Error`, `Blocked` → `Warn`, else `Info`).
//! - **Metrics** (`ExportMetricsServiceRequest`, one `Gauge` `Metric` per
//!   distinct name, one `NumberDataPoint` per envelope/account that measured
//!   it): the two host-level records — `tokens.snapshot`, `host.health` —
//!   become `Gauge` metrics (`loom.host.*`, `loom.tokens.*`). An unmeasured
//!   optional field (e.g. `cpu_idle_fraction: None`) produces **no** data
//!   point — the schema's "unknown != zero" contract carries through to the
//!   OTLP mapping. `host.health`'s `daemon_version` becomes the `Resource`
//!   attribute `service.version`, not a metric, since it describes the
//!   emitting entity rather than a measurement.
//!
//! See [`mapping`] for the field-by-field implementation and its unit tests
//! (fixture envelopes for every record kind, verifying the log/metric split
//! and every attribute above).

mod mapping;

use std::time::Duration;

use serde::Serialize;

use super::exporter::{ExportError, Exporter};
use crate::telemetry::TelemetryEnvelope;

/// Per-request timeout — same rationale and value as
/// [`super::exporter::HttpsExporter`]'s.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// The OTLP/HTTP+JSON [`Exporter`]. POSTs mapped logs/metrics batches to
/// `{base_endpoint}/v1/logs` and `{base_endpoint}/v1/metrics` respectively —
/// the standard OTLP/HTTP path suffixes — each with `Authorization: Bearer
/// <ingest_key>`, the same auth convention [`super::exporter::HttpsExporter`]
/// uses, so `observability.ingestKeyFile` is shared across both sinks.
pub struct OtlpExporter {
    client: reqwest::Client,
    logs_endpoint: String,
    metrics_endpoint: String,
    ingest_key: String,
}

impl OtlpExporter {
    /// Build an exporter posting to `{base_endpoint}/v1/logs` and
    /// `{base_endpoint}/v1/metrics` (a trailing slash on `base_endpoint` is
    /// tolerated and stripped), authenticating with `ingest_key`. Fails only
    /// if the underlying `reqwest::Client` cannot be constructed — never
    /// touches the network.
    pub fn new(base_endpoint: String, ingest_key: String) -> Result<Self, ExportError> {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|error| ExportError::Transport(error.to_string()))?;
        let base = base_endpoint.trim_end_matches('/');
        Ok(OtlpExporter {
            client,
            logs_endpoint: format!("{base}/v1/logs"),
            metrics_endpoint: format!("{base}/v1/metrics"),
            ingest_key,
        })
    }

    async fn post<T: Serialize + Sync>(&self, endpoint: &str, body: &T) -> Result<(), ExportError> {
        let response = self
            .client
            .post(endpoint)
            .bearer_auth(&self.ingest_key)
            .json(body)
            .send()
            .await
            .map_err(|error| ExportError::Transport(error.to_string()))?;
        if response.status().is_success() {
            return Ok(());
        }
        let status = response.status().as_u16();
        let body_snippet = response
            .text()
            .await
            .unwrap_or_default()
            .chars()
            .take(200)
            .collect();
        Err(ExportError::Rejected {
            status,
            body_snippet,
        })
    }
}

impl Exporter for OtlpExporter {
    /// Maps `envelopes` into an OTLP logs request and/or an OTLP metrics
    /// request (see the module docs) and POSTs whichever are non-empty —
    /// skipping either POST entirely when the batch contains no envelope of
    /// the corresponding signal.
    ///
    /// **Not atomic across the two requests**: if the logs POST succeeds but
    /// the metrics POST then fails, this returns `Err` and
    /// [`super::sender`] leaves the *whole* batch queued for retry — the
    /// already-accepted log records will be POSTed again next attempt. This
    /// is the same at-least-once tradeoff every sink in this module makes at
    /// the single-request granularity; telemetry ingestion is expected to
    /// tolerate duplicates, never data loss.
    async fn emit_batch(&self, envelopes: &[TelemetryEnvelope]) -> Result<(), ExportError> {
        if let Some(request) = mapping::build_logs_request(envelopes) {
            self.post(&self.logs_endpoint, &request).await?;
        }
        if let Some(request) = mapping::build_metrics_request(envelopes) {
            self.post(&self.metrics_endpoint, &request).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
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
                                    let _ = write_response(&mut stream);
                                    requests.lock().unwrap().push((path, body));
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

    fn write_response(stream: &mut std::net::TcpStream) -> std::io::Result<()> {
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
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
                active_sweep_ids: Vec::new(),
                dispatch_halted: false,
                halt_reason: None,
                managed_repos: Vec::new(),
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
            OtlpExporter::new("https://collector.example.com/".to_string(), "key".to_string())
                .unwrap();
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
}
