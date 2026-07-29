//! `loom-daemon serve` (Issue #4391 — dashboard phase 1 of #4329).
//!
//! A minimal, read-only HTTP status-snapshot listener. A single endpoint —
//! `GET /api/status` — serializes the same [`crate::types::DaemonStatusReport`]
//! data `loom-daemon status --json` already aggregates, fetched live over the
//! *existing* Unix socket (the same `Request::DaemonStatus` / `Response::DaemonStatus`
//! wire contract [`crate::ipc::build_daemon_status`] already answers) — this
//! module never re-derives the report itself, and it starts nothing until the
//! `serve` subcommand is explicitly invoked (never from the default daemon-run
//! path, never from a config value alone).
//!
//! # Security posture
//!
//! - Off by default: nothing listens unless `loom-daemon serve` is invoked.
//! - Binds `127.0.0.1` (loopback) by default.
//! - A non-loopback bind (e.g. a tailnet interface, for cross-host fleet
//!   visibility per #4391's operator comment) requires the *separate* explicit
//!   `--allow-non-loopback` flag — the bind address alone is never enough.
//! - A wildcard/unspecified bind (`0.0.0.0` / `::`) is refused unconditionally,
//!   even with `--allow-non-loopback`: this endpoint must never become
//!   reachable from the public internet, only from an explicit interface
//!   address (loopback or a specific tailnet IP).
//! - Read-only and stateless: every request re-fetches the live snapshot over
//!   the socket; nothing is written to disk, and no daemon state is mutated.
//!
//! # HTTP dependency decision
//!
//! Hand-rolled HTTP/1.1 over [`tokio::net::TcpListener`] rather than pulling in
//! `axum`/`hyper`: a single read-only GET endpoint on a localhost-by-default
//! surface does not warrant a full HTTP framework and its dependency tree
//! (`loom-daemon/Cargo.toml` currently has no HTTP framework at all — tokio
//! only). This keeps the same tradeoff the daemon already makes for its
//! Unix-socket IPC protocol (also hand-rolled, newline-delimited JSON).
//! Revisit if a later phase (SSE, #4392) needs persistent multi-frame
//! connections beyond what this minimal responder supports.

use crate::types::{DaemonStatusReport, Request, Response};
use anyhow::{anyhow, Result};
use serde::Serialize;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream, UnixStream};

/// Default TCP port for `loom-daemon serve`.
pub const DEFAULT_PORT: u16 = 7420;

/// Path this module's single endpoint answers.
const STATUS_PATH: &str = "/api/status";

/// Bounded timeout for a single connect + `DaemonStatus` round-trip against
/// the daemon's own Unix socket. Deliberately a single attempt (unlike
/// `loom-daemon status`'s dropped-connection retry, #4279) — a dashboard
/// poller simply retries on its own next tick, so extra client-side
/// complexity here is not worth it for a phase-1 read-only surface.
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// Cap on bytes read while parsing an incoming HTTP request's headers, so a
/// misbehaving/slow client cannot pin this task on an unbounded read (this
/// listener never needs a request body — every request is a bare GET).
const MAX_REQUEST_HEADER_BYTES: usize = 8 * 1024;

/// Validate a requested bind address against the security posture documented
/// on this module (Issue #4391's non-negotiable constraints, carried from the
/// #4329 parent). Returns `Err(<reason>)` when the bind must be refused.
///
/// - Loopback (`127.0.0.1`, `::1`) is always allowed.
/// - A wildcard/unspecified address (`0.0.0.0`, `::`) is refused
///   unconditionally — never reachable via `--allow-non-loopback` alone, so
///   this listener can never become a public-internet surface.
/// - Any other non-loopback address (e.g. a tailnet interface IP) requires
///   `allow_non_loopback == true`.
pub fn validate_bind(addr: IpAddr, allow_non_loopback: bool) -> Result<(), String> {
    if addr.is_loopback() {
        return Ok(());
    }
    if addr.is_unspecified() {
        return Err(format!(
            "refusing to bind wildcard address {addr} — loom-daemon serve never binds \
             0.0.0.0/:: (would be reachable from any interface, including the public \
             internet); bind a specific loopback or tailnet interface address instead"
        ));
    }
    if !allow_non_loopback {
        return Err(format!(
            "refusing non-loopback bind {addr} without --allow-non-loopback — pass \
             --allow-non-loopback to explicitly opt in (e.g. for a tailnet interface \
             address); the bind address alone is never enough"
        ));
    }
    Ok(())
}

/// The JSON payload every response from this listener's `/api/status`
/// endpoint carries: the same [`DaemonStatusReport`] `loom-daemon status
/// --json` aggregates, flattened alongside a `hostname` field (Issue #4391's
/// operator comment: multihost visibility needs a host-identity field so a
/// later client-side aggregator — #4393 — can label sources without any
/// server-side fan-out in this phase).
#[derive(Debug, Clone, Serialize)]
pub struct StatusSnapshot {
    /// This host's identity, via [`crate::sweep_registry::host_identity`]
    /// (`LOOM_HOST_ID` env > `$HOSTNAME` env > the `hostname` binary >
    /// `"unknown-host"`) — loom's single existing host-identity concept,
    /// reused rather than inventing a second one for this endpoint.
    pub hostname: String,
    #[serde(flatten)]
    pub report: DaemonStatusReport,
}

/// Wrap a freshly-fetched [`DaemonStatusReport`] with this host's identity.
#[must_use]
pub fn build_snapshot(report: DaemonStatusReport) -> StatusSnapshot {
    StatusSnapshot {
        hostname: crate::sweep_registry::host_identity(),
        report,
    }
}

/// Fetch the live [`DaemonStatusReport`] from the running daemon over its
/// Unix socket — the exact same `Request::DaemonStatus` request
/// `loom-daemon status --json` sends, so the aggregation itself (dynamic
/// caps, per-repo breakdown, capacity, drain state, …) is computed exactly
/// once, in [`crate::ipc::build_daemon_status`], never re-derived here.
async fn fetch_report(socket_path: &Path) -> Result<DaemonStatusReport> {
    let roundtrip = async {
        let stream = UnixStream::connect(socket_path)
            .await
            .map_err(|e| anyhow!("connect to daemon socket failed: {e}"))?;
        let (reader, mut writer) = stream.into_split();

        let request_json = serde_json::to_string(&Request::DaemonStatus)?;
        writer.write_all(request_json.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;

        let mut lines = BufReader::new(reader).lines();
        let line = lines
            .next_line()
            .await?
            .ok_or_else(|| anyhow!("daemon closed the connection without responding"))?;
        let response: Response = serde_json::from_str(&line)?;
        match response {
            Response::DaemonStatus(report) => Ok(*report),
            Response::Error { message } => Err(anyhow!("daemon error: {message}")),
            other => Err(anyhow!("unexpected response: {other:?}")),
        }
    };

    tokio::time::timeout(FETCH_TIMEOUT, roundtrip)
        .await
        .map_err(|_| anyhow!("status round-trip timed out after {}s", FETCH_TIMEOUT.as_secs()))?
}

/// Fetch a fresh [`StatusSnapshot`] (report + hostname) from the running
/// daemon. Errors when the daemon is unreachable or the round-trip fails —
/// the HTTP layer maps that to a 503.
pub async fn fetch_snapshot(socket_path: &Path) -> Result<StatusSnapshot> {
    fetch_report(socket_path).await.map(build_snapshot)
}

/// A minimal parsed HTTP/1.1 request line: method + path (query string, if
/// any, is stripped — this listener has exactly one route and no query
/// params). Headers and any body are read and discarded; this listener never
/// needs them.
struct ParsedRequest {
    method: String,
    path: String,
}

/// Read and minimally parse a single HTTP/1.1 request from `reader`: the
/// request line, then headers up to the blank line that terminates them.
/// Bounded by [`MAX_REQUEST_HEADER_BYTES`] so a slow/malicious client cannot
/// pin this task open. This listener never reads a body (every route is a
/// bare GET), so any body bytes the client sent are simply left unread —
/// harmless because every response sets `Connection: close`.
async fn parse_request<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<ParsedRequest> {
    let mut total_bytes = 0usize;
    let mut request_line: Option<String> = None;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Err(anyhow!("connection closed before a complete request was read"));
        }
        total_bytes += n;
        if total_bytes > MAX_REQUEST_HEADER_BYTES {
            return Err(anyhow!("request headers exceeded {MAX_REQUEST_HEADER_BYTES} bytes"));
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if request_line.is_none() {
            request_line = Some(trimmed.to_string());
            continue;
        }
        // Blank line terminates the header block.
        if trimmed.is_empty() {
            break;
        }
    }

    let request_line = request_line.ok_or_else(|| anyhow!("no request line"))?;
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| anyhow!("malformed request line: {request_line:?}"))?
        .to_string();
    let raw_path = parts
        .next()
        .ok_or_else(|| anyhow!("malformed request line: {request_line:?}"))?;
    let path = raw_path.split('?').next().unwrap_or(raw_path).to_string();

    Ok(ParsedRequest { method, path })
}

/// Write a well-formed HTTP/1.1 JSON response. Always `Connection: close` —
/// this minimal responder does not support keep-alive/pipelining, so every
/// connection is single-request.
async fn write_json_response(stream: &mut TcpStream, status_line: &str, body: &str) -> Result<()> {
    let response = format!(
        "HTTP/1.1 {status_line}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        body.len(),
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

/// Handle a single accepted connection: parse the request, route it, and
/// write exactly one response. Every branch is read-only — this function
/// never mutates daemon state and never writes to disk.
async fn handle_connection(mut stream: TcpStream, socket_path: PathBuf) -> Result<()> {
    // Scoped so the `BufReader`'s mutable borrow of `stream` ends before this
    // function needs `&mut stream` again to write the response.
    let parse_result = {
        let mut buf_reader = BufReader::new(&mut stream);
        parse_request(&mut buf_reader).await
    };
    let parsed = match parse_result {
        Ok(p) => p,
        Err(_) => {
            // Malformed/incomplete request: best-effort 400, ignore write
            // failures (the client may already be gone).
            let _ = write_json_response(
                &mut stream,
                "400 Bad Request",
                r#"{"error":"malformed request"}"#,
            )
            .await;
            return Ok(());
        }
    };

    if parsed.path != STATUS_PATH {
        write_json_response(
            &mut stream,
            "404 Not Found",
            &format!(r#"{{"error":"not found","path":{}}}"#, serde_json::to_string(&parsed.path)?),
        )
        .await?;
        return Ok(());
    }

    if parsed.method != "GET" {
        write_json_response(
            &mut stream,
            "405 Method Not Allowed",
            r#"{"error":"method not allowed, only GET is supported"}"#,
        )
        .await?;
        return Ok(());
    }

    match fetch_snapshot(&socket_path).await {
        Ok(snapshot) => {
            let body = serde_json::to_string(&snapshot)?;
            write_json_response(&mut stream, "200 OK", &body).await?;
        }
        Err(e) => {
            let body = serde_json::json!({ "error": format!("daemon unreachable: {e}") });
            write_json_response(&mut stream, "503 Service Unavailable", &body.to_string()).await?;
        }
    }
    Ok(())
}

/// Run the HTTP accept loop against an already-bound [`TcpListener`]. Each
/// connection is handled on its own task; a per-connection failure (parse
/// error, client disconnect mid-write, …) is logged and never brings down the
/// listener. Returns only on a fatal `accept()` error.
pub async fn run(listener: TcpListener, socket_path: PathBuf) -> Result<()> {
    let local_addr = listener.local_addr().ok();
    log::info!(
        "serve: listening on {} (proxying {})",
        local_addr.map_or_else(|| "?".to_string(), |a| a.to_string()),
        socket_path.display()
    );
    loop {
        let (stream, peer) = listener.accept().await?;
        let socket_path = socket_path.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, socket_path).await {
                log::debug!("serve: connection from {peer} ended with error: {e}");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{CapacityReport, DaemonStatusReport};
    use std::net::Ipv4Addr;
    use tokio::io::AsyncReadExt;
    use tokio::net::UnixListener;

    /// A minimal but structurally complete [`DaemonStatusReport`] for tests —
    /// the zero-workspaces / zero-in-flight case, matching what a freshly
    /// started daemon with no registered repos reports.
    fn empty_report() -> DaemonStatusReport {
        DaemonStatusReport {
            in_flight: vec![],
            unregistered_locked: vec![],
            token_pool_size: 0,
            token_pool_dir: None,
            disk_headroom: 0,
            cpu_headroom: 0,
            logical_cpus: 0,
            loadavg_1m: None,
            cpu_idle_fraction: None,
            capacity_bound: false,
            configured_max: 0,
            per_token_concurrency: 1,
            dynamic_cap: 0,
            main_health_gate_halted: false,
            main_health_gate_not_evaluated: false,
            main_health_gate_not_evaluated_reason: None,
            main_health_gate_enabled: None,
            main_health_gate_verdict_at: None,
            capacity: CapacityReport {
                ranking_present: false,
                total_accounts: 0,
                healthy_accounts: 0,
                exhausted_accounts: 0,
                token_axis_limit: 0,
                token_bound: false,
            },
            per_repo: vec![],
            credential_preflight: None,
            draining: false,
            drain_deadline: None,
            drain_note: None,
            auto_update_enabled: false,
            auto_update_last_check: None,
            auto_update_last_roll: None,
            auto_update_consecutive_failures: 0,
            auto_update_backoff_secs: None,
            auto_update_terminal_reason: None,
            auto_update_note: None,
            host_breaker: None,
            safehouse: None,
        }
    }

    // ===== Security: bind validation =====

    #[test]
    fn validate_bind_allows_loopback_v4_without_opt_in() {
        assert!(validate_bind(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), false).is_ok());
    }

    #[test]
    fn validate_bind_allows_loopback_v6_without_opt_in() {
        assert!(validate_bind(IpAddr::V6(std::net::Ipv6Addr::LOCALHOST), false).is_ok());
    }

    #[test]
    fn validate_bind_refuses_non_loopback_without_opt_in() {
        let addr = IpAddr::V4(Ipv4Addr::new(100, 64, 1, 2)); // tailnet-range example
        let err = validate_bind(addr, false).expect_err("must refuse without opt-in");
        assert!(err.contains("--allow-non-loopback"), "error should name the opt-in flag: {err}");
    }

    #[test]
    fn validate_bind_allows_non_loopback_with_opt_in() {
        let addr = IpAddr::V4(Ipv4Addr::new(100, 64, 1, 2));
        assert!(validate_bind(addr, true).is_ok());
    }

    #[test]
    fn validate_bind_refuses_wildcard_v4_even_with_opt_in() {
        let addr = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
        let err = validate_bind(addr, true).expect_err("wildcard must never be allowed");
        assert!(err.contains("0.0.0.0"), "error should call out the wildcard bind: {err}");
    }

    #[test]
    fn validate_bind_refuses_wildcard_v6_even_with_opt_in() {
        let addr = IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED);
        assert!(validate_bind(addr, true).is_err());
    }

    // ===== Unit: JSON snapshot shape =====

    #[test]
    fn build_snapshot_includes_hostname_and_flattens_report_fields() {
        let mut report = empty_report();
        report.token_pool_size = 7;
        let snapshot = build_snapshot(report);
        let value = serde_json::to_value(&snapshot).expect("serialize");
        let obj = value.as_object().expect("object");
        assert!(
            obj.contains_key("hostname"),
            "snapshot JSON must carry a host-identity field: {obj:?}"
        );
        assert_eq!(obj.get("token_pool_size"), Some(&serde_json::json!(7)));
        // Flattened, not nested under a "report" key.
        assert!(!obj.contains_key("report"));
    }

    #[test]
    fn build_snapshot_round_trips_zero_workspaces_edge_case() {
        let snapshot = build_snapshot(empty_report());
        let json = serde_json::to_string(&snapshot).expect("serialize");
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        assert_eq!(value["per_repo"], serde_json::json!([]));
        assert_eq!(value["in_flight"], serde_json::json!([]));
        assert_eq!(value["token_pool_size"], serde_json::json!(0));
    }

    /// Spin up a fake daemon Unix socket that answers exactly one
    /// `DaemonStatus` request with a canned report, mirroring the fake-socket
    /// pattern already used by `main.rs`'s `dispatch_tests` module.
    async fn spawn_fake_daemon_socket(report: DaemonStatusReport) -> PathBuf {
        let dir = tempfile::tempdir().expect("tempdir");
        // Leak the tempdir so it outlives the spawned task (tests are
        // short-lived processes; this is the same tradeoff `dispatch_tests`
        // makes for its fake sockets).
        let socket_path = dir.path().join("fake-daemon.sock");
        std::mem::forget(dir);
        let listener = UnixListener::bind(&socket_path).expect("bind fake socket");
        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let (reader, mut writer) = stream.into_split();
                let mut lines = BufReader::new(reader).lines();
                if let Ok(Some(line)) = lines.next_line().await {
                    let _req: Request = serde_json::from_str(&line).expect("valid request");
                    let response = Response::DaemonStatus(Box::new(report));
                    let response_json = serde_json::to_string(&response).expect("serialize");
                    let _ = writer.write_all(response_json.as_bytes()).await;
                    let _ = writer.write_all(b"\n").await;
                    let _ = writer.flush().await;
                }
            }
        });
        // Give the listener a moment to be ready to accept.
        tokio::time::sleep(Duration::from_millis(20)).await;
        socket_path
    }

    // ===== Integration: fetch_snapshot over a fake daemon socket =====

    #[tokio::test]
    async fn fetch_snapshot_returns_report_from_fake_daemon() {
        let mut report = empty_report();
        report.token_pool_size = 3;
        report.dynamic_cap = 2;
        let socket_path = spawn_fake_daemon_socket(report).await;

        let snapshot = fetch_snapshot(&socket_path).await.expect("fetch snapshot");
        assert_eq!(snapshot.report.token_pool_size, 3);
        assert_eq!(snapshot.report.dynamic_cap, 2);
        assert!(!snapshot.hostname.is_empty());
    }

    #[tokio::test]
    async fn fetch_snapshot_errors_when_daemon_unreachable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("nonexistent.sock");
        let result = fetch_snapshot(&socket_path).await;
        assert!(result.is_err(), "must error when nothing is listening");
    }

    // ===== Integration: full HTTP round-trip on an ephemeral port =====

    async fn http_get(addr: std::net::SocketAddr, path: &str) -> (u16, String) {
        let mut stream = TcpStream::connect(addr).await.expect("connect");
        let request = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
        stream
            .write_all(request.as_bytes())
            .await
            .expect("write request");
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.expect("read response");
        let text = String::from_utf8_lossy(&buf).to_string();
        let status_line = text.lines().next().unwrap_or_default();
        let status_code: u16 = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let body = text
            .split("\r\n\r\n")
            .nth(1)
            .unwrap_or_default()
            .to_string();
        (status_code, body)
    }

    #[tokio::test]
    async fn http_get_status_endpoint_returns_valid_json() {
        let mut report = empty_report();
        report.token_pool_size = 5;
        let socket_path = spawn_fake_daemon_socket(report).await;

        let tcp_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind tcp");
        let addr = tcp_listener.local_addr().expect("local addr");
        // Loopback-only default bind — a live security assertion, not just a
        // config default: the listener this test exercises is bound
        // explicitly to `127.0.0.1`.
        assert!(addr.ip().is_loopback());

        let server_task = tokio::spawn(run(tcp_listener, socket_path));

        let (status, body) = http_get(addr, "/api/status").await;
        assert_eq!(status, 200);
        let value: serde_json::Value = serde_json::from_str(&body).expect("valid json body");
        assert_eq!(value["token_pool_size"], serde_json::json!(5));
        assert!(value.get("hostname").is_some());

        server_task.abort();
    }

    #[tokio::test]
    async fn http_get_unknown_path_returns_404() {
        let socket_path = spawn_fake_daemon_socket(empty_report()).await;
        let tcp_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind tcp");
        let addr = tcp_listener.local_addr().expect("local addr");
        let server_task = tokio::spawn(run(tcp_listener, socket_path));

        let (status, _body) = http_get(addr, "/nope").await;
        assert_eq!(status, 404);

        server_task.abort();
    }

    /// Statelessness (#4391 test plan): killing the serve task leaves no trace
    /// on disk — this listener never creates a persistent store. We only
    /// assert the module-level invariant (no file I/O anywhere in this
    /// module's request path); there is no daemon-state file for `serve`
    /// itself to touch, unlike the sweep registry / activity db.
    #[tokio::test]
    async fn serve_task_abort_leaves_no_files_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let before: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .filter_map(std::result::Result::ok)
            .collect();
        assert!(before.is_empty(), "sanity: tempdir starts empty");

        let socket_path = spawn_fake_daemon_socket(empty_report()).await;
        let tcp_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind tcp");
        let addr = tcp_listener.local_addr().expect("local addr");
        let server_task = tokio::spawn(run(tcp_listener, socket_path));
        let _ = http_get(addr, "/api/status").await;
        server_task.abort();

        let after: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .filter_map(std::result::Result::ok)
            .collect();
        assert!(
            after.is_empty(),
            "serve must never write files under an unrelated scratch dir: {after:?}"
        );
    }
}
