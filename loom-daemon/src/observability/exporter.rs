//! Exporter trait + native JSON-over-HTTPS push implementation (Epic #4702,
//! Phase 1 — issue #4705).
//!
//! [`Exporter`] is deliberately narrow — "emit a batch, optionally flush" —
//! so a later OTLP exporter (Phase 4 of the epic) is a drop-in second
//! implementation rather than a rewrite of [`super::sender`]'s drain loop,
//! which is generic over `E: Exporter` and never sees a concrete transport.
//! **No OTel dependency is added in this issue** — [`HttpsExporter`] is the
//! only implementation, and it depends on nothing but `reqwest`.
//!
//! Native `async fn` in a trait (stable since Rust 1.75) is used instead of
//! the `async-trait` crate — nothing here needs a `dyn Exporter` trait
//! object; [`super::sender::spawn_task`] is generic over a single concrete
//! `E: Exporter`, so no vtable/boxing is required.

use serde::Serialize;

use crate::telemetry::TelemetryEnvelope;

/// A sink [`super::sender`]'s drain loop can push batches of
/// [`TelemetryEnvelope`]s to. Implementations must never panic on a
/// transport failure — they return [`ExportError`] and the caller retries.
pub trait Exporter: Send + Sync {
    /// Push `envelopes` to the sink. `envelopes` is never empty (the caller
    /// only invokes this with a non-empty batch).
    fn emit_batch(
        &self,
        envelopes: &[TelemetryEnvelope],
    ) -> impl std::future::Future<Output = Result<(), ExportError>> + Send;

    /// Best-effort flush of any transport-level buffering. The default no-op
    /// is correct for [`HttpsExporter`] (every [`Exporter::emit_batch`] call
    /// is already a complete, synchronous-from-the-caller's-perspective HTTP
    /// request/response round trip) and for any sink with no internal
    /// buffer.
    fn flush(&self) -> impl std::future::Future<Output = Result<(), ExportError>> + Send {
        async { Ok(()) }
    }
}

/// An export failure. [`std::fmt::Display`] **never** includes the ingest
/// key — [`HttpsExporter`] sends it only as an `Authorization: Bearer` header
/// value, which `reqwest::Error`'s `Display` does not print (it surfaces the
/// request URL and failure kind, not headers), and this type's own variants
/// carry no credential material either. This is the AC's "ingest key …
/// absent from logs and error messages" guarantee.
#[derive(Debug)]
pub enum ExportError {
    /// The HTTP request itself failed (connect refused, DNS, TLS, timeout).
    Transport(String),
    /// The sink responded, but with a non-2xx status.
    Rejected { status: u16, body_snippet: String },
}

impl std::fmt::Display for ExportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExportError::Transport(message) => write!(f, "transport error: {message}"),
            ExportError::Rejected {
                status,
                body_snippet,
            } => {
                write!(f, "sink rejected batch: HTTP {status} — {body_snippet}")
            }
        }
    }
}

impl std::error::Error for ExportError {}

/// Request body: a batch push is a bare JSON array of envelopes — no
/// wrapping object, so the Phase-2 backend's ingest endpoint can decode the
/// request body directly as `TelemetryEnvelope[]`.
#[derive(Serialize)]
struct BatchPayload<'a>(&'a [TelemetryEnvelope]);

/// The native JSON-over-HTTPS push [`Exporter`]. POSTs each batch as a JSON
/// array to `endpoint` with `Authorization: Bearer <ingest_key>`.
pub struct HttpsExporter {
    client: reqwest::Client,
    endpoint: String,
    ingest_key: String,
}

/// Per-request timeout — generous enough for a slow mobile/tethered fleet
/// host, short enough that a wedged sink cannot pin the sender loop
/// indefinitely (the loop would otherwise wait a full request timeout before
/// the next scheduled flush tick anyway, but an explicit bound avoids ever
/// depending on the underlying TCP stack's own timeout behavior).
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

impl HttpsExporter {
    /// Build an exporter posting to `endpoint`, authenticating with
    /// `ingest_key`. Fails only if the underlying `reqwest::Client` cannot be
    /// constructed (e.g. no usable TLS backend) — never touches the network.
    pub fn new(endpoint: String, ingest_key: String) -> Result<Self, ExportError> {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|error| ExportError::Transport(error.to_string()))?;
        Ok(HttpsExporter {
            client,
            endpoint,
            ingest_key,
        })
    }
}

impl Exporter for HttpsExporter {
    async fn emit_batch(&self, envelopes: &[TelemetryEnvelope]) -> Result<(), ExportError> {
        let response = self
            .client
            .post(&self.endpoint)
            .bearer_auth(&self.ingest_key)
            .json(&BatchPayload(envelopes))
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::telemetry::{HostHealthRecord, TelemetryRecord};
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener};
    use std::sync::{Arc, Mutex};

    /// A tiny hand-rolled HTTP/1.1 mock sink: no framework dependency, just
    /// enough parsing to read one request's headers + body off a
    /// `std::net::TcpListener` connection and answer with a fixed status.
    /// Runs on a blocking thread (the daemon's tests are not all async), so
    /// [`HttpsExporter`] — an async `reqwest::Client` — talks to it over a
    /// real loopback socket exactly as it would talk to a real sink.
    struct MockSink {
        addr: SocketAddr,
        requests: Arc<Mutex<Vec<MockRequest>>>,
        respond_with: Arc<Mutex<u16>>,
        shutdown: Arc<std::sync::atomic::AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    #[derive(Debug, Clone)]
    struct MockRequest {
        auth_header: Option<String>,
        body: Vec<u8>,
    }

    impl MockSink {
        fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let addr = listener.local_addr().unwrap();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let respond_with = Arc::new(Mutex::new(200u16));
            let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let handle = spawn_accept_loop(
                listener,
                requests.clone(),
                respond_with.clone(),
                shutdown.clone(),
            );
            MockSink {
                addr,
                requests,
                respond_with,
                shutdown,
                handle: Some(handle),
            }
        }

        fn url(&self) -> String {
            format!("http://{}/ingest", self.addr)
        }

        fn requests(&self) -> Vec<MockRequest> {
            self.requests.lock().unwrap().clone()
        }

        fn set_status(&self, status: u16) {
            *self.respond_with.lock().unwrap() = status;
        }

        /// Simulate the sink going offline: stop accepting new connections.
        /// Existing in-flight requests are unaffected; a *new* POST from the
        /// exporter will hit connection-refused once the listener itself is
        /// dropped in [`Self::kill`].
        fn kill(mut self) -> (SocketAddr, Arc<Mutex<Vec<MockRequest>>>) {
            self.shutdown
                .store(true, std::sync::atomic::Ordering::SeqCst);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
            // `requests` is cloned (both point at the same underlying data —
            // it is an `Arc`) rather than moved, because `Self` implements
            // `Drop` and cannot be partially moved out of.
            (self.addr, self.requests.clone())
        }

        /// Revive a killed sink on the **same** address (rebinding a plain
        /// TCP listening socket with no established connections is safe
        /// immediately after close — no TIME_WAIT is incurred on a
        /// never-accepted listener).
        fn revive(addr: SocketAddr, requests: Arc<Mutex<Vec<MockRequest>>>) -> Self {
            let listener = TcpListener::bind(addr).unwrap();
            listener.set_nonblocking(true).unwrap();
            let respond_with = Arc::new(Mutex::new(200u16));
            let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let handle = spawn_accept_loop(
                listener,
                requests.clone(),
                respond_with.clone(),
                shutdown.clone(),
            );
            MockSink {
                addr,
                requests,
                respond_with,
                shutdown,
                handle: Some(handle),
            }
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

    fn spawn_accept_loop(
        listener: TcpListener,
        requests: Arc<Mutex<Vec<MockRequest>>>,
        respond_with: Arc<Mutex<u16>>,
        shutdown: Arc<std::sync::atomic::AtomicBool>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            while !shutdown.load(std::sync::atomic::Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).ok();
                        if let Some(request) = read_request(&mut stream) {
                            let status = *respond_with.lock().unwrap();
                            let _ = write_response(&mut stream, status);
                            requests.lock().unwrap().push(request);
                        }
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        })
    }

    fn read_request(stream: &mut std::net::TcpStream) -> Option<MockRequest> {
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
            if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
                header_end = pos + 4;
                break;
            }
        }
        let header_text = String::from_utf8_lossy(&buf[..header_end]).to_string();
        let auth_header = header_text
            .lines()
            .find(|line| line.to_ascii_lowercase().starts_with("authorization:"))
            .map(|line| {
                line.split_once(':')
                    .map(|x| x.1)
                    .unwrap_or("")
                    .trim()
                    .to_string()
            });
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
        Some(MockRequest { auth_header, body })
    }

    fn write_response(stream: &mut std::net::TcpStream, status: u16) -> std::io::Result<()> {
        let reason = if status < 300 { "OK" } else { "Error" };
        let response =
            format!("HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        stream.write_all(response.as_bytes())
    }

    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    fn one_envelope() -> TelemetryEnvelope {
        TelemetryEnvelope::new(
            "host-test",
            TelemetryRecord::HostHealth(HostHealthRecord {
                captured_at: chrono::Utc::now(),
                daemon_version: "0.16.0".to_string(),
                uptime_sec: 10,
                logical_cpus: 8,
                cpu_idle_fraction: None,
                load_per_core: None,
                worktree_root_free_gb: None,
            }),
        )
    }

    #[tokio::test]
    async fn emit_batch_posts_the_batch_with_bearer_auth() {
        let sink = MockSink::start();
        let exporter = HttpsExporter::new(sink.url(), "s3cr3t-ingest-key".to_string()).unwrap();
        let batch = vec![one_envelope(), one_envelope()];
        exporter.emit_batch(&batch).await.unwrap();

        let requests = sink.requests();
        assert_eq!(requests.len(), 1, "one batch ⇒ one HTTP request");
        assert_eq!(requests[0].auth_header.as_deref(), Some("Bearer s3cr3t-ingest-key"));
        let decoded: Vec<TelemetryEnvelope> = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(decoded.len(), 2);
    }

    #[tokio::test]
    async fn non_2xx_status_is_reported_as_rejected() {
        let sink = MockSink::start();
        sink.set_status(500);
        let exporter = HttpsExporter::new(sink.url(), "key".to_string()).unwrap();
        let batch = vec![one_envelope()];
        let error = exporter.emit_batch(&batch).await.unwrap_err();
        match error {
            ExportError::Rejected { status, .. } => assert_eq!(status, 500),
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unreachable_sink_is_a_transport_error_never_a_panic() {
        // Bind-then-drop to get a loopback port nothing is listening on.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let exporter =
            HttpsExporter::new(format!("http://{addr}/ingest"), "key".to_string()).unwrap();
        let batch = vec![one_envelope()];
        let error = exporter.emit_batch(&batch).await.unwrap_err();
        assert!(matches!(error, ExportError::Transport(_)));
    }

    #[tokio::test]
    async fn export_error_display_never_includes_the_ingest_key() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let exporter = HttpsExporter::new(
            format!("http://{addr}/ingest"),
            "top-secret-ingest-key-value".to_string(),
        )
        .unwrap();
        let batch = vec![one_envelope()];
        let error = exporter.emit_batch(&batch).await.unwrap_err();
        let rendered = error.to_string();
        assert!(
            !rendered.contains("top-secret-ingest-key-value"),
            "ExportError::Display leaked the ingest key: {rendered}"
        );
    }

    #[tokio::test]
    async fn kill_and_revive_round_trip_still_talks_to_the_same_exporter() {
        // Exercises the mock sink helper itself (the drain-loop-level
        // kill/revive integration test lives in `super::super::sender`).
        let sink = MockSink::start();
        let exporter = HttpsExporter::new(sink.url(), "key".to_string()).unwrap();
        exporter.emit_batch(&[one_envelope()]).await.unwrap();
        let (addr, requests) = sink.kill();
        assert!(HttpsExporter::new(format!("http://{addr}/ingest"), "key".to_string())
            .unwrap()
            .emit_batch(&[one_envelope()])
            .await
            .is_err());
        let revived = MockSink::revive(addr, requests);
        let exporter2 = HttpsExporter::new(revived.url(), "key".to_string()).unwrap();
        exporter2.emit_batch(&[one_envelope()]).await.unwrap();
        assert_eq!(revived.requests().len(), 2);
    }
}
