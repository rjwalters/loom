//! The relay end to end over real loopback sockets (#10964): a launched
//! session's request in, an identity-bound, scrubbed request out of the real
//! `OtlpExporter` — plus every way the receiver refuses.
//!
//! No test registers the process-global relay; each builds its own pieces.
use super::super::transport::Signal;
use super::forward::{self, RelayQueue};
use super::sanitize::{Encoding, Payload};
use super::server::{self, Receiver};
use super::*;
use crate::observability::agent_relay::{
    Harness, SessionIdentity, SessionKind, SessionLease, ENABLED_ENV, RELAY_HEADERS_ENV,
};
use prost::Message;
use serde_json::json;
use serial_test::serial;
use std::process::Command;
use std::sync::atomic::Ordering;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// The upstream's own credential — never the session's.
const INGEST_KEY: &str = "fixture-ingest-key-7c1d";
const OPTED_IN: &str = r#"{"observability":{"agentRelay":{"enabled":true}}}"#;

struct EnvGuard(Option<std::ffi::OsString>);

impl EnvGuard {
    fn clear() -> Self {
        let previous = std::env::var_os(ENABLED_ENV);
        std::env::remove_var(ENABLED_ENV);
        std::env::remove_var("LOOM_SWEEP_CONTAINERIZED");
        Self(previous)
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.0 {
            Some(value) => std::env::set_var(ENABLED_ENV, value),
            None => std::env::remove_var(ENABLED_ENV),
        }
    }
}

fn workspace(config: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
    std::fs::write(dir.path().join(".loom/config.json"), config).unwrap();
    dir
}

// ---------------------------------------------------------------------------
// A stand-in upstream OTLP endpoint
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Seen {
    path: String,
    headers: Vec<(String, String)>,
    body: String,
}

#[derive(Clone, Copy, PartialEq)]
enum Answer {
    Accept,
    /// Read the request, then never answer.
    Hang,
}

struct UpstreamStub {
    addr: std::net::SocketAddr,
    seen: Arc<Mutex<Vec<Seen>>>,
    task: tokio::task::JoinHandle<()>,
}

impl UpstreamStub {
    async fn start(answer: Answer) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let task = {
            let seen = seen.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((mut stream, _)) = listener.accept().await else {
                        break;
                    };
                    let seen = seen.clone();
                    tokio::spawn(async move {
                        let Some(request) = read_request(&mut stream).await else {
                            return;
                        };
                        seen.lock().unwrap().push(request);
                        if answer == Answer::Hang {
                            std::future::pending::<()>().await;
                        }
                        let _ = stream
                            .write_all(
                                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                                  Content-Length: 2\r\nConnection: close\r\n\r\n{}",
                            )
                            .await;
                    });
                }
            })
        };
        UpstreamStub { addr, seen, task }
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

impl Drop for UpstreamStub {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn read_request(stream: &mut TcpStream) -> Option<Seen> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 8192];
    let end = loop {
        if let Some(position) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            break position + 4;
        }
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8_lossy(&buffer[..end]).to_string();
    let mut lines = head.lines();
    let path = lines.next()?.split_whitespace().nth(1)?.to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    let length: usize = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse().ok())
        .unwrap_or(0);
    let mut body = buffer[end..].to_vec();
    while body.len() < length {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    Some(Seen {
        path,
        headers,
        body: String::from_utf8_lossy(&body).to_string(),
    })
}

// ---------------------------------------------------------------------------
// The relay under test
// ---------------------------------------------------------------------------

struct Stack {
    relay: Arc<Relay>,
    receiver: Arc<Receiver>,
    queue: Arc<RelayQueue>,
    root: tempfile::TempDir,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    _env: EnvGuard,
}

impl Stack {
    /// A receiver forwarding to `upstream_url` through a real `OtlpExporter`.
    async fn start(upstream_url: &str, limits: server::Limits, queue: forward::Limits) -> Self {
        let env = EnvGuard::clear();
        let listener = bind_loopback().unwrap();
        let relay = Relay::new(listener.local_addr().unwrap(), "host-fixture", None).unwrap();
        let queue = RelayQueue::new(queue);
        let exporter = OtlpExporter::new(upstream_url.to_string(), INGEST_KEY.to_string()).unwrap();
        let receiver = Receiver::new(relay.clone(), vec![queue.clone()], limits);
        let tasks = vec![
            tokio::spawn(forward::drain(queue.clone(), exporter, Duration::from_millis(5))),
            tokio::spawn(server::serve(listener, receiver.clone())),
        ];
        let root = workspace(OPTED_IN);
        relay.note_repo(root.path(), "example-owner/example-repo");
        Stack {
            relay,
            receiver,
            queue,
            root,
            tasks,
            _env: env,
        }
    }

    async fn default(upstream_url: &str) -> Self {
        Self::start(
            upstream_url,
            server::Limits::default(),
            forward::Limits {
                max_requests: MAX_QUEUED_REQUESTS,
                max_bytes: MAX_QUEUED_BYTES,
            },
        )
        .await
    }

    /// Launch a sweep session exactly as the daemon does and return what the
    /// child would find in its environment: the token, and its lease.
    fn launch(&self, issue: u32, sweep_id: &str) -> (String, SessionLease) {
        let mut command = Command::new("/bin/true");
        let identity = SessionIdentity {
            harness: Harness::ClaudeCode,
            kind: SessionKind::Sweep,
            role: None,
            issue: Some(issue),
            sweep_id: Some(sweep_id.to_string()),
            workspace_root: self.root.path().to_path_buf(),
        };
        let lease = self
            .relay
            .prepare(&mut command, self.root.path(), identity)
            .unwrap();
        let header = command
            .get_envs()
            .find(|(name, _)| *name == std::ffi::OsStr::new(RELAY_HEADERS_ENV))
            .and_then(|(_, value)| value.map(|v| v.to_string_lossy().into_owned()))
            .unwrap();
        let token = header
            .strip_prefix("Authorization=Bearer ")
            .unwrap()
            .to_string();
        (token, lease)
    }

    /// Send raw bytes and return the response's status code.
    async fn raw(&self, request: &[u8]) -> u16 {
        let mut stream = TcpStream::connect(self.relay.addr()).await.unwrap();
        // The receiver may answer (and close) before the whole request is
        // written — a refusal before the body is the point of several tests.
        let _ = stream.write_all(request).await;
        status_of(&mut stream).await
    }

    async fn post(&self, path: &str, token: Option<&str>, content_type: &str, body: &[u8]) -> u16 {
        let authorization = token
            .map(|token| format!("Authorization: Bearer {token}\r\n"))
            .unwrap_or_default();
        let mut request = format!(
            "POST {path} HTTP/1.1\r\nHost: {}\r\n{authorization}Content-Type: {content_type}\r\n\
             Content-Length: {}\r\n\r\n",
            self.relay.addr(),
            body.len()
        )
        .into_bytes();
        request.extend_from_slice(body);
        self.raw(&request).await
    }
}

impl Drop for Stack {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

async fn status_of(stream: &mut TcpStream) -> u16 {
    let mut response = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut response)).await;
    String::from_utf8_lossy(&response)
        .split_whitespace()
        .nth(1)
        .and_then(|status| status.parse().ok())
        .unwrap_or(0)
}

async fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A logs request from a sender that lies about who it is and leaks a
/// (synthetic) credential.
fn lying_leaky_logs() -> serde_json::Value {
    json!({"resourceLogs": [{
        "resource": {"attributes": [
            {"key": "service.name", "value": {"stringValue": "forged-service"}},
            {"key": "host.id", "value": {"stringValue": "forged-host"}},
            {"key": "loom.repo", "value": {"stringValue": "forged-owner/forged-repo"}},
            {"key": "loom.issue", "value": {"intValue": "1"}},
            {"key": "loom.sweep_id", "value": {"stringValue": "forged-sweep"}}]},
        "scopeLogs": [{"logRecords": [{
            "timeUnixNano": "1700000000000000000",
            "body": {"stringValue": "ran git push with ghp_abcdefghijklmnopqrstuvwxyz0123"},
            "attributes": [
                {"key": "loom.role", "value": {"stringValue": "forged-role"}},
                {"key": "password", "value": {"stringValue": "hunter2hunter2"}},
                {"key": "model", "value": {"stringValue": "fixture-model-1"}}]
        }]}]
    }]})
}

fn as_protobuf(logs: &serde_json::Value) -> Vec<u8> {
    let Payload::Logs(request) =
        super::sanitize::decode(Signal::Logs, Encoding::Json, logs.to_string().as_bytes()).unwrap()
    else {
        panic!()
    };
    request.encode_to_vec()
}

fn resource_value(seen: &Seen, key: &str) -> Option<serde_json::Value> {
    let body: serde_json::Value = serde_json::from_str(&seen.body).unwrap();
    let resources = ["resourceLogs", "resourceMetrics", "resourceSpans"]
        .iter()
        .find_map(|name| body.get(name))?;
    resources[0]["resource"]["attributes"]
        .as_array()?
        .iter()
        .find(|attribute| attribute["key"] == key)
        .map(|attribute| attribute["value"].clone())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn a_lying_leaky_session_is_forwarded_bound_and_scrubbed() {
    let upstream = UpstreamStub::start(Answer::Accept).await;
    let stack = Stack::default(&upstream.url()).await;
    let (token, _lease) = stack.launch(4242, "sweep-fixture-1");

    let body = as_protobuf(&lying_leaky_logs());
    assert_eq!(
        stack
            .post("/v1/logs", Some(&token), "application/x-protobuf", &body)
            .await,
        200
    );
    until("the request to reach the upstream", || !upstream.seen().is_empty()).await;

    let seen = upstream.seen();
    assert_eq!(seen.len(), 1);
    let request = &seen[0];
    assert_eq!(request.path, "/v1/logs");
    // The daemon's identity, not the sender's.
    assert_eq!(resource_value(request, "service.name").unwrap()["stringValue"], "claude-code");
    assert_eq!(resource_value(request, "host.id").unwrap()["stringValue"], "host-fixture");
    assert_eq!(
        resource_value(request, "loom.repo").unwrap()["stringValue"],
        "example-owner/example-repo"
    );
    assert_eq!(resource_value(request, "loom.issue").unwrap()["intValue"], "4242");
    assert_eq!(
        resource_value(request, "loom.sweep_id").unwrap()["stringValue"],
        "sweep-fixture-1"
    );
    assert!(!request.body.contains("forged"), "{}", request.body);
    // Scrubbed.
    assert!(!request.body.contains("ghp_abcdefghijklmnopqrstuvwxyz0123"), "{}", request.body);
    assert!(!request.body.contains("hunter2hunter2"), "{}", request.body);
    assert!(request.body.contains("[REDACTED:github-token]"));
    assert!(request.body.contains("fixture-model-1"));
    // Authenticated upstream as the exporter, never with the session's token.
    let authorization: Vec<&str> = request
        .headers
        .iter()
        .filter(|(name, _)| name == "authorization")
        .map(|(_, value)| value.as_str())
        .collect();
    assert_eq!(authorization, [format!("Bearer {INGEST_KEY}").as_str()]);
    assert!(!format!("{request:?}").contains(&token), "the session token went upstream");
    assert_eq!(stack.receiver.counters.accepted.load(Ordering::Relaxed), 1);
}

#[tokio::test]
#[serial]
async fn all_three_signals_are_relayed_in_json_and_chunked_bodies_are_read() {
    let upstream = UpstreamStub::start(Answer::Accept).await;
    let stack = Stack::default(&upstream.url()).await;
    let (token, _lease) = stack.launch(7, "sweep-fixture-2");
    let json_type = "application/json";

    let metrics = json!({"resourceMetrics": [{"scopeMetrics": [{"metrics": [{
        "name": "fixture.counter",
        "sum": {"aggregationTemporality": 2, "isMonotonic": true,
                "dataPoints": [{"asInt": "3", "timeUnixNano": "1700000000000000000"}]}}]}]}]});
    let traces = json!({"resourceSpans": [{"scopeSpans": [{"spans": [{
        "traceId": "0af7651916cd43dd8448eb211c80319c", "spanId": "b7ad6b7169203331",
        "name": "fixture.span", "startTimeUnixNano": "1700000000000000000",
        "endTimeUnixNano": "1700000001000000000"}]}]}]});
    let metrics = metrics.to_string();
    let traces = traces.to_string();
    assert_eq!(
        stack
            .post("/v1/metrics", Some(&token), json_type, metrics.as_bytes())
            .await,
        200
    );
    assert_eq!(
        stack
            .post("/v1/traces", Some(&token), json_type, traces.as_bytes())
            .await,
        200
    );

    // A streaming client frames its body as chunks rather than a length.
    let logs = lying_leaky_logs().to_string();
    let (first, second) = logs.split_at(logs.len() / 2);
    let chunked = format!(
        "POST /v1/logs HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {token}\r\n\
         Content-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n\
         {:x}\r\n{first}\r\n{:x};ext=1\r\n{second}\r\n0\r\n\r\n",
        first.len(),
        second.len()
    );
    assert_eq!(stack.raw(chunked.as_bytes()).await, 200);

    until("three requests upstream", || upstream.seen().len() == 3).await;
    let seen = upstream.seen();
    let paths: Vec<&str> = seen.iter().map(|request| request.path.as_str()).collect();
    assert_eq!(paths, ["/v1/metrics", "/v1/traces", "/v1/logs"]);
    for request in &seen {
        assert_eq!(
            resource_value(request, "service.name").unwrap()["stringValue"],
            "claude-code",
            "{}",
            request.path
        );
        assert_eq!(resource_value(request, "loom.issue").unwrap()["intValue"], "7");
    }
    assert!(seen[1].body.contains("0af7651916cd43dd8448eb211c80319c"));
}

#[tokio::test]
#[serial]
async fn a_request_without_a_live_sessions_token_is_refused_before_its_body_is_read() {
    let upstream = UpstreamStub::start(Answer::Accept).await;
    let stack = Stack::default(&upstream.url()).await;
    let (token, lease) = stack.launch(4242, "sweep-fixture-1");
    let body = lying_leaky_logs().to_string();
    let json_type = "application/json";

    assert_eq!(
        stack
            .post("/v1/logs", None, json_type, body.as_bytes())
            .await,
        401
    );
    assert_eq!(
        stack
            .post("/v1/logs", Some("not-a-token"), json_type, body.as_bytes())
            .await,
        401
    );
    // The upstream's own key is not a session token either.
    assert_eq!(
        stack
            .post("/v1/logs", Some(INGEST_KEY), json_type, body.as_bytes())
            .await,
        401
    );
    // A token in the wrong scheme, or presented twice, is not accepted.
    let basic = format!(
        "POST /v1/logs HTTP/1.1\r\nAuthorization: Basic {token}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    assert_eq!(stack.raw(basic.as_bytes()).await, 401);
    let twice = format!(
        "POST /v1/logs HTTP/1.1\r\nAuthorization: Bearer {token}\r\nAuthorization: Bearer {token}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    assert_eq!(stack.raw(twice.as_bytes()).await, 401);
    // Refused on the head alone: a declared 1 GiB body is never waited for.
    let began = Instant::now();
    let huge = "POST /v1/logs HTTP/1.1\r\nAuthorization: Bearer nope\r\n\
                Content-Type: application/json\r\nContent-Length: 1073741824\r\n\r\n";
    assert_eq!(stack.raw(huge.as_bytes()).await, 401);
    assert!(began.elapsed() < Duration::from_secs(5));

    // The live token works — and stops working the moment the session ends.
    assert_eq!(
        stack
            .post("/v1/logs", Some(&token), json_type, body.as_bytes())
            .await,
        200
    );
    drop(lease);
    assert_eq!(
        stack
            .post("/v1/logs", Some(&token), json_type, body.as_bytes())
            .await,
        401
    );

    until("the one accepted request upstream", || !upstream.seen().is_empty()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(upstream.seen().len(), 1, "a refused request must never be forwarded");
    assert_eq!(stack.receiver.counters.unauthorized.load(Ordering::Relaxed), 7);
}

#[tokio::test]
#[serial]
async fn malformed_requests_get_an_error_status_and_the_receiver_keeps_serving() {
    let upstream = UpstreamStub::start(Answer::Accept).await;
    let limits = server::Limits {
        max_body: 4096,
        ..server::Limits::default()
    };
    let queue = forward::Limits {
        max_requests: 8,
        max_bytes: 1 << 20,
    };
    let stack = Stack::start(&upstream.url(), limits, queue).await;
    let (token, _lease) = stack.launch(4242, "sweep-fixture-1");
    let auth = format!("Authorization: Bearer {token}\r\n");
    let json_type = "application/json";

    // Bodies that will never decode.
    for (content_type, body) in [
        (json_type, b"{".as_slice()),
        (json_type, b"not json".as_slice()),
        (json_type, br#"{"resourceLogs": 7}"#.as_slice()),
        ("application/x-protobuf", [0xffu8; 32].as_slice()),
    ] {
        assert_eq!(
            stack
                .post("/v1/logs", Some(&token), content_type, body)
                .await,
            400
        );
    }
    // Wrong place, wrong verb, wrong media.
    assert_eq!(
        stack
            .post("/v1/profiles", Some(&token), json_type, b"{}")
            .await,
        404
    );
    assert_eq!(stack.post("/", Some(&token), json_type, b"{}").await, 404);
    assert_eq!(
        stack
            .raw(format!("GET /v1/logs HTTP/1.1\r\n{auth}\r\n").as_bytes())
            .await,
        405
    );
    assert_eq!(
        stack
            .post("/v1/logs", Some(&token), "text/plain", b"{}")
            .await,
        415
    );
    let gzip = format!(
        "POST /v1/logs HTTP/1.1\r\n{auth}Content-Type: application/json\r\n\
         Content-Encoding: gzip\r\nContent-Length: 2\r\n\r\n{{}}"
    );
    assert_eq!(stack.raw(gzip.as_bytes()).await, 415);
    // Too large: by declared length (refused unread), and by chunked growth.
    let declared = format!(
        "POST /v1/logs HTTP/1.1\r\n{auth}Content-Type: application/json\r\nContent-Length: 4097\r\n\r\n"
    );
    assert_eq!(stack.raw(declared.as_bytes()).await, 413);
    let chunk = "x".repeat(3000);
    let growing = format!(
        "POST /v1/logs HTTP/1.1\r\n{auth}Content-Type: application/json\r\n\
         Transfer-Encoding: chunked\r\n\r\nbb8\r\n{chunk}\r\nbb8\r\n{chunk}\r\n0\r\n\r\n"
    );
    assert_eq!(stack.raw(growing.as_bytes()).await, 413);
    let oversized_head = format!("POST /v1/logs HTTP/1.1\r\nX-Pad: {}\r\n\r\n", "a".repeat(40_000));
    assert_eq!(stack.raw(oversized_head.as_bytes()).await, 413);
    // Broken framing.
    for request in [
        "\r\n\r\n".to_string(),
        "POST\r\n\r\n".to_string(),
        "POST /v1/logs\r\n\r\n".to_string(),
        "POST /v1/logs SPDY/3\r\n\r\n".to_string(),
        format!("POST /v1/logs HTTP/1.1\r\n{auth}no-colon-here\r\n\r\n"),
        format!("POST /v1/logs HTTP/1.1\r\n{auth}Content-Type: application/json\r\nContent-Length: two\r\n\r\n"),
        format!("POST /v1/logs HTTP/1.1\r\n{auth}Content-Type: application/json\r\nContent-Length: 2\r\nContent-Length: 2\r\n\r\n{{}}"),
        format!("POST /v1/logs HTTP/1.1\r\n{auth}Content-Type: application/json\r\nContent-Length: 2\r\nTransfer-Encoding: chunked\r\n\r\n{{}}"),
        format!("POST /v1/logs HTTP/1.1\r\n{auth}Content-Type: application/json\r\nTransfer-Encoding: gzip\r\n\r\n"),
        format!("POST /v1/logs HTTP/1.1\r\n{auth}Content-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n"),
        format!("POST /v1/logs HTTP/1.1\r\n{auth}Content-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{{}}XX0\r\n\r\n"),
    ] {
        assert_eq!(stack.raw(request.as_bytes()).await, 400, "{request:?}");
    }
    let no_length =
        format!("POST /v1/logs HTTP/1.1\r\n{auth}Content-Type: application/json\r\n\r\n");
    assert_eq!(stack.raw(no_length.as_bytes()).await, 411);
    // Bytes that are not HTTP at all.
    assert_eq!(
        stack
            .raw(&[0xff, 0xfe, 0x00, 0x01, b'\r', b'\n', b'\r', b'\n'])
            .await,
        400
    );

    // After all of that the receiver still serves, and none of it went on.
    let body = lying_leaky_logs().to_string();
    assert_eq!(
        stack
            .post("/v1/logs", Some(&token), json_type, body.as_bytes())
            .await,
        200
    );
    until("the one good request upstream", || !upstream.seen().is_empty()).await;
    assert_eq!(upstream.seen().len(), 1);
    assert_eq!(stack.receiver.counters.too_large.load(Ordering::Relaxed), 3);
}

#[tokio::test]
#[serial]
async fn an_empty_but_valid_request_is_acknowledged_and_not_forwarded() {
    let upstream = UpstreamStub::start(Answer::Accept).await;
    let stack = Stack::default(&upstream.url()).await;
    let (token, _lease) = stack.launch(1, "sweep-fixture-1");
    assert_eq!(
        stack
            .post("/v1/logs", Some(&token), "application/x-protobuf", b"")
            .await,
        200
    );
    assert_eq!(
        stack
            .post("/v1/traces", Some(&token), "application/json", b"{}")
            .await,
        200
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(upstream.seen().is_empty());
    assert_eq!(stack.queue.len(), 0);
}

#[tokio::test]
#[serial]
async fn connections_past_the_cap_are_turned_away_and_stalled_ones_time_out() {
    let upstream = UpstreamStub::start(Answer::Accept).await;
    let limits = server::Limits {
        max_connections: 2,
        head_timeout: Duration::from_millis(400),
        ..server::Limits::default()
    };
    let queue = forward::Limits {
        max_requests: 8,
        max_bytes: 1 << 20,
    };
    let stack = Stack::start(&upstream.url(), limits, queue).await;
    let (token, _lease) = stack.launch(1, "sweep-fixture-1");

    // Two clients connect and send nothing, holding both slots.
    let mut idle_a = TcpStream::connect(stack.relay.addr()).await.unwrap();
    let mut idle_b = TcpStream::connect(stack.relay.addr()).await.unwrap();
    until("both idle connections to be accepted", || {
        stack.receiver.permits_available() == 0
    })
    .await;
    // A third is answered at once rather than queued behind them.
    let began = Instant::now();
    assert_eq!(
        stack
            .post("/v1/logs", Some(&token), "application/json", b"{}")
            .await,
        503
    );
    assert!(began.elapsed() < Duration::from_millis(350), "{:?}", began.elapsed());
    assert_eq!(stack.receiver.counters.busy.load(Ordering::Relaxed), 1);

    // The idle ones are timed out, which frees their slots.
    assert_eq!(status_of(&mut idle_a).await, 408);
    assert_eq!(status_of(&mut idle_b).await, 408);
    until("the slots to be released", || stack.receiver.permits_available() == 2).await;
    assert_eq!(
        stack
            .post("/v1/logs", Some(&token), "application/json", b"{}")
            .await,
        200
    );
}

#[tokio::test]
#[serial]
async fn an_upstream_that_never_answers_does_not_slow_the_session_down() {
    let upstream = UpstreamStub::start(Answer::Hang).await;
    let queue = forward::Limits {
        max_requests: 4,
        max_bytes: 1 << 20,
    };
    let stack = Stack::start(&upstream.url(), server::Limits::default(), queue).await;
    let (token, _lease) = stack.launch(4242, "sweep-fixture-1");
    let body = lying_leaky_logs().to_string();

    let began = Instant::now();
    for _ in 0..40 {
        assert_eq!(
            stack
                .post("/v1/logs", Some(&token), "application/json", body.as_bytes())
                .await,
            200
        );
    }
    // Forty exports against a hung upstream, each acknowledged without
    // waiting on it (the exporter's own timeout is 20 s per request).
    assert!(began.elapsed() < Duration::from_secs(10), "{:?}", began.elapsed());
    assert_eq!(stack.queue.len(), 4, "the queue stays at its bound");
    assert_eq!(stack.queue.dropped_requests(), 36);
    assert_eq!(stack.queue.dropped_records(), 36);
}

#[tokio::test]
#[serial]
async fn an_unreachable_upstream_drops_with_a_count_and_reports_the_gap_on_recovery() {
    // Reserve a port, then close it: connections to it are refused.
    let reserved = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = reserved.local_addr().unwrap();
    drop(reserved);
    let queue = forward::Limits {
        max_requests: 2,
        max_bytes: 1 << 20,
    };
    let stack = Stack::start(&format!("http://{address}"), server::Limits::default(), queue).await;
    let (token, _lease) = stack.launch(4242, "sweep-fixture-1");
    let body = lying_leaky_logs().to_string();
    for _ in 0..5 {
        assert_eq!(
            stack
                .post("/v1/logs", Some(&token), "application/json", body.as_bytes())
                .await,
            200
        );
    }
    assert_eq!(stack.queue.len(), 2);
    assert_eq!(stack.queue.dropped_requests(), 3);

    // The upstream comes back on the same address.
    let listener = TcpListener::bind(address).await.unwrap();
    let seen = Arc::new(Mutex::new(Vec::<Seen>::new()));
    let server = {
        let seen = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                if let Some(request) = read_request(&mut stream).await {
                    seen.lock().unwrap().push(request);
                    let _ = stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                              Content-Length: 2\r\nConnection: close\r\n\r\n{}",
                        )
                        .await;
                }
            }
        })
    };
    until("the gap marker and the two survivors", || seen.lock().unwrap().len() >= 3).await;
    let seen = seen.lock().unwrap().clone();
    let marker: serde_json::Value = serde_json::from_str(&seen[0].body).unwrap();
    let record = &marker["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0];
    assert_eq!(record["eventName"], forward::GAP_EVENT);
    let attribute = |key: &str| {
        record["attributes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|attribute| attribute["key"] == key)
            .map(|attribute| attribute["value"].clone())
            .unwrap()
    };
    assert_eq!(attribute("loom.relay.gap_reason")["stringValue"], "relay_queue_overflow");
    assert_eq!(attribute("loom.relay.dropped_requests")["intValue"], "3");
    assert_eq!(attribute("loom.relay.dropped_log_records")["intValue"], "3");
    // The marker is filed under the session that lost the records.
    assert_eq!(
        resource_value(&seen[0], "loom.sweep_id").unwrap()["stringValue"],
        "sweep-fixture-1"
    );
    assert_eq!(resource_value(&seen[0], "service.name").unwrap()["stringValue"], "claude-code");
    server.abort();
}

#[tokio::test]
#[serial]
async fn the_receiver_starts_only_when_opted_in_and_an_otlp_sink_started() {
    let _env = EnvGuard::clear();
    let sink = Sink {
        endpoint: "http://127.0.0.1:9".to_string(),
        headers_file: None,
    };
    // An `otlp` sink without the relay's own switch: nothing.
    let off = workspace(
        r#"{"observability":{"enabled":true,"exporter":"otlp","endpoint":"http://127.0.0.1:9"}}"#,
    );
    assert!(launch(off.path(), "host-fixture", INGEST_KEY, std::slice::from_ref(&sink)).is_none());
    assert!(start(off.path(), "host-fixture", INGEST_KEY, std::slice::from_ref(&sink)).is_empty());
    // The switch without an `otlp` sink that started: nothing.
    let on = workspace(OPTED_IN);
    assert!(launch(on.path(), "host-fixture", INGEST_KEY, &[]).is_none());
    // A sink whose exporter cannot be built is not a sink.
    let unusable = Sink {
        endpoint: "ftp://127.0.0.1:9".to_string(),
        headers_file: None,
    };
    assert!(launch(on.path(), "host-fixture", INGEST_KEY, &[unusable]).is_none());
    assert!(agent_relay::global().is_none());

    // Both: a receiver on 127.0.0.1, on a port the kernel chose.
    let (relay, tasks) = launch(on.path(), "host-fixture", INGEST_KEY, &[sink]).unwrap();
    assert_eq!(relay.addr().ip(), std::net::IpAddr::V4(Ipv4Addr::LOCALHOST));
    assert_ne!(relay.addr().port(), 0);
    assert_eq!(tasks.len(), 2, "one drain per sink, plus the accept loop");
    // It answers on that address, and refuses a stranger.
    let mut stream = TcpStream::connect(relay.addr()).await.unwrap();
    stream
        .write_all(b"POST /v1/logs HTTP/1.1\r\nContent-Length: 0\r\n\r\n")
        .await
        .unwrap();
    assert_eq!(status_of(&mut stream).await, 401);
    // Two daemons on one host get two ports rather than a collision.
    let (second, more) = launch(
        on.path(),
        "host-fixture",
        INGEST_KEY,
        &[Sink {
            endpoint: "http://127.0.0.1:9".to_string(),
            headers_file: None,
        }],
    )
    .unwrap();
    assert_ne!(second.addr(), relay.addr());
    for task in tasks.into_iter().chain(more) {
        task.abort();
    }
}

// ---------------------------------------------------------------------------
// Review round 1: amplification and slow peers
// ---------------------------------------------------------------------------

/// `count` empty `ResourceLogs`, then one container holding one record: the
/// shape that used to decode into `count` full daemon-built resources.
fn empty_resources_then_one_record(count: usize) -> Vec<u8> {
    let mut body = Vec::with_capacity(count * 2 + 16);
    for _ in 0..count {
        body.extend_from_slice(&[0x0a, 0x00]);
    }
    // ResourceLogs { scope_logs: [ScopeLogs { log_records: [LogRecord {}] }] }
    body.extend_from_slice(&[0x0a, 0x04, 0x12, 0x02, 0x12, 0x00]);
    body
}

#[tokio::test]
#[serial]
async fn a_body_of_millions_of_empty_resources_is_refused_before_it_is_decoded() {
    let upstream = UpstreamStub::start(Answer::Accept).await;
    let stack = Stack::default(&upstream.url()).await;
    let (token, _lease) = stack.launch(4242, "sweep-fixture-1");

    // The reported reproduction: just over 4 MB of wire, two million
    // resources once decoded.
    let body = empty_resources_then_one_record(2_000_000);
    assert_eq!(body.len(), 4_000_006);
    let estimate = super::estimate::protobuf(Signal::Logs, &body).unwrap();
    assert!(
        estimate
            > 2_000_000 * std::mem::size_of::<opentelemetry_proto::tonic::logs::v1::ResourceLogs>()
    );
    let budget = server::DecodeBudget::default();
    assert!(estimate > budget.decoded(body.len()));
    assert!(budget.decoded(body.len()) <= MAX_DECODED_BYTES);

    let began = Instant::now();
    let status = stack
        .post("/v1/logs", Some(&token), "application/x-protobuf", &body)
        .await;
    assert_eq!(status, 413);
    // Refused on the estimate, a byte scan: no multi-gigabyte decode ran.
    assert!(began.elapsed() < Duration::from_secs(10), "{:?}", began.elapsed());
    assert_eq!(stack.queue.len(), 0);
    assert_eq!(stack.queue.held_bytes(), 0);
    assert_eq!(stack.receiver.counters.too_large.load(Ordering::Relaxed), 1);

    // The same shape in JSON is refused the same way.
    let mut json_body = String::from(r#"{"resourceLogs":["#);
    json_body.push_str(&"{},".repeat(1_300_000));
    json_body.push_str(r#"{"scopeLogs":[{"logRecords":[{}]}]}]}"#);
    let status = stack
        .post("/v1/logs", Some(&token), "application/json", json_body.as_bytes())
        .await;
    assert_eq!(status, 413);
    assert_eq!(stack.queue.len(), 0);

    // A modest number of empty containers is legitimate input: accepted,
    // pruned to one bound resource, and held at its true encoded size.
    let body = empty_resources_then_one_record(1_000);
    assert_eq!(
        stack
            .post("/v1/logs", Some(&token), "application/x-protobuf", &body)
            .await,
        200
    );
    until("the request upstream", || !upstream.seen().is_empty()).await;
    let forwarded: serde_json::Value = serde_json::from_str(&upstream.seen()[0].body).unwrap();
    assert_eq!(forwarded["resourceLogs"].as_array().unwrap().len(), 1);
}

#[tokio::test]
#[serial]
async fn a_request_that_binding_would_inflate_out_of_proportion_is_refused() {
    let upstream = UpstreamStub::start(Answer::Hang).await;
    let stack = Stack::default(&upstream.url()).await;
    let (token, _lease) = stack.launch(4242, "sweep-fixture-1");
    // Thousands of distinct sender resources, each with one record: small
    // enough per container to pass the decode estimate, but each would gain
    // a whole daemon-built resource when bound.
    let resource_logs: Vec<serde_json::Value> = (0..3_000)
        .map(|i| {
            json!({"resource": {"attributes": [
                {"key": "os.type", "value": {"stringValue": format!("{i:0>60}")}}]},
                "scopeLogs": [{"logRecords": [{}]}]})
        })
        .collect();
    let request = json!({ "resourceLogs": resource_logs });
    let Payload::Logs(decoded) =
        super::sanitize::decode(Signal::Logs, Encoding::Json, request.to_string().as_bytes())
            .unwrap()
    else {
        panic!()
    };
    let body = decoded.encode_to_vec();
    let budget = server::DecodeBudget::default();
    let estimate = super::estimate::protobuf(Signal::Logs, &body).unwrap();
    assert!(estimate <= budget.decoded(body.len()), "this case must reach binding");
    assert_eq!(
        stack
            .post("/v1/logs", Some(&token), "application/x-protobuf", &body)
            .await,
        413
    );
    assert_eq!(stack.queue.len(), 0);

    // An ordinary request is held at exactly its encoded, bound size.
    let ordinary = as_protobuf(&lying_leaky_logs());
    assert_eq!(
        stack
            .post("/v1/logs", Some(&token), "application/x-protobuf", &ordinary)
            .await,
        200
    );
    until("the request to be queued or sent", || {
        stack.queue.len() + upstream.seen().len() > 0
    })
    .await;
    // With the upstream hung, the request is held (in flight at the head).
    assert!(stack.queue.held_bytes() > 0);
    assert!(stack.queue.held_bytes() <= server::DecodeBudget::default().bound(ordinary.len()) * 2);
}

#[tokio::test]
#[serial]
async fn a_peer_without_a_head_is_cut_off_quickly_and_a_slow_authenticated_body_is_not() {
    let upstream = UpstreamStub::start(Answer::Accept).await;
    let limits = server::Limits {
        head_timeout: Duration::from_millis(300),
        read_timeout: Duration::from_secs(10),
        ..server::Limits::default()
    };
    let queue = forward::Limits {
        max_requests: 8,
        max_bytes: 1 << 20,
    };
    let stack = Stack::start(&upstream.url(), limits, queue).await;
    let (token, _lease) = stack.launch(1, "sweep-fixture-1");

    // Silent, and a head trickled in too slowly: both answered 408 at the
    // head deadline, long before the body budget.
    let began = Instant::now();
    let mut silent = TcpStream::connect(stack.relay.addr()).await.unwrap();
    let mut partial = TcpStream::connect(stack.relay.addr()).await.unwrap();
    partial
        .write_all(b"POST /v1/logs HTTP/1.1\r\n")
        .await
        .unwrap();
    assert_eq!(status_of(&mut silent).await, 408);
    assert_eq!(status_of(&mut partial).await, 408);
    assert!(began.elapsed() < Duration::from_secs(3), "{:?}", began.elapsed());

    // An authenticated head, then the body after the head deadline: fine.
    let body = lying_leaky_logs().to_string();
    let mut stream = TcpStream::connect(stack.relay.addr()).await.unwrap();
    let head = format!(
        "POST /v1/logs HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;
    stream.write_all(body.as_bytes()).await.unwrap();
    assert_eq!(status_of(&mut stream).await, 200);
}

#[tokio::test]
#[serial]
async fn a_flood_of_connections_past_the_cap_is_bounded_and_the_receiver_recovers() {
    let upstream = UpstreamStub::start(Answer::Accept).await;
    let limits = server::Limits {
        max_connections: 2,
        head_timeout: Duration::from_millis(300),
        ..server::Limits::default()
    };
    let queue = forward::Limits {
        max_requests: 8,
        max_bytes: 1 << 20,
    };
    let stack = Stack::start(&upstream.url(), limits, queue).await;
    let (token, _lease) = stack.launch(1, "sweep-fixture-1");
    let mut held = Vec::new();
    for _ in 0..2 {
        held.push(TcpStream::connect(stack.relay.addr()).await.unwrap());
    }
    until("both slots taken", || stack.receiver.permits_available() == 0).await;
    // Many more: each is answered 503 or simply closed — or, if it landed
    // after a held slot's head deadline freed it, timed out like any silent
    // peer. Never left waiting.
    let mut flood = Vec::new();
    for _ in 0..200 {
        flood.push(TcpStream::connect(stack.relay.addr()).await.unwrap());
    }
    let began = Instant::now();
    for mut stream in flood {
        let status = status_of(&mut stream).await;
        assert!(matches!(status, 503 | 408 | 0), "{status}");
    }
    assert!(began.elapsed() < Duration::from_secs(8), "{:?}", began.elapsed());
    assert!(stack.receiver.counters.busy.load(Ordering::Relaxed) > 0);
    drop(held);
    until("the slots to be released", || stack.receiver.permits_available() == 2).await;
    assert_eq!(
        stack
            .post("/v1/logs", Some(&token), "application/json", b"{}")
            .await,
        200
    );
}
