//! `headers_file` on the wire (Issue #10961): the file's headers reach all
//! three OTLP signal paths, a file-supplied `Authorization` replaces the
//! Bearer header, and no header value escapes into a log line, an error, the
//! export status or an exported record.
use super::*;
use crate::telemetry::trace::{SpanName, SpanRecord, SpanStatus, TraceContext};

/// Synthetic credentials. Distinct strings so a leak names its source.
const CLIENT_ID: &str = "fixture-client-id-51c2";
const CLIENT_SECRET: &str = "fixture-client-secret-9e7b";
const FILE_AUTHORIZATION: &str = "Basic fixture-authorization-3d1f";
const INGEST_KEY: &str = "fixture-ingest-key-a40e";

/// One request as the sink saw it: path, lower-cased header names with their
/// values, and the body.
#[derive(Clone)]
struct Captured {
    path: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Captured {
    fn values(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(header, _)| header == name)
            .map(|(_, value)| value.as_str())
            .collect()
    }
}

/// A loopback sink that records each request's headers — which
/// [`MockSink`] deliberately does not — and answers every request with one
/// fixed status and JSON body.
struct HeaderSink {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<Captured>>>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl HeaderSink {
    fn start() -> Self {
        Self::answering(200, "{}")
    }

    fn answering(status: u16, body: &str) -> Self {
        let body = body.to_string();
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
                            if let Some(captured) = capture(&mut stream) {
                                // Record before responding, as `MockSink` does.
                                requests.lock().unwrap().push(captured);
                                let response = format!(
                                    "HTTP/1.1 {status} response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                    body.len()
                                );
                                let _ = stream.write_all(response.as_bytes());
                            }
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => break,
                    }
                }
            })
        };
        HeaderSink {
            addr,
            requests,
            shutdown,
            handle: Some(handle),
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn requests(&self) -> Vec<Captured> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for HeaderSink {
    fn drop(&mut self) {
        self.shutdown
            .store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn capture(stream: &mut std::net::TcpStream) -> Option<Captured> {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|window| window == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.lines();
    let path = lines.next()?.split_whitespace().nth(1)?.to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    let content_length: usize = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse().ok())
        .unwrap_or(0);
    while buf.len() < header_end + content_length {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let body = String::from_utf8_lossy(&buf[header_end..]).to_string();
    Some(Captured {
        path,
        headers,
        body,
    })
}

/// An owner-only headers file holding `contents`.
fn headers_file(contents: &str) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("otlp-headers");
    std::fs::write(&path, contents).unwrap();
    set_mode(&path, 0o600);
    (dir, path.to_string_lossy().into_owned())
}

#[cfg(unix)]
fn set_mode(path: &std::path::Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

#[cfg(not(unix))]
fn set_mode(_: &std::path::Path, _: u32) {}

fn proxy_headers() -> String {
    format!("# identity-aware proxy\nX-Client-Id: {CLIENT_ID}\nX-Client-Secret: {CLIENT_SECRET}\n")
}

fn span_envelope() -> TelemetryEnvelope {
    let now = chrono::Utc::now();
    TelemetryEnvelope::new(
        "host-otlp-test",
        TelemetryRecord::Span(SpanRecord {
            context: TraceContext::root(true),
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

/// One envelope per OTLP signal.
fn all_signals() -> Vec<TelemetryEnvelope> {
    vec![
        span_envelope(),
        sweep_started_envelope(),
        host_health_envelope(),
    ]
}

fn exporter_for(sink: &HeaderSink, headers_file: Option<&str>) -> OtlpExporter {
    OtlpExporter::with_headers_file(sink.base_url(), Some(INGEST_KEY.to_string()), headers_file)
        .unwrap()
}

const SIGNAL_PATHS: [&str; 3] = ["/v1/traces", "/v1/logs", "/v1/metrics"];

fn assert_all_three_signals(requests: &[Captured]) {
    let mut paths: Vec<&str> = requests.iter().map(|r| r.path.as_str()).collect();
    paths.sort_unstable();
    let mut expected = SIGNAL_PATHS.to_vec();
    expected.sort_unstable();
    assert_eq!(paths, expected, "one request per signal");
}

#[tokio::test]
async fn headers_file_headers_are_sent_on_traces_logs_and_metrics() {
    let sink = HeaderSink::start();
    let (_dir, path) = headers_file(&proxy_headers());
    let exporter = exporter_for(&sink, Some(&path));
    let outcome = exporter.emit_batch_outcome(&all_signals()).await;
    assert!(outcome.error.is_none());
    assert_eq!(outcome.acknowledged, 3);

    let requests = sink.requests();
    assert_all_three_signals(&requests);
    for request in &requests {
        let path = &request.path;
        assert_eq!(request.values("x-client-id"), vec![CLIENT_ID], "{path}");
        assert_eq!(request.values("x-client-secret"), vec![CLIENT_SECRET], "{path}");
        // The file sets no Authorization, so the Bearer header is sent as
        // it always was.
        assert_eq!(
            request.values("authorization"),
            vec![format!("Bearer {INGEST_KEY}").as_str()],
            "{path}"
        );
        assert_eq!(request.values("content-type"), vec!["application/json"], "{path}");
    }
}

#[tokio::test]
async fn a_file_supplied_authorization_replaces_the_bearer_header() {
    let sink = HeaderSink::start();
    let (_dir, path) =
        headers_file(&format!("authorization: {FILE_AUTHORIZATION}\nX-Client-Id: {CLIENT_ID}\n"));
    let exporter = exporter_for(&sink, Some(&path));
    assert!(exporter
        .emit_batch_outcome(&all_signals())
        .await
        .error
        .is_none());

    let requests = sink.requests();
    assert_all_three_signals(&requests);
    for request in &requests {
        let path = &request.path;
        // Replaced, not appended: exactly one Authorization header.
        assert_eq!(request.values("authorization"), vec![FILE_AUTHORIZATION], "{path}");
        assert_eq!(request.values("x-client-id"), vec![CLIENT_ID], "{path}");
        assert!(
            request
                .headers
                .iter()
                .all(|(_, value)| !value.contains(INGEST_KEY)),
            "the ingest key must not be sent once the file owns Authorization ({path})"
        );
    }
}

/// "Existing configs behave exactly as before": no `headers_file` produces
/// the same request headers as the pre-existing constructor.
#[tokio::test]
async fn without_a_headers_file_the_request_headers_are_unchanged() {
    let mut seen = Vec::new();
    for with_none in [false, true] {
        let sink = HeaderSink::start();
        let exporter = if with_none {
            exporter_for(&sink, None)
        } else {
            OtlpExporter::new(sink.base_url(), INGEST_KEY.to_string()).unwrap()
        };
        exporter.emit_batch(&all_signals()).await.unwrap();
        let requests = sink.requests();
        assert_all_three_signals(&requests);
        for request in &requests {
            assert_eq!(
                request.values("authorization"),
                vec![format!("Bearer {INGEST_KEY}").as_str()]
            );
        }
        let mut shape: Vec<(String, Vec<String>)> = requests
            .iter()
            .map(|request| {
                let mut names: Vec<String> = request
                    .headers
                    .iter()
                    // `host` carries the sink's ephemeral port and
                    // `content-length` the timestamped body's size.
                    .filter(|(name, _)| name != "host" && name != "content-length")
                    .map(|(name, value)| format!("{name}: {value}"))
                    .collect();
                names.sort();
                (request.path.clone(), names)
            })
            .collect();
        shape.sort();
        seen.push(shape);
    }
    assert_eq!(seen[0], seen[1]);
}

#[tokio::test]
async fn rebuilding_the_exporter_re_reads_the_file() {
    let sink = HeaderSink::start();
    let (_dir, path) = headers_file("X-Token: before-rotation\n");
    exporter_for(&sink, Some(&path))
        .emit_batch(&[sweep_started_envelope()])
        .await
        .unwrap();
    std::fs::write(&path, "X-Token: after-rotation\n").unwrap();
    exporter_for(&sink, Some(&path))
        .emit_batch(&[sweep_started_envelope()])
        .await
        .unwrap();
    let tokens: Vec<String> = sink
        .requests()
        .iter()
        .map(|request| request.values("x-token").join(","))
        .collect();
    assert_eq!(tokens, vec!["before-rotation", "after-rotation"]);
}

#[test]
fn a_refused_headers_file_fails_construction_naming_file_and_line_only() {
    let (_dir, path) = headers_file(&format!("X-Client-Id: {CLIENT_ID}\n{CLIENT_SECRET}\n"));
    let error = OtlpExporter::with_headers_file(
        "https://otlp.test-fixture.internal".to_string(),
        Some(INGEST_KEY.to_string()),
        Some(&path),
    )
    .err()
    .unwrap();
    for rendered in [error.to_string(), format!("{error:?}")] {
        assert!(rendered.contains(&path), "{rendered}");
        assert!(rendered.contains("line 2:"), "{rendered}");
        for secret in [CLIENT_ID, CLIENT_SECRET, INGEST_KEY] {
            assert!(!rendered.contains(secret), "{rendered}");
        }
    }
}

#[cfg(unix)]
#[test]
fn a_group_or_world_readable_headers_file_fails_construction() {
    let (_dir, path) = headers_file(&proxy_headers());
    for mode in [0o640, 0o604, 0o644] {
        set_mode(std::path::Path::new(&path), mode);
        let error = OtlpExporter::with_headers_file(
            "https://otlp.test-fixture.internal".to_string(),
            Some(INGEST_KEY.to_string()),
            Some(&path),
        )
        .err()
        .unwrap()
        .to_string();
        assert!(error.contains(&path), "{error}");
        assert!(error.contains("group or others"), "{error}");
        assert!(!error.contains(CLIENT_SECRET), "{error}");
    }
}

#[test]
fn the_endpoint_is_validated_before_the_headers_file_is_opened() {
    // A missing file would be its own error; the endpoint's comes first.
    let error = OtlpExporter::with_headers_file(
        "ftp://otlp.test-fixture.internal".to_string(),
        Some(INGEST_KEY.to_string()),
        Some("/nonexistent/otlp-headers"),
    )
    .err()
    .unwrap()
    .to_string();
    assert!(error.contains("OTLP base URL"), "{error}");
    assert!(!error.contains("otlp-headers"), "{error}");
}

/// The acceptance criterion "no header value appears in any log line or
/// exported record", end to end: construction, a rejected export whose
/// response body echoes the credentials back, a retried one, a transport
/// failure, and the status the daemon reports afterwards.
///
/// A plain `#[test]` driving a current-thread runtime, because
/// [`crate::test_log_capture`] collects on the calling thread only.
#[test]
fn header_values_never_reach_logs_errors_status_or_exported_records() {
    use crate::observability::{queue::DurableQueue, sender::try_flush, ExportStatus};

    let secrets = [CLIENT_ID, CLIENT_SECRET, FILE_AUTHORIZATION];
    let echo = format!(
        r#"{{"error":"denied {CLIENT_ID} {CLIENT_SECRET} {FILE_AUTHORIZATION}","partialSuccess":{{"errorMessage":"{CLIENT_SECRET}"}}}}"#
    );
    let (_dir, path) = headers_file(&format!(
        "X-Client-Id: {CLIENT_ID}\nX-Client-Secret: {CLIENT_SECRET}\nAuthorization: {FILE_AUTHORIZATION}\n"
    ));
    let queue_dir = tempfile::tempdir().unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let mut rendered: Vec<String> = Vec::new();
    let mut bodies: Vec<String> = Vec::new();
    let logs = crate::test_log_capture::capture_logs(|| {
        runtime.block_on(async {
            // 200 + warning, 401 (dropped), 503 (retried): every response
            // class, each echoing the credentials.
            for status_code in [200, 401, 503] {
                let sink = HeaderSink::answering(status_code, &echo);
                let exporter = exporter_for(&sink, Some(&path));
                let outcome = exporter.emit_batch_outcome(&all_signals()).await;
                rendered.push(format!("{:?}", outcome.error));
                rendered.push(serde_json::to_string(&outcome.signals).unwrap());

                let queue =
                    DurableQueue::open(queue_dir.path().join(format!("q-{status_code}.jsonl")), 10);
                for envelope in all_signals() {
                    queue.push(envelope);
                }
                let status = ExportStatus::started("host", &sink.base_url(), "otlp", 30);
                let _ = try_flush(&queue, &exporter, 10, &status).await;
                rendered.push(serde_json::to_string(&status.snapshot()).unwrap());
                rendered.push(format!("{:?}", status.snapshot()));

                let requests = sink.requests();
                assert!(!requests.is_empty());
                // Positive control: the sink really was sent the secrets, in
                // headers — so their absence below is not vacuous.
                assert_eq!(requests[0].values("x-client-secret"), vec![CLIENT_SECRET]);
                bodies.extend(requests.into_iter().map(|request| request.body));
            }

            // Nothing listening: the transport-failure path.
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            drop(listener);
            let exporter = OtlpExporter::with_headers_file(
                format!("http://{addr}"),
                Some(INGEST_KEY.to_string()),
                Some(&path),
            )
            .unwrap();
            let outcome = exporter.emit_batch_outcome(&all_signals()).await;
            rendered.push(format!("{:?}", outcome.error.unwrap()));
            rendered.push(format!("{:?}", exporter.extra_headers));
        });
    });

    assert!(
        logs.iter()
            .any(|(_, message)| message.contains("extra request header(s)")
                && message.contains(&path)),
        "construction logs the file path and header count: {logs:?}"
    );
    for (_, message) in &logs {
        for secret in secrets {
            assert!(!message.contains(secret), "log line leaked a header value: {message}");
        }
    }
    for text in &rendered {
        for secret in secrets {
            assert!(!text.contains(secret), "error/status leaked a header value: {text}");
        }
    }
    assert!(!bodies.is_empty());
    for body in &bodies {
        for secret in secrets {
            assert!(!body.contains(secret), "an exported record carries a header value");
        }
    }
}

// ---------------------------------------------------------------------
// #11555 — `headers_file` without ingest.key, re-read on file change.
// ---------------------------------------------------------------------

/// An entry whose `headers_file` carries all its credentials exports with
/// no ingest key at all: the file's headers are sent, and no
/// `Authorization: Bearer` is added (#11555).
#[tokio::test]
async fn a_headers_file_exporter_without_an_ingest_key_sends_no_bearer() {
    let sink = HeaderSink::start();
    let (_dir, path) = headers_file(&proxy_headers());
    let exporter = OtlpExporter::with_headers_file(sink.base_url(), None, Some(&path)).unwrap();
    exporter.emit_batch(&all_signals()).await.unwrap();

    let requests = sink.requests();
    assert_all_three_signals(&requests);
    for request in &requests {
        let path = &request.path;
        assert_eq!(request.values("x-client-id"), vec![CLIENT_ID], "{path}");
        assert_eq!(request.values("x-client-secret"), vec![CLIENT_SECRET], "{path}");
        assert!(
            request.values("authorization").is_empty(),
            "no Authorization at all — not even an empty Bearer ({path})"
        );
    }
}

/// An atomically rotated headers file (write a sibling, rename it over the
/// live path) is picked up by the *same, running* exporter — no restart, no
/// rebuild (#11555). The two values are the same length, so the change is
/// caught by mtime/inode, not by a length delta.
#[tokio::test]
async fn a_rotated_headers_file_is_re_read_by_the_running_exporter() {
    let sink = HeaderSink::start();
    let (dir, path) = headers_file("X-Token: rotation-one\n");
    let exporter = exporter_for(&sink, Some(&path));
    exporter
        .emit_batch(&[sweep_started_envelope()])
        .await
        .unwrap();

    let staged = dir.path().join("otlp-headers.staged");
    std::fs::write(&staged, "X-Token: rotation-two\n").unwrap();
    set_mode(&staged, 0o600);
    std::fs::rename(&staged, &path).unwrap();

    exporter
        .emit_batch(&[sweep_started_envelope()])
        .await
        .unwrap();
    let tokens: Vec<String> = sink
        .requests()
        .iter()
        .map(|request| request.values("x-token").join(","))
        .collect();
    assert_eq!(
        tokens,
        vec!["rotation-one", "rotation-two"],
        "the second request must carry the rotated value"
    );
}

/// The last request's `X-Client-Secret`, if any.
fn last_secret(sink: &HeaderSink) -> String {
    sink.requests()
        .last()
        .and_then(|request| request.values("x-client-secret").into_iter().next())
        .unwrap_or_default()
        .to_string()
}

/// A reload that fails — a malformed line, then loose permissions — keeps
/// the last good headers, never panics or stops the sender, and logs one
/// WARN per distinct file state naming the path (and line), never a value
/// (#10961 rules, #11555). Fixing the file recovers on the next request.
///
/// A plain `#[test]` driving a current-thread runtime, because
/// [`crate::test_log_capture`] collects on the calling thread only.
#[test]
fn a_failed_reload_keeps_the_last_good_headers_and_warns_once_per_state() {
    let sink = HeaderSink::start();
    let (dir, path) = headers_file(&proxy_headers());
    let exporter = exporter_for(&sink, Some(&path));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let mut secret_after_each_step: Vec<String> = Vec::new();
    let logs = crate::test_log_capture::capture_logs(|| {
        runtime.block_on(async {
            exporter
                .emit_batch(&[sweep_started_envelope()])
                .await
                .unwrap();

            // Broken state 1: a malformed line 2.
            std::fs::write(&path, format!("X-Client-Secret: {CLIENT_SECRET}\n{CLIENT_ID}\n"))
                .unwrap();
            // Two requests under the same broken state: the WARN fires once.
            exporter
                .emit_batch(&[sweep_started_envelope()])
                .await
                .unwrap();
            exporter
                .emit_batch(&[sweep_started_envelope()])
                .await
                .unwrap();
            secret_after_each_step.push(last_secret(&sink));

            // Broken state 2: a rotation whose replacement file has loose
            // permissions — a new inode, so a reload is attempted and
            // refused. (An in-place `chmod` of the live file changes none
            // of mtime/len/inode and so is deliberately not re-read.)
            let staged = dir.path().join("otlp-headers.loose");
            std::fs::write(&staged, "X-Client-Secret: rotated-7f31\n").unwrap();
            set_mode(&staged, 0o644);
            std::fs::rename(&staged, &path).unwrap();
            exporter
                .emit_batch(&[sweep_started_envelope()])
                .await
                .unwrap();
            secret_after_each_step.push(last_secret(&sink));

            // Fixed: the very next request carries the fresh value.
            let fixed = dir.path().join("otlp-headers.fixed");
            std::fs::write(&fixed, "X-Client-Secret: rotated-7f31\n").unwrap();
            set_mode(&fixed, 0o600);
            std::fs::rename(&fixed, &path).unwrap();
            exporter
                .emit_batch(&[sweep_started_envelope()])
                .await
                .unwrap();
            secret_after_each_step.push(last_secret(&sink));
        });
    });

    assert_eq!(sink.requests().len(), 5, "the sender never stopped");
    // Both failures kept the last good headers…
    assert_eq!(secret_after_each_step[0], CLIENT_SECRET);
    assert_eq!(secret_after_each_step[1], CLIENT_SECRET);
    // …and the fix reached the next request.
    assert_eq!(secret_after_each_step[2], "rotated-7f31");

    let warnings: Vec<&str> = logs
        .iter()
        .filter(|(level, _)| *level == log::Level::Warn)
        .map(|(_, message)| message.as_str())
        .collect();
    assert_eq!(
        warnings.len(),
        2,
        "one WARN per distinct broken state, not per request: {logs:?}"
    );
    assert!(
        warnings
            .iter()
            .any(|message| message.contains(&path) && message.contains("line 2")),
        "the parse refusal names path and line: {warnings:?}"
    );
    assert!(
        warnings
            .iter()
            .any(|message| message.contains(&path) && message.contains("group or others")),
        "the permissions refusal names the path: {warnings:?}"
    );
    for (_, message) in &logs {
        for value in [CLIENT_ID, CLIENT_SECRET, "rotated-7f31"] {
            assert!(!message.contains(value), "log line leaked a header value: {message}");
        }
    }
}
