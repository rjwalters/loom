//! The host-side listener that swaps a placeholder for the real credential
//! (issue #8674).
//!
//! Hand-rolled HTTP/1.1 over [`tokio::net::TcpListener`], for the same reason
//! [`crate::serve`] is: this daemon has no HTTP framework and one narrow
//! forwarding surface does not justify adding one. The outbound half reuses the
//! `reqwest` client already vendored for the observability exporter.
//!
//! # What this listener will and will not do
//!
//! - It **never chooses a destination from the request.** The upstream origin
//!   comes from the matched launch record, so there is no request shape that
//!   makes this an open relay. `CONNECT`/`TRACE` are refused outright, and a
//!   request that merely *names* another host is refused 403 so the attempt is
//!   visible rather than silently coerced.
//! - It **never follows a redirect** ([`reqwest::redirect::Policy::none`]): a
//!   `302` to another origin is exactly how a compromised-or-spoofed upstream
//!   would walk the real credential off the pinned host.
//! - It **strips every credential header the client sent**
//!   ([`registry::CREDENTIAL_HEADERS`]) before adding the record's own, so a
//!   container that presents its placeholder in one header and something else
//!   in another cannot smuggle the second value upstream.
//! - It **streams the response** chunk by chunk, because the provider traffic
//!   this carries is mostly `text/event-stream`; buffering would turn every
//!   token into a stall — **except** an error response (any non-2xx status),
//!   which is buffered whole instead. Error bodies are small JSON, never a
//!   token stream, and buffering them is what lets this listener read the
//!   provider's own words to decide whether to bad-mark the account (#8699,
//!   below).
//! - It **answers its own control path locally** ([`rotation::CONTROL_PREFIX`],
//!   #8818): every request under it is handled here, authorized by the same
//!   placeholder, and never forwarded — whatever method, sub-path or body.
//!
//! # Per-launch usage attribution and proxy-side bad-marking (issue #8699)
//!
//! Every forwarded request/response pair updates the launch's
//! [`registry::Usage`] tally (byte counts, plus the provider's own
//! rate-limit/usage response headers — an **allowlist**,
//! [`is_usage_header`], so nothing outside that fixed set, and in particular
//! never a [`CREDENTIAL_HEADERS`] entry, is ever captured).
//!
//! An error response is additionally run through
//! [`crate::api_keys_pool::classify::classify`] — the SAME text classifier
//! [`crate::api_keys_pool::ingest`] uses post-hoc on a whole launch log — plus
//! a plain HTTP 429 check ([`classify_response`]). A hit bad-marks the
//! account immediately, here, rather than waiting for the child to exit and a
//! log to be read: [`Record::begin_bad_mark`] guards a launch from being
//! marked twice at the same strength by a racing pair of 429s, and both
//! writers go through [`crate::api_keys_pool::escalate_bad_for_class`], which
//! skips a mark an active one already covers and never shortens a horizon —
//! so the exit-code-driven path neither duplicates the proxy's mark nor
//! downgrades a stronger one.

use super::registry::{Record, Refusal, Registry, CREDENTIAL_HEADERS};
use super::rotation::{self, ControlRefusal, RotateRequest};
use crate::api_keys_pool::classify::{self, Classification};
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Cap on the request head (request line + headers). Generous for an LLM API
/// client, far below anything that could be used to exhaust memory.
const MAX_HEAD_BYTES: usize = 64 * 1024;
/// Cap on a buffered request body. Prompts are large; uploads are not.
const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;
/// Cap on a buffered ERROR response body (never applied to a success
/// response, which is always streamed). Generous for a provider's JSON error
/// shape, far below anything a provider would plausibly send on a refusal.
const MAX_ERROR_BODY_BYTES: usize = 1024 * 1024;

/// Headers that are connection-scoped and must not be forwarded, plus `host`
/// and `content-length`, which the outbound client recomputes.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-connection",
    "proxy-authenticate",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "host",
    "content-length",
    "expect",
];

/// A bound, not-yet-serving listener.
///
/// Split from [`serve`] on purpose: the dispatcher must know the port **before**
/// it builds the `docker run` command (the port is part of the base URL the
/// container is given), which is one step before there is a tokio runtime to
/// serve on.
pub struct Bound {
    listener: std::net::TcpListener,
    addr: SocketAddr,
}

impl Bound {
    /// Bind an ephemeral port on `ip`.
    pub fn bind(ip: std::net::IpAddr) -> std::io::Result<Self> {
        let listener = std::net::TcpListener::bind(SocketAddr::new(ip, 0))?;
        let addr = listener.local_addr()?;
        Ok(Self { listener, addr })
    }

    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Hand the socket to tokio. Must be called from inside a runtime.
    pub fn into_tokio(self) -> std::io::Result<TcpListener> {
        self.listener.set_nonblocking(true)?;
        TcpListener::from_std(self.listener)
    }
}

/// Accept loop. Runs until the task is dropped; each connection is handled
/// once and closed (`Connection: close`), which keeps the framing trivial.
pub async fn serve(listener: TcpListener, registry: Registry) {
    let client = match build_client() {
        Ok(client) => client,
        Err(error) => {
            log::error!("egress-proxy: cannot build outbound client: {error}");
            return;
        }
    };
    loop {
        let Ok((stream, peer)) = listener.accept().await else {
            continue;
        };
        let registry = registry.clone();
        let client = client.clone();
        tokio::spawn(async move {
            if let Err(error) = handle(stream, peer, &registry, &client).await {
                log::debug!("egress-proxy: connection from {peer} ended: {error}");
            }
        });
    }
}

fn build_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        // A redirect is the one way a response can move the substituted
        // credential off the pinned origin. Never follow one.
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(20))
        .build()
}

async fn handle(
    mut stream: TcpStream,
    peer: SocketAddr,
    registry: &Registry,
    client: &reqwest::Client,
) -> std::io::Result<()> {
    let Some((head, leftover)) = read_head(&mut stream).await? else {
        return Ok(());
    };
    let Some(request) = Head::parse(&head) else {
        return refuse(&mut stream, Refusal::MethodNotAllowed, peer, None).await;
    };
    if is_control(&request.path_and_query()) {
        return control(&mut stream, &request, leftover, peer, registry).await;
    }
    let record = match registry.authorize(
        &request.method,
        &request.presented,
        request.requested_authority().as_deref(),
    ) {
        Ok(record) => record,
        Err(refusal) => {
            let launch = registry
                .authorize(&request.method, &request.presented, None)
                .ok()
                .map(|r| r.launch_id);
            return refuse(&mut stream, refusal, peer, launch.as_deref()).await;
        }
    };
    let body = match read_body(&mut stream, &request, leftover).await {
        Ok(body) => body,
        Err(status) => return write_status(&mut stream, status, "request body rejected").await,
    };
    forward(&mut stream, &request, body, &record, client, registry).await
}

/// Is this origin-form target on the proxy's own control path? Case-insensitive
/// so no casing of the prefix reaches the upstream either.
fn is_control(path_and_query: &str) -> bool {
    path_and_query
        .get(..rotation::CONTROL_PREFIX.len())
        .is_some_and(|p| p.eq_ignore_ascii_case(rotation::CONTROL_PREFIX))
}

/// Handle a control request (#8818). Never forwards anything upstream.
async fn control(
    stream: &mut TcpStream,
    head: &Head,
    leftover: Vec<u8>,
    peer: SocketAddr,
    registry: &Registry,
) -> std::io::Result<()> {
    let outcome = async {
        registry
            .authorize(&head.method, &head.presented, head.requested_authority().as_deref())
            .map_err(ControlRefusal::Auth)?;
        if head.path_and_query() != rotation::ROTATE_PATH {
            return Err(ControlRefusal::NotFound);
        }
        if !head.method.eq_ignore_ascii_case("POST") {
            return Err(ControlRefusal::MethodNotAllowed);
        }
        let body = read_control_body(stream, head, leftover).await?;
        let request = RotateRequest::parse(&body)?;
        let presented = head.presented.clone();
        let registry = registry.clone();
        let rotated = tokio::task::spawn_blocking(move || registry.rotate(&presented, request))
            .await
            .map_err(|_| ControlRefusal::RotationUnavailable)??;
        Ok((request.reason, rotated))
    }
    .await;
    match outcome {
        Ok((reason, rotated)) => {
            // Secret-free: launch id, reason, account NAMES only.
            eprintln!("{}", rotated.marker(reason));
            let body = serde_json::json!({
                "rotated": true,
                "account": rotated.account,
                "marked": rotated.marked,
            })
            .to_string();
            write_json(stream, 200, &body).await
        }
        Err(refusal) => {
            log::warn!(
                "egress-proxy: refused control request reason={} peer={peer}",
                refusal.token()
            );
            write_status(stream, refusal.status(), refusal.token()).await
        }
    }
}

/// A control body must declare a small `Content-Length`; chunked or oversized
/// bodies are refused before a byte of them is buffered.
async fn read_control_body(
    stream: &mut TcpStream,
    head: &Head,
    leftover: Vec<u8>,
) -> Result<Vec<u8>, ControlRefusal> {
    if head.header("transfer-encoding").is_some() {
        return Err(ControlRefusal::BadRequest);
    }
    let len: usize = head
        .header("content-length")
        .ok_or(ControlRefusal::BadRequest)?
        .trim()
        .parse()
        .map_err(|_| ControlRefusal::BadRequest)?;
    if len > rotation::MAX_CONTROL_BODY {
        return Err(ControlRefusal::PayloadTooLarge);
    }
    read_body(stream, head, leftover)
        .await
        .map_err(|_| ControlRefusal::BadRequest)
}

/// Read up to the blank line that ends the head. Returns the head plus any
/// body bytes that arrived in the same read.
async fn read_head(stream: &mut TcpStream) -> std::io::Result<Option<(String, Vec<u8>)>> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..read]);
        if let Some(end) = find_head_end(&buf) {
            let head = String::from_utf8_lossy(&buf[..end]).into_owned();
            return Ok(Some((head, buf[end..].to_vec())));
        }
        if buf.len() > MAX_HEAD_BYTES {
            return Ok(None);
        }
    }
}

fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

/// A parsed request head. Header names are lowercased; values keep their case.
struct Head {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    /// Every credential-shaped value the request carried, in header order.
    presented: Vec<String>,
}

impl Head {
    fn parse(raw: &str) -> Option<Self> {
        let mut lines = raw.split("\r\n");
        let mut parts = lines.next()?.split_whitespace();
        let method = parts.next()?.to_string();
        let target = parts.next()?.to_string();
        let mut headers = Vec::new();
        let mut presented = Vec::new();
        for line in lines {
            if line.is_empty() {
                break;
            }
            let (name, value) = line.split_once(':')?;
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim().to_string();
            if CREDENTIAL_HEADERS.contains(&name.as_str()) {
                // Accept both `Bearer <x>` and a bare value: the harness picks
                // the shape, and the placeholder is the same either way.
                let bare = value
                    .split_once(' ')
                    .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
                    .map_or(value.as_str(), |(_, rest)| rest.trim());
                if !bare.is_empty() {
                    presented.push(bare.to_string());
                }
            }
            headers.push((name, value));
        }
        Some(Self {
            method,
            target,
            headers,
            presented,
        })
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    /// The authority this request named for itself: the absolute-form target's
    /// authority when it used one, else its `Host:` header.
    fn requested_authority(&self) -> Option<String> {
        if let Some(rest) = self
            .target
            .split_once("://")
            .map(|(_, rest)| rest)
            .filter(|_| !self.target.starts_with('/'))
        {
            return Some(rest.split('/').next().unwrap_or(rest).to_string());
        }
        // `CONNECT host:port` has no scheme but is refused before this runs.
        self.header("host").map(str::to_string)
    }

    /// Origin-form path + query, whatever form the client used.
    fn path_and_query(&self) -> String {
        if self.target.starts_with('/') {
            return self.target.clone();
        }
        match self.target.split_once("://") {
            Some((_, rest)) => match rest.find('/') {
                Some(i) => rest[i..].to_string(),
                None => "/".to_string(),
            },
            None => format!("/{}", self.target.trim_start_matches('/')),
        }
    }
}

/// Buffer the request body. `Err(status)` is a client error with a fixed reply.
async fn read_body(stream: &mut TcpStream, head: &Head, leftover: Vec<u8>) -> Result<Vec<u8>, u16> {
    if head
        .header("transfer-encoding")
        .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"))
    {
        return read_chunked(stream, leftover).await;
    }
    let Some(len) = head.header("content-length") else {
        return Ok(Vec::new());
    };
    let len: usize = len.trim().parse().map_err(|_| 400u16)?;
    if len > MAX_BODY_BYTES {
        return Err(413);
    }
    let mut body = leftover;
    body.truncate(len.min(body.len()));
    while body.len() < len {
        let mut chunk = vec![0u8; (len - body.len()).min(64 * 1024)];
        let read = stream.read(&mut chunk).await.map_err(|_| 400u16)?;
        if read == 0 {
            return Err(400);
        }
        body.extend_from_slice(&chunk[..read]);
    }
    Ok(body)
}

/// Minimal de-chunker: the body is reassembled and re-sent with a
/// `Content-Length`, so no chunk framing is ever forwarded verbatim.
async fn read_chunked(stream: &mut TcpStream, leftover: Vec<u8>) -> Result<Vec<u8>, u16> {
    let mut raw = leftover;
    let mut body = Vec::new();
    let mut cursor = 0usize;
    loop {
        let line_end = loop {
            if let Some(i) = raw[cursor..].windows(2).position(|w| w == b"\r\n") {
                break cursor + i;
            }
            if !fill(stream, &mut raw).await? {
                return Err(400);
            }
        };
        let size_line = String::from_utf8_lossy(&raw[cursor..line_end]).into_owned();
        let size_hex = size_line.split(';').next().unwrap_or("").trim().to_string();
        let size = usize::from_str_radix(&size_hex, 16).map_err(|_| 400u16)?;
        cursor = line_end + 2;
        if size == 0 {
            return Ok(body);
        }
        if body.len() + size > MAX_BODY_BYTES {
            return Err(413);
        }
        while raw.len() < cursor + size + 2 {
            if !fill(stream, &mut raw).await? {
                return Err(400);
            }
        }
        body.extend_from_slice(&raw[cursor..cursor + size]);
        cursor += size + 2;
    }
}

async fn fill(stream: &mut TcpStream, raw: &mut Vec<u8>) -> Result<bool, u16> {
    let mut chunk = [0u8; 8192];
    let read = stream.read(&mut chunk).await.map_err(|_| 400u16)?;
    if read == 0 {
        return Ok(false);
    }
    raw.extend_from_slice(&chunk[..read]);
    Ok(true)
}

async fn forward(
    stream: &mut TcpStream,
    head: &Head,
    body: Vec<u8>,
    record: &Record,
    client: &reqwest::Client,
    registry: &Registry,
) -> std::io::Result<()> {
    let request_bytes = body.len() as u64;
    let url = record.upstream.url_for(&head.path_and_query());
    let method = match reqwest::Method::from_bytes(head.method.as_bytes()) {
        Ok(method) => method,
        Err(_) => return write_status(stream, 405, "method not allowed").await,
    };
    let mut request = client.request(method, &url);
    for (name, value) in &head.headers {
        if HOP_BY_HOP.contains(&name.as_str()) || CREDENTIAL_HEADERS.contains(&name.as_str()) {
            continue;
        }
        request = request.header(name, value);
    }
    let (header, value) = record.upstream_header();
    request = request.header(header, value);
    if !body.is_empty() {
        request = request.body(body);
    }
    let mut response = match request.send().await {
        Ok(response) => response,
        Err(error) => {
            // `error` renders the URL but never a header value, so the
            // substituted credential cannot reach the log through here.
            log::warn!(
                "egress-proxy: upstream request failed launch={} host={}: {error}",
                record.launch_id,
                record.upstream.host()
            );
            return write_status(stream, 502, "upstream request failed").await;
        }
    };
    let status = response.status();
    // Evidence for a later rotation request (#8818): what the upstream said
    // about THIS credential generation, observed here, never reported by the
    // container.
    registry.observe(&record.launch_id, record.generation, status.as_u16());
    // Captured before the body is consumed either way: the allowlisted
    // rate-limit/usage headers (#8699) and the forwarded header block, since
    // `response.headers()` borrows and both the error and success paths below
    // need it.
    let usage_headers = capture_usage_headers(response.headers());
    let mut header_lines = String::new();
    for (name, value) in response.headers() {
        let name = name.as_str();
        if HOP_BY_HOP.contains(&name) {
            continue;
        }
        if let Ok(value) = value.to_str() {
            header_lines.push_str(&format!("{name}: {value}\r\n"));
        }
    }

    if !status.is_success() {
        // Buffered, not streamed: an error body is small JSON, never a token
        // stream, and this is what lets #8699's classifier read it.
        let error_body = match read_capped(&mut response, MAX_ERROR_BODY_BYTES).await {
            Ok(bytes) => bytes,
            Err(error) => {
                log::warn!(
                    "egress-proxy: upstream error body failed launch={}: {error}",
                    record.launch_id
                );
                record.record_usage(request_bytes, 0, usage_headers);
                return write_status(stream, 502, "upstream response body failed").await;
            }
        };
        record.record_usage(request_bytes, error_body.len() as u64, usage_headers);
        let classification =
            classify_response(status.as_u16(), &String::from_utf8_lossy(&error_body));
        // Reply first, mark second: the harness gets its 429 without waiting
        // on the pool's `mkdir` lock, and a client that already hung up still
        // gets its account marked.
        let replied = async {
            let mut out = format!("HTTP/1.1 {} \r\n{header_lines}", status.as_u16());
            out.push_str(&format!(
                "content-length: {}\r\nconnection: close\r\n\r\n",
                error_body.len()
            ));
            stream.write_all(out.as_bytes()).await?;
            stream.write_all(&error_body).await?;
            stream.flush().await?;
            stream.shutdown().await
        }
        .await;
        if let Some(classification) = classification {
            bad_mark_at_proxy(record.clone(), classification).await;
        }
        return replied;
    }

    let mut response_bytes: u64 = 0;
    // Usage is recorded after this block whether or not it succeeds, so a
    // client that disconnects mid-stream still has its bytes counted.
    let streamed = async {
        let mut out = format!("HTTP/1.1 {} \r\n{header_lines}", status.as_u16());
        out.push_str("transfer-encoding: chunked\r\nconnection: close\r\n\r\n");
        stream.write_all(out.as_bytes()).await?;
        stream.flush().await?;
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) if !chunk.is_empty() => {
                    response_bytes += chunk.len() as u64;
                    stream
                        .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
                        .await?;
                    stream.write_all(&chunk).await?;
                    stream.write_all(b"\r\n").await?;
                    stream.flush().await?;
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(error) => {
                    log::warn!(
                        "egress-proxy: upstream stream ended early launch={}: {error}",
                        record.launch_id
                    );
                    break;
                }
            }
        }
        stream.write_all(b"0\r\n\r\n").await?;
        stream.flush().await?;
        stream.shutdown().await
    }
    .await;
    record.record_usage(request_bytes, response_bytes, usage_headers);
    streamed
}

/// Buffer up to `cap` bytes of `response`'s body. Only ever called on an
/// error response (never a streamed success), so `cap` is a defensive limit
/// on a shape that is normally a few hundred bytes of JSON. A body that hits
/// the cap is forwarded truncated (with a matching `content-length`) and
/// logged, so the truncation is never silent.
async fn read_capped(response: &mut reqwest::Response, cap: usize) -> reqwest::Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut complete = false;
    while buf.len() < cap {
        match response.chunk().await? {
            Some(chunk) => buf.extend_from_slice(&chunk),
            None => {
                complete = true;
                break;
            }
        }
    }
    if !complete || buf.len() > cap {
        log::warn!(
            "egress-proxy: upstream error body exceeded {cap} bytes; forwarding it truncated"
        );
    }
    buf.truncate(cap);
    Ok(buf)
}

/// Response header names worth keeping for per-launch usage attribution
/// (#8699): the provider's own rate-limit/quota telemetry. Deliberately an
/// **allowlist**, not a denylist — anything not named here, and in particular
/// every [`CREDENTIAL_HEADERS`] entry, is dropped rather than risking a future
/// provider header that happens to carry something sensitive.
fn is_usage_header(name: &str) -> bool {
    name.starts_with("anthropic-ratelimit-")
        || name.starts_with("x-ratelimit-")
        || name == "retry-after"
}

fn capture_usage_headers(headers: &reqwest::header::HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            let name = name.as_str().to_ascii_lowercase();
            if !is_usage_header(&name) {
                return None;
            }
            value.to_str().ok().map(|v| (name, v.to_string()))
        })
        .collect()
}

/// What an error response says about the account's quota, if anything.
///
/// Reuses [`classify::classify`] — the SAME table
/// [`crate::api_keys_pool::ingest`] runs post-hoc over a whole launch log —
/// against just this one response body, so a provider's JSON error shape
/// (Anthropic's `{"error":{"type":"rate_limit_error",...}}`, OpenAI's
/// `insufficient_quota`, …) is recognised here exactly as it would be there. A
/// [`Classification::CredentialFailure`] hit — even on a `429` — must NOT
/// bad-mark (guard 4, mirrored from `ingest`'s module docs): an auth fault is
/// not an exhaustion signal. Only when the body carries no recognisable
/// classification at all does a bare `429` status fall back to
/// [`Classification::RateLimited`].
fn classify_response(status: u16, body: &str) -> Option<Classification> {
    if let Some(found) = classify::classify(body, 1) {
        return found.marks_bad().then_some(found);
    }
    (status == 429).then_some(Classification::RateLimited)
}

/// Bad-mark this launch's pool account at the proxy (#8699 AC2), the moment a
/// 429/quota-exhausted response is seen — never waiting for the child to
/// exit. A no-op when the launch's credential was not pool-selected
/// ([`Record::pool_account`] is `None`), and — via
/// [`Record::begin_bad_mark`] — for every request on the same launch that is
/// not strictly stronger than one already marked.
///
/// Runs the actual pool write on a blocking thread:
/// [`crate::api_keys_pool::escalate_bad_for_class`] takes a filesystem
/// `mkdir` lock that can retry for seconds under
/// contention, which must never stall this listener's async reactor (it runs
/// on a two-worker-thread runtime, see [`super::run_with_proxy`]).
async fn bad_mark_at_proxy(record: Record, classification: Classification) {
    let (Some(root), Some(account)) = (
        record.workspace_root().map(std::path::Path::to_path_buf),
        record.pool_account().map(str::to_string),
    ) else {
        return;
    };
    let Some(cooldown) = classification.default_cooldown_secs() else {
        return;
    };
    if !record.begin_bad_mark(Some(cooldown)) {
        return;
    }
    let provider = record.provider.clone();
    let launch_id = record.launch_id.clone();
    let model_class = record.model_class().map(str::to_string);
    let outcome = tokio::task::spawn_blocking(move || {
        crate::api_keys_pool::paths::resolve_provider_root(&root, &provider)
            .map_err(|e| e.to_string())
            .and_then(|provider_root| {
                crate::api_keys_pool::escalate_bad_for_class(
                    &provider_root,
                    &provider,
                    &account,
                    &format!("{} (classified at the egress proxy)", classification.label()),
                    Some(cooldown),
                    model_class.as_deref(),
                )
            })
            .map(|write| (provider, account, write))
    })
    .await;
    match outcome {
        Ok(Ok((provider, account, crate::api_keys_pool::MarkWrite::Written(_)))) => log::warn!(
            "egress-proxy: bad-marked {provider}/{account} as {} launch={launch_id}",
            classification.label()
        ),
        Ok(Ok((provider, account, crate::api_keys_pool::MarkWrite::AlreadyCovered(_)))) => {
            log::info!(
                "egress-proxy: {provider}/{account} already bad-marked at least as long as {} \
                 — not re-marked launch={launch_id}",
                classification.label()
            );
        }
        Ok(Err(error)) => {
            log::warn!("egress-proxy: could not bad-mark for launch={launch_id}: {error}")
        }
        Err(error) => log::warn!("egress-proxy: bad-mark task failed launch={launch_id}: {error}"),
    }
}

async fn refuse(
    stream: &mut TcpStream,
    refusal: Refusal,
    peer: SocketAddr,
    launch: Option<&str>,
) -> std::io::Result<()> {
    let (status, _) = refusal.status();
    log::warn!(
        "egress-proxy: refused request reason={} peer={peer} launch={}",
        refusal.token(),
        launch.unwrap_or("-")
    );
    write_status(stream, status, refusal.token()).await
}

async fn write_status(stream: &mut TcpStream, status: u16, token: &str) -> std::io::Result<()> {
    let body = format!("{{\"error\":\"loom-egress-proxy\",\"reason\":\"{token}\"}}");
    write_json(stream, status, &body).await
}

async fn write_json(stream: &mut TcpStream, status: u16, body: &str) -> std::io::Result<()> {
    let response = format!(
        "HTTP/1.1 {status} \r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;
    stream.shutdown().await
}
