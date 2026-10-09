//! The agent telemetry relay's receiver (Issue #10964): a loopback OTLP/HTTP
//! endpoint for the agent sessions this daemon launches.
//!
//! ```text
//!   launched session ──OTLP/HTTP──▶ 127.0.0.1:<ephemeral>      (server)
//!        (token in its env)              │ token → launched session
//!                                        ▼
//!                              decode · bind identity · scrub  (sanitize)
//!                                        ▼
//!                              bounded queue, drop-oldest      (forward)
//!                                        ▼
//!                              the configured otlp exporter ──▶ upstream
//! ```
//!
//! The session side — the opt-in, the registry of live sessions, and the
//! environment a launch is given — is
//! [`agent_relay`](crate::observability::agent_relay), which is compiled with
//! or without this feature. This module needs the OTLP message types and so
//! lives behind `otlp`; a build without it has no receiver, and the session
//! side then never wires anything.
//!
//! # Address
//!
//! `127.0.0.1` on an **ephemeral port** chosen by the kernel at start and
//! handed to each launched session in its environment. A fixed port would
//! have to be configured, would collide between two daemons (or two users) on
//! one host, and would give every other local process a well-known place to
//! aim at; an ephemeral one needs no configuration and is known only to the
//! sessions the daemon told. The token is still required either way.
//!
//! # Encodings
//!
//! OTLP/HTTP in both `application/x-protobuf` and `application/json`.
//! Protobuf decoding uses `prost`, which `opentelemetry-proto` already
//! compiles in (declaring it directly adds no package to the lockfile). gRPC
//! is not served.

mod estimate;
mod forward;
mod sanitize;
mod server;

use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::Arc;

use super::OtlpExporter;
use crate::observability::agent_relay::{self, Relay};

/// Largest request body accepted. An OTLP batch from an agent CLI is tens of
/// kilobytes; this leaves two orders of magnitude and still bounds what one
/// connection can make the daemon buffer.
pub const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

/// Connections served at once. A launched session opens one per export per
/// signal; anything past this is answered `503` immediately rather than
/// queued.
pub const MAX_CONNECTIONS: usize = 32;

/// Ceiling on a request's estimated decoded size, whatever its wire size.
/// With [`DECODE_CONCURRENCY`] this bounds the memory decoding can take at
/// once (up to twice the estimate, for `Vec` growth).
pub const MAX_DECODED_BYTES: usize = 32 * 1024 * 1024;

/// Requests decoded at once.
pub const DECODE_CONCURRENCY: usize = 2;

/// Requests the forward queue holds for one sink.
pub const MAX_QUEUED_REQUESTS: usize = 256;

/// Bytes the forward queue holds for one sink, measured as the encoded,
/// already-bound requests it actually keeps.
pub const MAX_QUEUED_BYTES: usize = 32 * 1024 * 1024;

/// One `otlp` sink the relay forwards to: the endpoint and `headers_file` of
/// an exporter that actually started.
#[derive(Debug, Clone)]
pub struct Sink {
    pub endpoint: String,
    pub headers_file: Option<String>,
}

/// Start the receiver, or return nothing when the relay is off.
///
/// Started only when `observability.agentRelay.enabled` resolves `true` for
/// `workspace_root` **and** `sinks` is non-empty — the caller passes exactly
/// the `otlp` exporters whose senders it just started, so the relay can never
/// add, resolve or start an exporter of its own. Each sink gets a second
/// [`OtlpExporter`] built from the same endpoint, ingest key and
/// `headers_file`, on its own HTTP client, so relayed traffic shares nothing
/// with the daemon's own export but its destination and credentials.
///
/// Must be called from inside the daemon's tokio runtime.
#[must_use]
pub fn start(
    workspace_root: &Path,
    host_id: &str,
    ingest_key: &str,
    sinks: &[Sink],
) -> Vec<tokio::task::JoinHandle<()>> {
    let Some((relay, handles)) = launch(workspace_root, host_id, ingest_key, sinks) else {
        return Vec::new();
    };
    log::info!(
        "agent-relay: receiving on {} for launched sessions, forwarding to {} otlp sink(s)",
        relay.addr(),
        handles.len().saturating_sub(1)
    );
    agent_relay::register_global(relay);
    handles
}

/// [`start`] without the process-global registration: the relay and its
/// tasks, or `None` when the relay is off or nothing could be started.
fn launch(
    workspace_root: &Path,
    host_id: &str,
    ingest_key: &str,
    sinks: &[Sink],
) -> Option<(Arc<Relay>, Vec<tokio::task::JoinHandle<()>>)> {
    if sinks.is_empty() || !agent_relay::enabled(workspace_root) {
        return None;
    }
    let mut upstreams = Vec::with_capacity(sinks.len());
    for sink in sinks {
        match OtlpExporter::with_headers_file(
            sink.endpoint.clone(),
            ingest_key.to_string(),
            sink.headers_file.as_deref(),
        ) {
            Ok(exporter) => upstreams.push(exporter),
            Err(error) => log::warn!(
                "agent-relay: could not build the forwarder for {}: {error}",
                sink.endpoint
            ),
        }
    }
    if upstreams.is_empty() {
        return None;
    }
    let listener = match bind_loopback() {
        Ok(listener) => listener,
        Err(error) => {
            log::warn!("agent-relay: could not bind a loopback port: {error} — relay off");
            return None;
        }
    };
    let addr = listener.local_addr().ok()?;
    let relay = Relay::new(addr, host_id, tokio::runtime::Handle::try_current().ok())?;
    let limits = forward::Limits {
        max_requests: MAX_QUEUED_REQUESTS,
        max_bytes: MAX_QUEUED_BYTES,
    };
    let mut handles = Vec::with_capacity(upstreams.len() + 1);
    let mut queues = Vec::with_capacity(upstreams.len());
    for upstream in upstreams {
        let queue = forward::RelayQueue::new(limits);
        handles.push(forward::spawn_drain(queue.clone(), upstream));
        queues.push(queue);
    }
    let receiver = server::Receiver::new(relay.clone(), queues, server::Limits::default());
    handles.push(tokio::spawn(server::serve(listener, receiver)));
    Some((relay, handles))
}

/// Bind `127.0.0.1:0` and hand the socket to tokio.
fn bind_loopback() -> std::io::Result<tokio::net::TcpListener> {
    let listener = std::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))?;
    listener.set_nonblocking(true)?;
    tokio::net::TcpListener::from_std(listener)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "relay/tests.rs"]
mod tests;
