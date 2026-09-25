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
//!   token into a stall.

use super::registry::{Record, Refusal, Registry, CREDENTIAL_HEADERS};
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Cap on the request head (request line + headers). Generous for an LLM API
/// client, far below anything that could be used to exhaust memory.
const MAX_HEAD_BYTES: usize = 64 * 1024;
/// Cap on a buffered request body. Prompts are large; uploads are not.
const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

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
    forward(&mut stream, &request, body, &record, client).await
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
) -> std::io::Result<()> {
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
    let mut out = format!("HTTP/1.1 {} \r\n", response.status().as_u16());
    for (name, value) in response.headers() {
        let name = name.as_str();
        if HOP_BY_HOP.contains(&name) {
            continue;
        }
        if let Ok(value) = value.to_str() {
            out.push_str(&format!("{name}: {value}\r\n"));
        }
    }
    out.push_str("transfer-encoding: chunked\r\nconnection: close\r\n\r\n");
    stream.write_all(out.as_bytes()).await?;
    stream.flush().await?;
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) if !chunk.is_empty() => {
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
    let response = format!(
        "HTTP/1.1 {status} \r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;
    stream.shutdown().await
}
