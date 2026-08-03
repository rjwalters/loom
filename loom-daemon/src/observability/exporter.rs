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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::telemetry::TelemetryEnvelope;

use super::HostIdStatus;

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

/// The subset of the `/ingest` success response this exporter reads
/// (Issue #4830). Every other field is ignored, and a body that is not JSON —
/// or is JSON without a `host_id` — deserializes to `host_id: None`, which is
/// treated as "this backend does not echo an identity" and checked no further.
/// That is what keeps a pre-#4830 backend (whose response was a bare
/// `{"accepted": N}`) working unchanged.
#[derive(Deserialize)]
struct IngestAck {
    #[serde(default)]
    host_id: Option<String>,
}

/// How much of a success-response body is read before the host-identity echo
/// is parsed out of it. The real response is a few dozen bytes
/// (`{"accepted":50,"host_id":"…"}`); this bound exists only so a misconfigured
/// endpoint that answers 200 with a huge body cannot be buffered into memory
/// just to read one diagnostic field out of it.
const MAX_ACK_BODY_BYTES: usize = 4096;

/// Read at most `limit` bytes of `response`'s body, discarding the rest.
///
/// Streams chunk-by-chunk rather than using `Response::text()`, which would
/// buffer the *whole* body first and make [`MAX_ACK_BODY_BYTES`] a post-hoc
/// truncation rather than a real bound. A read error mid-body is not an error
/// here: the batch is already acked, so whatever was read so far is simply
/// parsed (and a truncated/partial body just fails to parse, which is handled
/// as "no identity to compare").
async fn read_bounded_body(mut response: reqwest::Response, limit: usize) -> String {
    let mut buf: Vec<u8> = Vec::new();
    while buf.len() < limit {
        match response.chunk().await {
            Ok(Some(chunk)) => buf.extend_from_slice(&chunk),
            Ok(None) | Err(_) => break,
        }
    }
    buf.truncate(limit);
    String::from_utf8_lossy(&buf).into_owned()
}

/// The native JSON-over-HTTPS push [`Exporter`]. POSTs each batch as a JSON
/// array to `endpoint` with `Authorization: Bearer <ingest_key>`.
pub struct HttpsExporter {
    client: reqwest::Client,
    endpoint: String,
    ingest_key: String,
    /// This daemon's own identity — [`crate::sweep_registry::host_identity`],
    /// resolved once by [`super::spawn_task`] and shared with the collector so
    /// this is literally the id stamped on the envelopes being pushed.
    host_id: String,
    /// WARN-once-per-daemon-lifetime guard (Issue #4830). The exporter is
    /// constructed once per process, so instance scope *is* lifetime scope;
    /// this is what keeps a permanent misconfiguration from re-logging on every
    /// flush for the life of the daemon.
    mismatch_warned: AtomicBool,
    /// Where a confirmed mismatch is published for `loom-daemon status` /
    /// `health` to read.
    host_id_status: Arc<HostIdStatus>,
}

/// Per-request timeout — generous enough for a slow mobile/tethered fleet
/// host, short enough that a wedged sink cannot pin the sender loop
/// indefinitely (the loop would otherwise wait a full request timeout before
/// the next scheduled flush tick anyway, but an explicit bound avoids ever
/// depending on the underlying TCP stack's own timeout behavior).
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

impl HttpsExporter {
    /// Build an exporter posting to `endpoint`, authenticating with
    /// `ingest_key`, and verifying every success response's echoed `host_id`
    /// against `host_id` (this daemon's own identity), publishing any mismatch
    /// to `host_id_status`. Fails only if the underlying `reqwest::Client`
    /// cannot be constructed (e.g. no usable TLS backend) — never touches the
    /// network.
    pub fn new(
        endpoint: String,
        ingest_key: String,
        host_id: String,
        host_id_status: Arc<HostIdStatus>,
    ) -> Result<Self, ExportError> {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|error| ExportError::Transport(error.to_string()))?;
        Ok(HttpsExporter {
            client,
            endpoint,
            ingest_key,
            host_id,
            mismatch_warned: AtomicBool::new(false),
            host_id_status,
        })
    }

    /// Compare the `host_id` a `/ingest` success response echoed against this
    /// daemon's own identity, WARNing **once per daemon lifetime** and
    /// publishing to [`HostIdStatus`] on a mismatch (Issue #4830).
    ///
    /// Never returns an error and never affects the export result: a batch the
    /// backend accepted stays accepted, whatever it says the key is bound to.
    /// The only thing a mismatch changes is that the operator now finds out.
    fn check_host_identity(&self, body: &str) {
        let Ok(ack) = serde_json::from_str::<IngestAck>(body) else {
            // Not JSON at all (a proxy's HTML 200, an empty body). The batch was
            // still acked; there is simply no identity to compare.
            return;
        };
        let Some(ingest_host_id) = ack
            .host_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            // A pre-#4830 backend, which echoes no identity. Nothing to check —
            // deliberately silent, so pointing a daemon at an older backend does
            // not produce a recurring "cannot verify" line.
            return;
        };
        if ingest_host_id == self.host_id {
            return;
        }
        // `record_mismatch` and the WARN guard are independent (the status
        // handle may be shared with a future second exporter) but both are
        // once-per-process, so the log line and the published record always
        // describe the same first observation.
        self.host_id_status
            .record_mismatch(&self.host_id, ingest_host_id);
        if !self.mismatch_warned.swap(true, Ordering::SeqCst) {
            log::warn!(
                "observability: ingest key is bound to host_id {ingest_host_id:?} but this daemon \
                 identifies as {:?} — every record pushed from this host is being filed under \
                 {ingest_host_id:?}. Install the key provisioned for {:?}, or set $LOOM_HOST_ID to \
                 match the key's binding. (This warning is logged once per daemon lifetime.)",
                self.host_id,
                self.host_id,
            );
        }
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
            // The batch is already acked at this point — reading the body is a
            // pure self-diagnostic (Issue #4830), so a body that fails to read
            // is ignored rather than turned into a spurious export failure that
            // would re-send records the backend has already durably stored.
            let body = read_bounded_body(response, MAX_ACK_BODY_BYTES).await;
            self.check_host_identity(&body);
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
        /// The success-response body (Issue #4830 — the real `/ingest` echoes
        /// `{"accepted":N,"host_id":"…"}`). Empty by default so the pre-#4830
        /// "no echo" backend is what the untouched tests below exercise.
        respond_body: Arc<Mutex<String>>,
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
            let respond_body = Arc::new(Mutex::new(String::new()));
            let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let handle = spawn_accept_loop(
                listener,
                requests.clone(),
                respond_with.clone(),
                respond_body.clone(),
                shutdown.clone(),
            );
            MockSink {
                addr,
                requests,
                respond_with,
                respond_body,
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

        /// Answer every subsequent request with this body (Issue #4830).
        fn set_body(&self, body: &str) {
            *self.respond_body.lock().unwrap() = body.to_string();
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
            let respond_body = Arc::new(Mutex::new(String::new()));
            let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let handle = spawn_accept_loop(
                listener,
                requests.clone(),
                respond_with.clone(),
                respond_body.clone(),
                shutdown.clone(),
            );
            MockSink {
                addr,
                requests,
                respond_with,
                respond_body,
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
        respond_body: Arc<Mutex<String>>,
        shutdown: Arc<std::sync::atomic::AtomicBool>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            while !shutdown.load(std::sync::atomic::Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).ok();
                        if let Some(request) = read_request(&mut stream) {
                            let status = *respond_with.lock().unwrap();
                            let body = respond_body.lock().unwrap().clone();
                            let _ = write_response(&mut stream, status, &body);
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

    fn write_response(
        stream: &mut std::net::TcpStream,
        status: u16,
        body: &str,
    ) -> std::io::Result<()> {
        let reason = if status < 300 { "OK" } else { "Error" };
        let response = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes())
    }

    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    /// The identity these tests' daemon reports for itself — matched against
    /// the `host_id` a mock sink echoes.
    const TEST_HOST_ID: &str = "host-test";

    /// An exporter with a throwaway (unregistered) status handle: nothing here
    /// touches the process-global, so tests never leak mismatch state into each
    /// other or into `ipc::build_daemon_status`.
    fn test_exporter(endpoint: String, key: &str) -> HttpsExporter {
        HttpsExporter::new(
            endpoint,
            key.to_string(),
            TEST_HOST_ID.to_string(),
            Arc::new(HostIdStatus::default()),
        )
        .unwrap()
    }

    fn test_exporter_with_status(
        endpoint: String,
        host_id: &str,
        status: Arc<HostIdStatus>,
    ) -> HttpsExporter {
        HttpsExporter::new(endpoint, "key".to_string(), host_id.to_string(), status).unwrap()
    }

    fn one_envelope() -> TelemetryEnvelope {
        TelemetryEnvelope::new(
            "host-test",
            TelemetryRecord::HostHealth(HostHealthRecord {
                captured_at: chrono::Utc::now(),
                daemon_version: "0.16.0".to_string(),
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
                roles: crate::telemetry::RoleTickHealth::default(),
            }),
        )
    }

    #[tokio::test]
    async fn emit_batch_posts_the_batch_with_bearer_auth() {
        let sink = MockSink::start();
        let exporter = test_exporter(sink.url(), "s3cr3t-ingest-key");
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
        let exporter = test_exporter(sink.url(), "key");
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
        let exporter = test_exporter(format!("http://{addr}/ingest"), "key");
        let batch = vec![one_envelope()];
        let error = exporter.emit_batch(&batch).await.unwrap_err();
        assert!(matches!(error, ExportError::Transport(_)));
    }

    #[tokio::test]
    async fn export_error_display_never_includes_the_ingest_key() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let exporter =
            test_exporter(format!("http://{addr}/ingest"), "top-secret-ingest-key-value");
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
        let exporter = test_exporter(sink.url(), "key");
        exporter.emit_batch(&[one_envelope()]).await.unwrap();
        let (addr, requests) = sink.kill();
        assert!(test_exporter(format!("http://{addr}/ingest"), "key")
            .emit_batch(&[one_envelope()])
            .await
            .is_err());
        let revived = MockSink::revive(addr, requests);
        let exporter2 = test_exporter(revived.url(), "key");
        exporter2.emit_batch(&[one_envelope()]).await.unwrap();
        assert_eq!(revived.requests().len(), 2);
    }

    // ------------------------------------------------------------------
    // host_id echo verification (Issue #4830)
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn matching_echoed_host_id_publishes_no_mismatch() {
        let sink = MockSink::start();
        sink.set_body(r#"{"accepted":1,"host_id":"host-test"}"#);
        let status = Arc::new(HostIdStatus::default());
        let exporter = test_exporter_with_status(sink.url(), TEST_HOST_ID, status.clone());

        exporter.emit_batch(&[one_envelope()]).await.unwrap();

        assert!(status.snapshot().is_none(), "ids agree ⇒ nothing published, nothing warned");
    }

    #[tokio::test]
    async fn mismatched_echoed_host_id_is_published_with_both_identities() {
        // The live 2026-07-31 incident: the Studio's key was bound to
        // `robb-pro`, so every record it pushed was filed under that host.
        let sink = MockSink::start();
        sink.set_body(r#"{"accepted":1,"host_id":"robb-pro"}"#);
        let status = Arc::new(HostIdStatus::default());
        let exporter = test_exporter_with_status(sink.url(), "robb-studio", status.clone());

        exporter.emit_batch(&[one_envelope()]).await.unwrap();

        let mismatch = status.snapshot().expect("mismatch must be published");
        assert_eq!(mismatch.daemon_host_id, "robb-studio");
        assert_eq!(mismatch.ingest_host_id, "robb-pro");
    }

    #[tokio::test]
    async fn a_mismatch_warns_once_per_lifetime_not_once_per_flush() {
        let sink = MockSink::start();
        sink.set_body(r#"{"accepted":1,"host_id":"robb-pro"}"#);
        let status = Arc::new(HostIdStatus::default());
        let exporter = test_exporter_with_status(sink.url(), "robb-studio", status.clone());

        // Ten flushes, one warning: `mismatch_warned` latches on the first, and
        // `record_mismatch` returns `false` for every subsequent flush — which
        // is exactly the "not per flush" guarantee, observed through the same
        // once-only guard the WARN is gated on.
        for _ in 0..10 {
            exporter.emit_batch(&[one_envelope()]).await.unwrap();
        }
        assert_eq!(sink.requests().len(), 10, "all ten batches were pushed");

        let first_seen = status.snapshot().expect("published").first_seen_at;
        assert!(
            !status.record_mismatch("robb-studio", "robb-pro"),
            "the exporter already consumed the one-shot; a later caller gets false"
        );
        assert_eq!(
            status.snapshot().expect("published").first_seen_at,
            first_seen,
            "first_seen_at is the age of the condition, never re-stamped per flush"
        );
    }

    #[tokio::test]
    async fn a_backend_that_echoes_no_host_id_is_not_a_mismatch() {
        // A pre-#4830 backend answers `{"accepted":N}` — unverifiable, not
        // wrong. Must stay silent rather than cry wolf on every flush.
        let sink = MockSink::start();
        sink.set_body(r#"{"accepted":1}"#);
        let status = Arc::new(HostIdStatus::default());
        let exporter = test_exporter_with_status(sink.url(), "robb-studio", status.clone());

        exporter.emit_batch(&[one_envelope()]).await.unwrap();

        assert!(status.snapshot().is_none());
    }

    #[tokio::test]
    async fn a_non_json_success_body_never_fails_the_export() {
        // A proxy answering 200 with HTML must not turn an acked batch into a
        // retry (which would duplicate rows the backend already stored).
        let sink = MockSink::start();
        sink.set_body("<html>hello</html>");
        let status = Arc::new(HostIdStatus::default());
        let exporter = test_exporter_with_status(sink.url(), "robb-studio", status.clone());

        exporter.emit_batch(&[one_envelope()]).await.unwrap();

        assert!(status.snapshot().is_none());
    }

    #[tokio::test]
    async fn an_oversized_success_body_is_bounded_and_never_fails_the_export() {
        // The identity echo is worth a few dozen bytes, not an unbounded read:
        // past MAX_ACK_BODY_BYTES the body is cut off, the truncated JSON fails
        // to parse, and the whole thing degrades to "nothing to compare".
        let sink = MockSink::start();
        let padding = "x".repeat(2 * MAX_ACK_BODY_BYTES);
        sink.set_body(&format!(r#"{{"accepted":1,"host_id":"robb-pro","pad":"{padding}"}}"#));
        let status = Arc::new(HostIdStatus::default());
        let exporter = test_exporter_with_status(sink.url(), "robb-studio", status.clone());

        exporter.emit_batch(&[one_envelope()]).await.unwrap();

        assert!(status.snapshot().is_none());
    }

    #[test]
    fn host_id_status_defaults_to_no_mismatch_and_keeps_the_first_one() {
        let status = HostIdStatus::default();
        assert!(status.snapshot().is_none(), "a disabled exporter publishes nothing");
        assert!(status.record_mismatch("a", "b"), "first write wins");
        assert!(!status.record_mismatch("a", "c"), "second write is refused");
        let snapshot = status.snapshot().unwrap();
        assert_eq!(snapshot.ingest_host_id, "b", "the first observation is kept");
    }
}
