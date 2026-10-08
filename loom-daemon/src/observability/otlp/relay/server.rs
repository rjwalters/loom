//! The receiver's HTTP/1.1 surface (Issue #10964).
//!
//! Hand-rolled over [`tokio::net::TcpListener`] for the same reason
//! `worker_spawn::egress_proxy::server` and `serve` are: the daemon has no
//! HTTP framework, and three `POST` paths do not justify adding one.
//!
//! # What a request must be
//!
//! `POST /v1/logs`, `/v1/metrics` or `/v1/traces`, carrying
//! `Authorization: Bearer <token>` for a session this daemon launched and
//! that is still live, with an identity-encoded `application/x-protobuf` or
//! `application/json` body no larger than [`Limits::max_body`]. The token is
//! checked **before a byte of the body is read**, so an unauthenticated peer
//! cannot make the daemon buffer anything.
//!
//! Every refusal is an HTTP status, and nothing here indexes, unwraps or
//! trusts a length it did not check: a malformed request cannot panic the
//! daemon. Statuses follow OTLP/HTTP — `400` for a body that will never
//! decode (the client must not retry it), `503` when the connection cap is
//! reached (it may).
//!
//! Each connection serves one request and is closed, which keeps the framing
//! trivial; an OTLP exporter batches, so it reconnects a few times a minute.
//!
//! The token never reaches a log line: refusals log a reason and the peer's
//! loopback port, nothing from the request.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::super::transport::Signal;
use super::forward::{Batch, RelayQueue};
use super::sanitize::{self, Encoding};
use crate::observability::agent_relay::Relay;

/// Per-request bounds.
#[derive(Debug, Clone, Copy)]
pub(super) struct Limits {
    /// Request line plus headers.
    pub max_head: usize,
    /// Decoded body.
    pub max_body: usize,
    /// Connections served at once.
    pub max_connections: usize,
    /// Wall-clock budget to receive one whole request.
    pub read_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_head: 16 * 1024,
            max_body: super::MAX_BODY_BYTES,
            max_connections: super::MAX_CONNECTIONS,
            read_timeout: Duration::from_secs(30),
        }
    }
}

/// Lifetime counters, for tests and the periodic refusal log line.
#[derive(Debug, Default)]
pub(super) struct Counters {
    pub accepted: AtomicU64,
    pub unauthorized: AtomicU64,
    pub malformed: AtomicU64,
    pub too_large: AtomicU64,
    pub busy: AtomicU64,
}

/// Everything a connection handler needs.
pub(super) struct Receiver {
    relay: Arc<Relay>,
    queues: Vec<Arc<RelayQueue>>,
    limits: Limits,
    permits: Arc<tokio::sync::Semaphore>,
    pub(super) counters: Counters,
}

impl Receiver {
    pub(super) fn new(
        relay: Arc<Relay>,
        queues: Vec<Arc<RelayQueue>>,
        limits: Limits,
    ) -> Arc<Self> {
        Arc::new(Receiver {
            relay,
            queues,
            permits: Arc::new(tokio::sync::Semaphore::new(limits.max_connections.max(1))),
            limits,
            counters: Counters::default(),
        })
    }

    /// Connection slots free right now.
    #[cfg(test)]
    pub(super) fn permits_available(&self) -> usize {
        self.permits.available_permits()
    }
}

/// Accept loop. Runs until the task is dropped.
pub(super) async fn serve(listener: TcpListener, receiver: Arc<Receiver>) {
    loop {
        let Ok((mut stream, peer)) = listener.accept().await else {
            // A transient accept failure (fd exhaustion) must not spin.
            tokio::time::sleep(Duration::from_millis(50)).await;
            continue;
        };
        let Ok(permit) = receiver.permits.clone().try_acquire_owned() else {
            receiver.counters.busy.fetch_add(1, Ordering::Relaxed);
            tokio::spawn(async move {
                let _ = respond(&mut stream, Refusal::Busy.status(), None).await;
            });
            continue;
        };
        let receiver = receiver.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let outcome =
                tokio::time::timeout(receiver.limits.read_timeout, handle(&mut stream, &receiver))
                    .await;
            let refusal = match outcome {
                Ok(Ok(encoding)) => {
                    receiver.counters.accepted.fetch_add(1, Ordering::Relaxed);
                    let _ = respond(&mut stream, 200, Some(encoding)).await;
                    return;
                }
                Ok(Err(refusal)) => refusal,
                Err(_) => Refusal::Timeout,
            };
            refusal.count(&receiver.counters);
            log::debug!("agent-relay: refused a request from {peer}: {}", refusal.reason());
            let _ = respond(&mut stream, refusal.status(), None).await;
        });
    }
}

/// Why a request was not accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Refusal {
    Closed,
    BadRequest,
    NotFound,
    MethodNotAllowed,
    Unauthorized,
    LengthRequired,
    TooLarge,
    UnsupportedMedia,
    Timeout,
    Busy,
}

impl Refusal {
    pub(super) fn status(self) -> u16 {
        match self {
            Refusal::Closed | Refusal::BadRequest => 400,
            Refusal::Unauthorized => 401,
            Refusal::NotFound => 404,
            Refusal::MethodNotAllowed => 405,
            Refusal::Timeout => 408,
            Refusal::LengthRequired => 411,
            Refusal::TooLarge => 413,
            Refusal::UnsupportedMedia => 415,
            Refusal::Busy => 503,
        }
    }

    fn reason(self) -> &'static str {
        match self {
            Refusal::Closed => "connection closed mid-request",
            Refusal::BadRequest => "malformed request",
            Refusal::NotFound => "unknown path",
            Refusal::MethodNotAllowed => "method not allowed",
            Refusal::Unauthorized => "no live session for the presented credential",
            Refusal::LengthRequired => "no body length",
            Refusal::TooLarge => "request too large",
            Refusal::UnsupportedMedia => "unsupported content type or encoding",
            Refusal::Timeout => "request not received in time",
            Refusal::Busy => "connection limit reached",
        }
    }

    fn count(self, counters: &Counters) {
        let counter = match self {
            Refusal::Unauthorized => &counters.unauthorized,
            Refusal::TooLarge => &counters.too_large,
            Refusal::Busy => &counters.busy,
            _ => &counters.malformed,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

/// Serve one request: authenticate, read, sanitize, enqueue.
async fn handle(stream: &mut TcpStream, receiver: &Receiver) -> Result<Encoding, Refusal> {
    let (head, leftover) = read_head(stream, receiver.limits.max_head).await?;
    let head = Head::parse(&head).ok_or(Refusal::BadRequest)?;
    let signal = match head.path() {
        "/v1/logs" => Signal::Logs,
        "/v1/metrics" => Signal::Metrics,
        "/v1/traces" => Signal::Traces,
        _ => return Err(Refusal::NotFound),
    };
    if !head.method.eq_ignore_ascii_case("POST") {
        return Err(Refusal::MethodNotAllowed);
    }
    // Attribution: the token, and only the token, names the session.
    let bound = head
        .bearer()
        .and_then(|token| receiver.relay.authorize(token))
        .ok_or(Refusal::Unauthorized)?;
    if head
        .header("content-encoding")?
        .is_some_and(|value| !value.trim().eq_ignore_ascii_case("identity"))
    {
        return Err(Refusal::UnsupportedMedia);
    }
    let encoding = head
        .header("content-type")?
        .and_then(Encoding::from_content_type)
        .ok_or(Refusal::UnsupportedMedia)?;
    let body = read_body(stream, &head, leftover, receiver.limits.max_body).await?;
    let bytes = body.len();
    // Decoding and the regex pass are CPU work proportional to the body, so
    // they run off the async workers.
    let batch = tokio::task::spawn_blocking(move || {
        let mut payload = sanitize::decode(signal, encoding, &body).ok()?;
        let items = sanitize::bind_and_scrub(&mut payload, &bound);
        Some(Batch {
            payload,
            items,
            bytes,
            session: bound.label(),
            resource: sanitize::bound_resource(None, &bound),
        })
    })
    .await
    .ok()
    .flatten()
    .ok_or(Refusal::BadRequest)?;
    if batch.items > 0 {
        if let Some((last, rest)) = receiver.queues.split_last() {
            for queue in rest {
                queue.offer(batch.clone());
            }
            last.offer(batch);
        }
    }
    Ok(encoding)
}

/// A parsed request head. Header names are lower-cased.
struct Head {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
}

impl Head {
    fn parse(raw: &str) -> Option<Self> {
        let mut lines = raw.split("\r\n");
        let mut parts = lines.next()?.split(' ');
        let method = parts.next().filter(|m| !m.is_empty())?.to_string();
        let target = parts.next().filter(|t| !t.is_empty())?.to_string();
        if !parts.next()?.starts_with("HTTP/1.") || parts.next().is_some() {
            return None;
        }
        let mut headers = Vec::new();
        for line in lines {
            if line.is_empty() {
                break;
            }
            let (name, value) = line.split_once(':')?;
            if name.is_empty() || name.contains(|c: char| c.is_ascii_whitespace()) {
                return None;
            }
            headers.push((name.to_ascii_lowercase(), value.trim().to_string()));
        }
        Some(Head {
            method,
            target,
            headers,
        })
    }

    /// The target's path, without a query.
    fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or_default()
    }

    /// The single value of `name`, `None` when absent. A repeated header is
    /// refused: which copy wins is exactly the ambiguity request smuggling
    /// needs.
    fn header(&self, name: &str) -> Result<Option<&str>, Refusal> {
        let mut values = self
            .headers
            .iter()
            .filter(|(header, _)| header == name)
            .map(|(_, value)| value.as_str());
        let first = values.next();
        if values.next().is_some() {
            return Err(Refusal::BadRequest);
        }
        Ok(first)
    }

    /// The `Authorization: Bearer` credential, when there is exactly one.
    fn bearer(&self) -> Option<&str> {
        let value = self.header("authorization").ok()??;
        let (scheme, token) = value.split_once(' ')?;
        let token = token.trim();
        (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then_some(token)
    }
}

/// Read up to the blank line that ends the head. Returns the head and any
/// body bytes that arrived with it.
async fn read_head(stream: &mut TcpStream, max_head: usize) -> Result<(String, Vec<u8>), Refusal> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(end) = find(&buffer, b"\r\n\r\n") {
            let head = String::from_utf8(buffer.get(..end).unwrap_or_default().to_vec())
                .map_err(|_| Refusal::BadRequest)?;
            let leftover = buffer.get(end + 4..).unwrap_or_default().to_vec();
            return Ok((head, leftover));
        }
        if buffer.len() > max_head {
            return Err(Refusal::TooLarge);
        }
        let read = stream.read(&mut chunk).await.map_err(|_| Refusal::Closed)?;
        if read == 0 {
            return Err(Refusal::Closed);
        }
        buffer.extend_from_slice(chunk.get(..read).unwrap_or_default());
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Read more of the request into `buffer`.
async fn fill(stream: &mut TcpStream, buffer: &mut Vec<u8>) -> Result<(), Refusal> {
    let mut chunk = [0u8; 16 * 1024];
    let read = stream.read(&mut chunk).await.map_err(|_| Refusal::Closed)?;
    if read == 0 {
        return Err(Refusal::Closed);
    }
    buffer.extend_from_slice(chunk.get(..read).unwrap_or_default());
    Ok(())
}

/// Read the body, never buffering more than `max_body` of it. A declared
/// length over the cap is refused before anything is read.
async fn read_body(
    stream: &mut TcpStream,
    head: &Head,
    leftover: Vec<u8>,
    max_body: usize,
) -> Result<Vec<u8>, Refusal> {
    let length = head.header("content-length")?;
    match head.header("transfer-encoding")? {
        Some(encoding) => {
            // Both framings at once is ambiguous; anything but plain
            // `chunked` is a coding this receiver does not implement.
            if length.is_some() || !encoding.eq_ignore_ascii_case("chunked") {
                return Err(Refusal::BadRequest);
            }
            read_chunked(stream, leftover, max_body).await
        }
        None => {
            let length: usize = length
                .ok_or(Refusal::LengthRequired)?
                .parse()
                .map_err(|_| Refusal::BadRequest)?;
            if length > max_body {
                return Err(Refusal::TooLarge);
            }
            let mut body = leftover;
            while body.len() < length {
                fill(stream, &mut body).await?;
            }
            body.truncate(length);
            Ok(body)
        }
    }
}

/// Decode a `chunked` body, capped at `max_body` decoded bytes. Trailers are
/// not read: nothing after the terminating chunk is needed.
async fn read_chunked(
    stream: &mut TcpStream,
    mut buffer: Vec<u8>,
    max_body: usize,
) -> Result<Vec<u8>, Refusal> {
    /// A chunk-size line longer than this is not a chunk-size line.
    const MAX_SIZE_LINE: usize = 256;
    let mut body = Vec::new();
    loop {
        let line_end = loop {
            if let Some(end) = find(&buffer, b"\r\n") {
                break end;
            }
            if buffer.len() > MAX_SIZE_LINE {
                return Err(Refusal::BadRequest);
            }
            fill(stream, &mut buffer).await?;
        };
        let line = std::str::from_utf8(buffer.get(..line_end).unwrap_or_default())
            .map_err(|_| Refusal::BadRequest)?;
        let size = line.split(';').next().unwrap_or_default().trim();
        let size = usize::from_str_radix(size, 16).map_err(|_| Refusal::BadRequest)?;
        buffer.drain(..line_end + 2);
        if size == 0 {
            return Ok(body);
        }
        if size > max_body.saturating_sub(body.len()) {
            return Err(Refusal::TooLarge);
        }
        // `size` is bounded by `max_body` here, so this cannot overflow.
        while buffer.len() < size + 2 {
            fill(stream, &mut buffer).await?;
        }
        body.extend_from_slice(buffer.get(..size).unwrap_or_default());
        if buffer.get(size..size + 2) != Some(b"\r\n".as_slice()) {
            return Err(Refusal::BadRequest);
        }
        buffer.drain(..size + 2);
    }
}

/// Write a response and close. A success answers in the request's own
/// encoding with an empty `Export*ServiceResponse` (`{}` in JSON, zero bytes
/// in protobuf), as OTLP/HTTP specifies.
async fn respond(
    stream: &mut TcpStream,
    status: u16,
    encoding: Option<Encoding>,
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        411 => "Length Required",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        _ => "Service Unavailable",
    };
    let (content_type, body): (&str, &[u8]) = match encoding {
        Some(Encoding::Json) => (Encoding::Json.content_type(), b"{}"),
        Some(Encoding::Protobuf) => (Encoding::Protobuf.content_type(), b""),
        None => ("text/plain", b""),
    };
    let retry = if status == 503 {
        "Retry-After: 1\r\n"
    } else {
        ""
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         {retry}Connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.shutdown().await?;
    if status != 200 {
        discard_unread(stream).await;
    }
    Ok(())
}

/// After a refusal, read and discard a little of what the client is still
/// sending before the socket is closed. Closing with unread input makes the
/// kernel reset the connection, which can destroy the status line the client
/// has not read yet — it would see a broken pipe instead of the `401`/`413`
/// that explains it. Bounded in bytes and in time, so a refused peer cannot
/// hold the connection open by trickling.
async fn discard_unread(stream: &mut TcpStream) {
    const MAX_BYTES: usize = 256 * 1024;
    const IDLE: Duration = Duration::from_millis(20);
    const TOTAL: Duration = Duration::from_millis(250);
    let deadline = tokio::time::Instant::now() + TOTAL;
    let mut chunk = [0u8; 16 * 1024];
    let mut discarded = 0usize;
    while discarded < MAX_BYTES && tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(IDLE, stream.read(&mut chunk)).await {
            Ok(Ok(read)) if read > 0 => discarded += read,
            _ => break,
        }
    }
}
