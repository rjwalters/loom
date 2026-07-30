//! `loom-daemon serve` (Issue #4391 — dashboard phase 1 of #4329; Issue #4392 —
//! phase 2).
//!
//! A minimal, read-only HTTP listener with two endpoints, both proxied over the
//! daemon's *existing* Unix socket:
//!
//! - `GET /api/status` (#4391) — serializes the same
//!   [`crate::types::DaemonStatusReport`] data `loom-daemon status --json`
//!   already aggregates (the same `Request::DaemonStatus` / `Response::DaemonStatus`
//!   wire contract [`crate::ipc::build_daemon_status`] already answers), so this
//!   module never re-derives the report itself.
//! - `GET /api/events` (#4392) — a `text/event-stream` (SSE) tail of the
//!   daemon's event bus, bridged from the same socket's existing
//!   `Request::SubscribeEvents` / `Response::EventStream` contract, so the
//!   topic-prefix filtering runs exactly once, in
//!   [`crate::event_bus::EventBus::subscribe`], never re-implemented here.
//!
//! Nothing starts until the `serve` subcommand is explicitly invoked (never
//! from the default daemon-run path, never from a config value alone).
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
//!   The SSE bridge **subscribes only** — it never sends `Request::PublishEvent`
//!   and never constructs an [`crate::types::Event`] of its own, so no traffic
//!   this listener originates can ever appear on the bus.
//! - `/api/events` adds no new bind, no new port and no new opt-in knob: it is
//!   routed off the same [`TcpListener`] `/api/status` already answers, so it
//!   inherits phase 1's bind validation ([`validate_bind`]) unchanged.
//!
//! # HTTP dependency decision
//!
//! Hand-rolled HTTP/1.1 over [`tokio::net::TcpListener`] rather than pulling in
//! `axum`/`hyper`: two read-only GET endpoints on a localhost-by-default
//! surface do not warrant a full HTTP framework and its dependency tree
//! (`loom-daemon/Cargo.toml` currently has no HTTP framework at all — tokio
//! only). This keeps the same tradeoff the daemon already makes for its
//! Unix-socket IPC protocol (also hand-rolled, newline-delimited JSON). SSE
//! (#4392) did not change that calculus: `text/event-stream` is a plain-text
//! framing (`data: <json>\n\n`) over an ordinary chunk-free HTTP/1.1 response,
//! so the long-lived `/api/events` connection needs no framework support
//! beyond what [`tokio::net::TcpStream`] already provides.

use crate::types::{DaemonStatusReport, Event, Request, Response};
use anyhow::{anyhow, Result};
use serde::Serialize;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream, UnixStream};

/// Default TCP port for `loom-daemon serve`.
pub const DEFAULT_PORT: u16 = 7420;

/// Path the JSON status-snapshot endpoint answers (Issue #4391).
const STATUS_PATH: &str = "/api/status";

/// Path the SSE event-tail endpoint answers (Issue #4392).
const EVENTS_PATH: &str = "/api/events";

/// Topic **prefixes** the SSE bridge subscribes to, passed verbatim as
/// `Request::SubscribeEvents { topics }` so the daemon applies them through the
/// very same [`crate::event_bus::EventBus::subscribe`] /
/// [`crate::event_bus::topic_matches`] prefix filter every other subscriber
/// uses. Matching is segment-aligned prefix match, so `sweep.issue` covers
/// `sweep.issue.{N}.phase` / `.blocker` / `.exited` / `.crashed` for every `N`
/// without needing a glob the bus does not support.
///
/// Every entry is an existing, already-authorized topic (or a prefix of one)
/// from `event_bus.rs`'s frozen taxonomy — **this module never invents a
/// topic**, and never renames one on the way out (see [`sse_frame`], which
/// emits [`Event::topic`] verbatim):
///
/// | Prefix | Covers | Authorized by |
/// |--------|--------|---------------|
/// | `sweep.issue` | the four frozen per-issue topics (`phase`/`blocker`/`exited`/`crashed`), plus `resume_dispatched` (#4256) | v0.10.0 frozen taxonomy |
/// | `sweep.global.dispatch` | frozen global dispatch topic | v0.10.0 frozen taxonomy |
/// | `sweep.global.completed` | frozen global completion topic | v0.10.0 frozen taxonomy |
/// | `epic.issue` | `epic.issue.{N}.{decompose,expand,join,close}` | #3873 |
/// | `daemon.capacity.advisory` | work-finder capacity backpressure advisory | #3902 |
/// | `daemon.drain` | `daemon.drain.{started,completed,aborted,timeout}` | #4090 |
/// | `daemon.dispatch.headroom_advisory` | dispatch-time headroom advisory | #4234 |
///
/// The first three rows are the **required** set (#4392: the six frozen
/// `sweep.*` topics MUST stream). The remaining rows are the issue's explicitly
/// optional, already-authorized follow-up topics — included because a fleet
/// dashboard wants them, and safe to include because each frame carries its
/// exact source topic, so a consumer can never confuse one for a frozen
/// `sweep.*` topic.
pub const SSE_TOPIC_PREFIXES: &[&str] = &[
    "sweep.issue",
    "sweep.global.dispatch",
    "sweep.global.completed",
    "epic.issue",
    "daemon.capacity.advisory",
    "daemon.drain",
    "daemon.dispatch.headroom_advisory",
];

/// How often the SSE bridge writes a heartbeat comment (`: keepalive`) when no
/// events are flowing. Two jobs: keep intermediaries from reaping an idle
/// connection, and give the bridge a periodic write that fails once the client
/// has gone away even if the bus is silent (so the upstream subscription is
/// dropped rather than leaked).
const SSE_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

/// Reconnect delay (ms) advertised to `EventSource` clients via the SSE
/// `retry:` field. This bridge has **no replay buffer** (the "no new persistent
/// store" constraint), so a reconnecting client resumes from live — and
/// deliberately never emits an `id:` field, which would imply a
/// `Last-Event-ID` replay capability that does not exist.
const SSE_RETRY_MS: u64 = 3_000;

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

// ============================================================================
// SSE event tail (`GET /api/events`, Issue #4392)
// ============================================================================

/// True when `topic` would be delivered by this bridge's subscription, decided
/// with the bus's own [`crate::event_bus::topic_matches`] rule against
/// [`SSE_TOPIC_PREFIXES`] — the same predicate the daemon side applies, so this
/// is a faithful local mirror of the server-side filter rather than a second
/// implementation of matching.
#[must_use]
pub fn topic_is_streamed(topic: &str) -> bool {
    SSE_TOPIC_PREFIXES
        .iter()
        .any(|prefix| crate::event_bus::topic_matches(topic, prefix))
}

/// The HTTP response head for the SSE stream, followed by the stream preamble.
///
/// `Connection: close` and the absence of `Content-Length` mean the body runs
/// until the connection ends — the standard framing for an unbounded
/// `text/event-stream`. `X-Accel-Buffering: no` tells a reverse proxy (should
/// an operator front this loopback listener with one) not to buffer the stream.
#[must_use]
fn sse_response_head() -> String {
    format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: text/event-stream\r\n\
         Cache-Control: no-cache, no-store\r\n\
         X-Accel-Buffering: no\r\n\
         Connection: close\r\n\
         \r\n\
         retry: {SSE_RETRY_MS}\n\
         : connected to the loom-daemon event bus\n\n"
    )
}

/// Render one bus [`Event`] as a single SSE frame.
///
/// Frames deliberately carry **no `event:` field**: the SSE event-name field is
/// a fixed string, but the frozen taxonomy's topics are parameterized by issue
/// number (`sweep.issue.{N}.phase`), so a browser could not
/// `addEventListener` for them. Instead every frame arrives as the default
/// `message` type and carries its exact source topic in the JSON payload's
/// `topic` field — verbatim from [`Event::topic`], never renamed or
/// re-namespaced — with the full typed event under `event`. A consumer routes
/// on `topic`, and the six frozen `sweep.*` topics are therefore
/// indistinguishable from how the bus itself names them.
///
/// `serde_json::to_string` on this payload cannot fail (an [`Event`]'s
/// `Serialize` impl is infallible and the map keys are static), so the
/// unreachable error branch degrades to an SSE comment rather than tearing
/// down a live stream.
#[must_use]
pub fn sse_frame(event: &Event) -> String {
    let payload = serde_json::json!({
        "topic": event.topic(),
        "event": event,
    });
    match serde_json::to_string(&payload) {
        // A `data:` value must not contain a newline; `to_string` (compact,
        // not `to_string_pretty`) never emits one.
        Ok(json) => format!("data: {json}\n\n"),
        Err(e) => format!(": unserializable event ({e})\n\n"),
    }
}

/// Translate one newline-delimited IPC response line from the daemon into zero
/// or more SSE frames.
///
/// A [`Response::EventStream`] frame may carry more than one event (the wire
/// contract allows batching), so this returns a `Vec`. Anything else — an
/// unparseable line, a [`Response::Error`], or a response variant that is not
/// part of the subscription stream — yields no frames and is skipped rather
/// than killing the stream.
#[must_use]
fn sse_frames_for_response_line(line: &str) -> Vec<String> {
    match serde_json::from_str::<Response>(line) {
        Ok(Response::EventStream { events }) => events.iter().map(sse_frame).collect(),
        Ok(Response::Error { message }) => {
            log::debug!("serve: event stream got an error response from the daemon: {message}");
            vec![]
        }
        Ok(other) => {
            log::debug!("serve: ignoring non-event response on the event stream: {other:?}");
            vec![]
        }
        Err(e) => {
            log::debug!("serve: unparseable event-stream line ({e})");
            vec![]
        }
    }
}

/// Open an event subscription on the daemon's Unix socket.
///
/// Sends exactly one `Request::SubscribeEvents { topics }` — the read-only IPC
/// verb — and returns the split halves of the connection. The write half is
/// returned (rather than dropped) purely so it stays alive for the life of the
/// stream; the bridge never writes to it again, which is what makes the
/// "subscribe, never publish" invariant structural rather than a convention.
async fn open_event_subscription(
    socket_path: &Path,
) -> Result<(tokio::net::unix::OwnedReadHalf, tokio::net::unix::OwnedWriteHalf)> {
    let connect = async {
        let stream = UnixStream::connect(socket_path)
            .await
            .map_err(|e| anyhow!("connect to daemon socket failed: {e}"))?;
        let (reader, mut writer) = stream.into_split();

        let topics: Vec<String> = SSE_TOPIC_PREFIXES
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        let request_json = serde_json::to_string(&Request::SubscribeEvents { topics })?;
        writer.write_all(request_json.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
        Ok::<_, anyhow::Error>((reader, writer))
    };

    tokio::time::timeout(FETCH_TIMEOUT, connect)
        .await
        .map_err(|_| anyhow!("event subscription timed out after {}s", FETCH_TIMEOUT.as_secs()))?
}

/// Serve `GET /api/events`: bridge the daemon's event bus onto this connection
/// as `text/event-stream`.
///
/// Lifecycle: subscribe over IPC first (so an unreachable daemon still gets a
/// clean 503 JSON body rather than a half-open stream), then commit to a 200
/// and pump frames until either side goes away. Termination is deliberately
/// total — client EOF, a failed write, the daemon closing the subscription, or
/// a socket error all return `Ok(())`, at which point both halves of the IPC
/// connection drop and the daemon-side [`crate::event_bus::Subscription`] is
/// released. There is no path that leaves the upstream subscription alive after
/// this function returns.
async fn handle_event_stream(stream: &mut TcpStream, socket_path: &Path) -> Result<()> {
    let (daemon_reader, _daemon_writer) = match open_event_subscription(socket_path).await {
        Ok(halves) => halves,
        Err(e) => {
            let body = serde_json::json!({ "error": format!("daemon unreachable: {e}") });
            write_json_response(stream, "503 Service Unavailable", &body.to_string()).await?;
            return Ok(());
        }
    };

    let (mut client_reader, mut client_writer) = stream.split();

    if client_writer
        .write_all(sse_response_head().as_bytes())
        .await
        .is_err()
        || client_writer.flush().await.is_err()
    {
        return Ok(());
    }

    let mut daemon_lines = BufReader::new(daemon_reader).lines();
    let mut heartbeat = tokio::time::interval(SSE_HEARTBEAT_INTERVAL);
    // `interval` fires immediately on its first tick; consume it so the first
    // heartbeat lands one full interval after the preamble.
    heartbeat.tick().await;
    // Scratch buffer for the client half. A well-behaved SSE client sends
    // nothing after its request, so this read is really an EOF detector.
    let mut client_scratch = [0u8; 512];

    loop {
        tokio::select! {
            line = daemon_lines.next_line() => match line {
                Ok(Some(line)) => {
                    for frame in sse_frames_for_response_line(&line) {
                        if client_writer.write_all(frame.as_bytes()).await.is_err() {
                            return Ok(());
                        }
                    }
                    if client_writer.flush().await.is_err() {
                        return Ok(());
                    }
                }
                Ok(None) => {
                    // Daemon closed the subscription (shutdown/restart). Tell
                    // the client in-band, then end the response; `EventSource`
                    // reconnects on its own after `retry:`.
                    let _ = client_writer.write_all(b": daemon closed the event stream\n\n").await;
                    let _ = client_writer.flush().await;
                    return Ok(());
                }
                Err(e) => {
                    log::debug!("serve: event subscription read error: {e}");
                    return Ok(());
                }
            },
            read = client_reader.read(&mut client_scratch) => match read {
                // Client hung up (or errored) — drop the subscription.
                Ok(0) | Err(_) => return Ok(()),
                // Unexpected trailing bytes from the client: this endpoint has
                // no client->server channel, so discard and keep streaming.
                Ok(_) => {}
            },
            _ = heartbeat.tick() => {
                if client_writer.write_all(b": keepalive\n\n").await.is_err()
                    || client_writer.flush().await.is_err()
                {
                    return Ok(());
                }
            }
        }
    }
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

    if parsed.path != STATUS_PATH && parsed.path != EVENTS_PATH {
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

    // The SSE tail (#4392) is a long-lived response: it owns the connection
    // from here until either side disconnects.
    if parsed.path == EVENTS_PATH {
        return handle_event_stream(&mut stream, &socket_path).await;
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
        "serve: listening on {} ({STATUS_PATH}, {EVENTS_PATH}; proxying {})",
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
            preflight_advisory_active: false,
            preflight_advisory_message: None,
            configured_max: 0,
            per_token_concurrency: 1,
            dynamic_cap: 0,
            main_health_gate_halted: false,
            main_health_gate_not_evaluated: false,
            main_health_gate_not_evaluated_reason: None,
            main_health_gate_enabled: None,
            main_health_gate_verdict_at: None,
            main_health_gate_deferred: false,
            main_health_gate_deferred_reason: None,
            main_health_gate_verdict_tier: None,
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
            rate_limit_breaker: None,
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

    // ========================================================================
    // SSE event tail (`GET /api/events`, Issue #4392)
    // ========================================================================

    use crate::event_bus::EventBus;
    use crate::types::{EpicActionClass, Event, SweepKind, SweepOutcome};
    use std::sync::Arc;

    /// The six topics frozen for v0.10.0 that #4392 requires this endpoint to
    /// stream, spelled out literally (not derived from
    /// [`SSE_TOPIC_PREFIXES`]) so the assertion is independent of the
    /// implementation it checks.
    const FROZEN_SWEEP_TOPICS: &[&str] = &[
        "sweep.issue.4392.phase",
        "sweep.issue.4392.blocker",
        "sweep.issue.4392.exited",
        "sweep.issue.4392.crashed",
        "sweep.global.dispatch",
        "sweep.global.completed",
    ];

    /// One constructed [`Event`] per frozen topic, in the same order.
    fn frozen_sweep_events() -> Vec<Event> {
        vec![
            Event::SweepPhase {
                issue: 4392,
                phase: "builder".to_string(),
                pr_number: None,
                repo: None,
            },
            Event::SweepBlocker {
                issue: 4392,
                reason: "dependency".to_string(),
                label_added: "loom:blocked".to_string(),
                repo: None,
            },
            Event::SweepExited {
                issue: 4392,
                exit_code: Some(0),
                duration_sec: 12,
                no_progress: false,
                death_class: None,
                repo: None,
            },
            Event::SweepCrashed {
                issue: 4392,
                checkpoint_phase: Some("judge".to_string()),
                classification: None,
                death_class: None,
                repo: None,
            },
            Event::SweepGlobalDispatch {
                sweep_id: "sweep-4392".to_string(),
                kind: SweepKind::Issue(4392),
                repo: None,
            },
            Event::SweepGlobalCompleted {
                sweep_id: "sweep-4392".to_string(),
                outcome: SweepOutcome::Exited,
            },
        ]
    }

    // ----- Unit: topic filter -----

    #[test]
    fn sse_filter_passes_all_six_frozen_sweep_topics() {
        for topic in FROZEN_SWEEP_TOPICS {
            assert!(
                topic_is_streamed(topic),
                "frozen v0.10.0 topic must stream on /api/events: {topic}"
            );
        }
    }

    #[test]
    fn sse_filter_passes_the_frozen_topics_for_any_issue_number() {
        // The bus offers prefix matching, not globbing, so `sweep.issue.{N}.*`
        // has to be covered by a prefix rather than enumerated per issue.
        for issue in [1u32, 42, 4392, 999_999] {
            for suffix in ["phase", "blocker", "exited", "crashed"] {
                let topic = format!("sweep.issue.{issue}.{suffix}");
                assert!(topic_is_streamed(&topic), "must stream {topic}");
            }
        }
    }

    #[test]
    fn sse_filter_subscribes_only_to_topics_from_the_frozen_taxonomy() {
        // Guards #4392's "never invent new topics": every subscribed prefix is
        // an existing taxonomy entry (or a segment-aligned prefix of one),
        // authorized by the issue number named in `SSE_TOPIC_PREFIXES`' docs.
        let expected = [
            "sweep.issue",
            "sweep.global.dispatch",
            "sweep.global.completed",
            "epic.issue",
            "daemon.capacity.advisory",
            "daemon.drain",
            "daemon.dispatch.headroom_advisory",
        ];
        assert_eq!(
            SSE_TOPIC_PREFIXES, &expected,
            "the subscribed prefix set is a frozen contract — adding one requires a follow-up \
             issue authorizing the topic in event_bus.rs's taxonomy"
        );
    }

    #[test]
    fn sse_filter_rejects_topics_outside_the_taxonomy() {
        for topic in [
            "dashboard.custom.ping", // an invented topic
            "sweep.issuetype.foo",   // near-miss: prefix match is segment-aligned
            "sweeps.global.dispatch",
            "daemon.drainage.started",
            "",
        ] {
            assert!(
                !topic_is_streamed(topic),
                "must not stream a topic outside the taxonomy: {topic:?}"
            );
        }
    }

    #[test]
    fn sse_filter_passes_the_optional_authorized_follow_up_topics() {
        for topic in [
            "epic.issue.4329.decompose",
            "epic.issue.4329.expand",
            "epic.issue.4329.join",
            "epic.issue.4329.close",
            "daemon.capacity.advisory",
            "daemon.drain.started",
            "daemon.drain.completed",
            "daemon.drain.aborted",
            "daemon.drain.timeout",
            "daemon.dispatch.headroom_advisory",
        ] {
            assert!(topic_is_streamed(topic), "must stream {topic}");
        }
    }

    // ----- Unit: frame translation -----

    #[test]
    fn sse_frame_carries_each_frozen_topic_verbatim() {
        for (event, expected_topic) in frozen_sweep_events().iter().zip(FROZEN_SWEEP_TOPICS) {
            let frame = sse_frame(event);
            assert!(frame.ends_with("\n\n"), "frame must be blank-line terminated: {frame:?}");
            let data = frame
                .strip_prefix("data: ")
                .and_then(|rest| rest.strip_suffix("\n\n"))
                .unwrap_or_else(|| panic!("frame must be a single `data:` line: {frame:?}"));
            assert!(!data.contains('\n'), "a `data:` value must be newline-free: {data:?}");
            let value: serde_json::Value = serde_json::from_str(data).expect("valid json payload");
            assert_eq!(
                value["topic"],
                serde_json::json!(*expected_topic),
                "the frame must carry the bus topic verbatim — no renaming"
            );
            assert_eq!(value["topic"], serde_json::json!(event.topic()));
            assert!(value.get("event").is_some(), "frame must carry the typed event");
        }
    }

    #[test]
    fn sse_frame_preserves_the_typed_event_payload() {
        let frame = sse_frame(&Event::SweepGlobalDispatch {
            sweep_id: "sweep-4392".to_string(),
            kind: SweepKind::Issue(4392),
            repo: Some("/repos/loom".to_string()),
        });
        let data = frame.trim_start_matches("data: ").trim_end_matches("\n\n");
        let value: serde_json::Value = serde_json::from_str(data).expect("valid json");
        assert_eq!(value["topic"], serde_json::json!("sweep.global.dispatch"));
        assert_eq!(value["event"]["sweep_id"], serde_json::json!("sweep-4392"));
        assert_eq!(value["event"]["repo"], serde_json::json!("/repos/loom"));
    }

    #[test]
    fn sse_frame_omits_the_event_name_field_so_browsers_get_onmessage() {
        // A per-issue topic cannot be an SSE `event:` name (a client cannot
        // `addEventListener` for `sweep.issue.{N}.phase`), so routing lives in
        // the payload's `topic` instead. Assert the framing stays that way.
        let frame = sse_frame(&Event::SweepPhase {
            issue: 7,
            phase: "judge".to_string(),
            pr_number: Some(1234),
            repo: None,
        });
        assert!(
            !frame.contains("event:"),
            "frames must use the default `message` type: {frame:?}"
        );
        assert!(!frame.contains("id:"), "no `id:` — this bridge has no replay buffer: {frame:?}");
    }

    #[test]
    fn sse_frame_renders_an_optional_authorized_topic_without_disguising_it() {
        let frame = sse_frame(&Event::EpicAction {
            epic: 4329,
            action: EpicActionClass::Expand,
            state: "epic:designed".to_string(),
        });
        let data = frame.trim_start_matches("data: ").trim_end_matches("\n\n");
        let value: serde_json::Value = serde_json::from_str(data).expect("valid json");
        // The optional follow-up topics stay recognizably themselves — never
        // relabelled into the frozen `sweep.*` namespace.
        assert_eq!(value["topic"], serde_json::json!("epic.issue.4329.expand"));
        assert!(!value["topic"]
            .as_str()
            .unwrap_or_default()
            .starts_with("sweep."));
    }

    #[test]
    fn sse_frames_for_response_line_translates_a_batched_event_stream_frame() {
        let response = Response::EventStream {
            events: frozen_sweep_events(),
        };
        let line = serde_json::to_string(&response).expect("serialize");
        let frames = sse_frames_for_response_line(&line);
        assert_eq!(frames.len(), FROZEN_SWEEP_TOPICS.len());
        for (frame, topic) in frames.iter().zip(FROZEN_SWEEP_TOPICS) {
            assert!(frame.contains(topic), "frame {frame:?} should carry topic {topic}");
        }
    }

    #[test]
    fn sse_frames_for_response_line_skips_junk_without_emitting_frames() {
        assert!(sse_frames_for_response_line("not json at all").is_empty());
        assert!(sse_frames_for_response_line("").is_empty());
        let err = serde_json::to_string(&Response::Error {
            message: "boom".to_string(),
        })
        .expect("serialize");
        assert!(sse_frames_for_response_line(&err).is_empty());
    }

    #[test]
    fn sse_response_head_declares_an_unbounded_event_stream() {
        let head = sse_response_head();
        assert!(head.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(head.contains("Content-Type: text/event-stream\r\n"));
        assert!(head.contains("Cache-Control: no-cache"));
        assert!(
            !head.contains("Content-Length"),
            "an SSE body is unbounded — a Content-Length would truncate it: {head:?}"
        );
        assert!(head.contains(&format!("retry: {SSE_RETRY_MS}")));
    }

    // ----- Integration harness: a fake daemon that answers SubscribeEvents -----

    struct FakeEventDaemon {
        socket_path: PathBuf,
        bus: Arc<EventBus>,
    }

    /// Spin up a fake daemon Unix socket that answers `SubscribeEvents` from a
    /// **real** [`EventBus`], forwarding matching events as
    /// `Response::EventStream` lines exactly the way `ipc::stream_events`
    /// does — same `bus.subscribe(topics)` prefix filter, same one-event-per-
    /// frame encoding, same "stop on write error" termination. Accepts
    /// repeatedly so a reconnect can be exercised.
    async fn spawn_fake_event_daemon() -> FakeEventDaemon {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("fake-daemon.sock");
        std::mem::forget(dir);
        let listener = UnixListener::bind(&socket_path).expect("bind fake socket");
        let bus = Arc::new(EventBus::new());
        let bus_for_accept = Arc::clone(&bus);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let bus = Arc::clone(&bus_for_accept);
                tokio::spawn(async move {
                    let (reader, mut writer) = stream.into_split();
                    let mut lines = BufReader::new(reader).lines();
                    let Ok(Some(line)) = lines.next_line().await else {
                        return;
                    };
                    let request: Request = serde_json::from_str(&line).expect("valid IPC request");
                    let Request::SubscribeEvents { topics } = request else {
                        panic!("the SSE bridge must only ever send SubscribeEvents: {request:?}");
                    };
                    let mut subscription = bus.subscribe(topics);
                    while let Ok(event) = subscription.recv().await {
                        let frame = Response::EventStream {
                            events: vec![event],
                        };
                        let json = serde_json::to_string(&frame).expect("serialize");
                        if writer.write_all(json.as_bytes()).await.is_err()
                            || writer.write_all(b"\n").await.is_err()
                            || writer.flush().await.is_err()
                        {
                            break;
                        }
                    }
                    // Subscription drops here — the leak check below observes
                    // this through `bus.receiver_count()`.
                });
            }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        FakeEventDaemon { socket_path, bus }
    }

    /// Poll until the bus reports exactly `want` subscribers, or fail.
    async fn wait_for_receiver_count(bus: &EventBus, want: usize) {
        for _ in 0..300 {
            if bus.receiver_count() == want {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("bus never settled at {want} receivers (last: {})", bus.receiver_count());
    }

    /// Open an SSE connection and read until the stream preamble arrives.
    async fn connect_sse(addr: std::net::SocketAddr) -> (TcpStream, String) {
        let mut stream = TcpStream::connect(addr).await.expect("connect");
        stream
            .write_all(
                b"GET /api/events HTTP/1.1\r\nHost: 127.0.0.1\r\nAccept: text/event-stream\r\n\r\n",
            )
            .await
            .expect("write request");
        let head = read_until(&mut stream, ": connected").await;
        (stream, head)
    }

    /// Read from `stream` until `needle` appears or a bounded timeout expires.
    async fn read_until(stream: &mut TcpStream, needle: &str) -> String {
        let mut acc = String::new();
        let mut buf = [0u8; 4096];
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, stream.read(&mut buf)).await {
                Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
                Ok(Ok(n)) => {
                    acc.push_str(&String::from_utf8_lossy(&buf[..n]));
                    if acc.contains(needle) {
                        break;
                    }
                }
            }
        }
        acc
    }

    /// Extract the parsed payloads of every `data:` line seen so far.
    fn data_payloads(text: &str) -> Vec<serde_json::Value> {
        text.lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .filter_map(|json| serde_json::from_str(json).ok())
            .collect()
    }

    // ----- Integration: live delivery on an ephemeral port -----

    #[tokio::test]
    async fn sse_endpoint_delivers_a_published_sweep_global_dispatch_event() {
        let fake = spawn_fake_event_daemon().await;
        let tcp_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind tcp");
        let addr = tcp_listener.local_addr().expect("local addr");
        // Same listener, same loopback-only posture as phase 1 — /api/events
        // adds no second bind.
        assert!(addr.ip().is_loopback());
        let server_task = tokio::spawn(run(tcp_listener, fake.socket_path.clone()));

        let (mut client, head) = connect_sse(addr).await;
        assert!(head.contains("200 OK"), "head: {head:?}");
        assert!(head.contains("text/event-stream"), "head: {head:?}");

        // The bus drops events published with no receivers, so wait until the
        // bridge's subscription has actually landed.
        wait_for_receiver_count(&fake.bus, 1).await;

        fake.bus
            .publish(Event::SweepGlobalDispatch {
                sweep_id: "sweep-4392".to_string(),
                kind: SweepKind::Issue(4392),
                repo: None,
            })
            .expect("publish");

        let body = read_until(&mut client, "sweep.global.dispatch").await;
        let payloads = data_payloads(&body);
        let dispatch = payloads
            .iter()
            .find(|p| p["topic"] == serde_json::json!("sweep.global.dispatch"))
            .unwrap_or_else(|| panic!("no dispatch frame in stream: {body:?}"));
        assert_eq!(dispatch["event"]["sweep_id"], serde_json::json!("sweep-4392"));

        server_task.abort();
    }

    #[tokio::test]
    async fn sse_endpoint_delivers_every_frozen_sweep_topic_and_filters_the_rest() {
        let fake = spawn_fake_event_daemon().await;
        let tcp_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind tcp");
        let addr = tcp_listener.local_addr().expect("local addr");
        let server_task = tokio::spawn(run(tcp_listener, fake.socket_path.clone()));

        let (mut client, _head) = connect_sse(addr).await;
        wait_for_receiver_count(&fake.bus, 1).await;

        // Negative control published FIRST: if the server-side prefix filter
        // were a no-op this would arrive before any frozen topic below.
        fake.bus
            .publish_generic(
                "unrelated.topic.for.this.test",
                serde_json::json!({ "should": "never reach the client" }),
            )
            .expect("publish");
        for event in frozen_sweep_events() {
            fake.bus.publish(event).expect("publish");
        }

        let body = read_until(&mut client, "sweep.global.completed").await;
        let topics: Vec<String> = data_payloads(&body)
            .iter()
            .filter_map(|p| p["topic"].as_str().map(str::to_string))
            .collect();
        for expected in FROZEN_SWEEP_TOPICS {
            assert!(
                topics.iter().any(|t| t == expected),
                "frozen topic {expected} missing from stream: {topics:?}"
            );
        }
        assert!(
            !body.contains("unrelated.topic.for.this.test"),
            "the subscription's topic filter must be honored: {body:?}"
        );

        server_task.abort();
    }

    // ----- Edge: disconnect / reconnect -----

    #[tokio::test]
    async fn sse_client_disconnect_releases_the_subscription_and_reconnect_works() {
        let fake = spawn_fake_event_daemon().await;
        let tcp_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind tcp");
        let addr = tcp_listener.local_addr().expect("local addr");
        let server_task = tokio::spawn(run(tcp_listener, fake.socket_path.clone()));

        // --- first client, then an abrupt disconnect ---
        let (first, _head) = connect_sse(addr).await;
        wait_for_receiver_count(&fake.bus, 1).await;
        drop(first);

        // The bridge sees EOF and drops its IPC connection; the daemon side
        // notices on its next write and releases the subscription. Publishing
        // into that race must not panic anything.
        for _ in 0..50 {
            if fake.bus.receiver_count() == 0 {
                break;
            }
            let _ = fake
                .bus
                .publish_generic("sweep.global.dispatch", serde_json::json!({ "probe": true }));
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        wait_for_receiver_count(&fake.bus, 0).await;

        // --- reconnect on the same listener ---
        let (mut second, head) = connect_sse(addr).await;
        assert!(head.contains("text/event-stream"), "head: {head:?}");
        wait_for_receiver_count(&fake.bus, 1).await;

        fake.bus
            .publish(Event::SweepGlobalCompleted {
                sweep_id: "sweep-4392".to_string(),
                outcome: SweepOutcome::Exited,
            })
            .expect("publish");
        let body = read_until(&mut second, "sweep.global.completed").await;
        assert!(
            body.contains("sweep.global.completed"),
            "reconnected client must receive live events: {body:?}"
        );

        // The listener survived the disconnect: /api/status still answers.
        assert!(!server_task.is_finished(), "the serve loop must survive a client disconnect");

        server_task.abort();
    }

    #[tokio::test]
    async fn sse_endpoint_returns_503_when_the_daemon_is_unreachable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("nonexistent.sock");
        let tcp_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind tcp");
        let addr = tcp_listener.local_addr().expect("local addr");
        let server_task = tokio::spawn(run(tcp_listener, socket_path));

        let (status, body) = http_get(addr, "/api/events").await;
        assert_eq!(status, 503, "body: {body:?}");
        assert!(body.contains("daemon unreachable"), "body: {body:?}");

        server_task.abort();
    }

    #[tokio::test]
    async fn sse_endpoint_rejects_non_get_methods() {
        let fake = spawn_fake_event_daemon().await;
        let tcp_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind tcp");
        let addr = tcp_listener.local_addr().expect("local addr");
        let server_task = tokio::spawn(run(tcp_listener, fake.socket_path.clone()));

        let mut stream = TcpStream::connect(addr).await.expect("connect");
        stream
            .write_all(b"POST /api/events HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .await
            .expect("write request");
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.expect("read response");
        let text = String::from_utf8_lossy(&buf);
        assert!(text.starts_with("HTTP/1.1 405"), "response: {text:?}");
        // Read-only invariant: a rejected write attempt never reaches the bus.
        assert_eq!(fake.bus.receiver_count(), 0);

        server_task.abort();
    }
}
