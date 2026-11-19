//! Who is calling (#12449): remote address → tailnet node.
//!
//! [`Identity`] is the seam: the server asks it for the node behind a
//! connection's remote address, then maps the node to a fleet `host_id`
//! through the roster ([`super::roster::Roster::host_for`]). Tests inject
//! [`StaticIdentity`]; production uses [`TailscaleWhois`], which asks the
//! local `tailscaled` over its LocalAPI Unix socket
//! (`GET /localapi/v0/whois?addr=<ip:port>`). The captain process needs read
//! access to that socket; nothing else (no token, no key) is involved.
//!
//! `whois` answers are cached per remote IP for [`WHOIS_CACHE_TTL`]: a node's
//! name for an address does not change between a worker's calls, and the
//! cache keeps the socket off the per-call path.

use std::collections::HashMap;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Linux `tailscaled`'s LocalAPI socket.
pub const DEFAULT_TAILSCALE_SOCKET: &str = "/var/run/tailscale/tailscaled.sock";
/// How long a `whois` answer is reused for one remote IP.
pub const WHOIS_CACHE_TTL: Duration = Duration::from_secs(60);
/// The whole LocalAPI exchange, at most.
const WHOIS_TIMEOUT: Duration = Duration::from_secs(2);
/// Largest LocalAPI response read.
const WHOIS_MAX_BYTES: u64 = 1 << 20;

/// The tailnet node behind a remote address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailnetNode {
    /// The node's MagicDNS name (`name.tailnet.ts.net.`).
    pub name: String,
    pub addresses: Vec<IpAddr>,
    pub tags: Vec<String>,
}

/// Why no node came back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityError {
    /// The address is not a tailnet peer: refuse with `403`.
    NotAPeer,
    /// The identity backend could not answer: `503`, so a worker fails open.
    Unavailable(String),
}

impl std::fmt::Display for IdentityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAPeer => write!(f, "not a tailnet peer"),
            Self::Unavailable(why) => write!(f, "identity unavailable: {why}"),
        }
    }
}

/// Resolve a connection's remote address to its tailnet node.
pub trait Identity: Send + Sync + 'static {
    /// The node behind `peer`.
    fn whois(
        &self,
        peer: SocketAddr,
    ) -> impl Future<Output = Result<TailnetNode, IdentityError>> + Send;
}

/// A fixed address → node map (tests).
#[derive(Debug, Clone, Default)]
pub struct StaticIdentity(pub HashMap<IpAddr, TailnetNode>);

impl Identity for StaticIdentity {
    fn whois(
        &self,
        peer: SocketAddr,
    ) -> impl Future<Output = Result<TailnetNode, IdentityError>> + Send {
        let found = self.0.get(&peer.ip().to_canonical()).cloned();
        async move { found.ok_or(IdentityError::NotAPeer) }
    }
}

/// `tailscaled`'s LocalAPI `whois`, over its Unix socket.
#[derive(Debug)]
pub struct TailscaleWhois {
    socket: PathBuf,
    cache: Mutex<HashMap<IpAddr, (Instant, TailnetNode)>>,
}

impl TailscaleWhois {
    /// Ask the daemon listening on `socket`.
    #[must_use]
    pub fn new(socket: PathBuf) -> Self {
        Self {
            socket,
            cache: Mutex::new(HashMap::new()),
        }
    }

    fn cached(&self, ip: IpAddr) -> Option<TailnetNode> {
        let cache = self.cache.lock().ok()?;
        let (at, node) = cache.get(&ip)?;
        (at.elapsed() < WHOIS_CACHE_TTL).then(|| node.clone())
    }

    fn remember(&self, ip: IpAddr, node: &TailnetNode) {
        if let Ok(mut cache) = self.cache.lock() {
            cache.retain(|_, (at, _)| at.elapsed() < WHOIS_CACHE_TTL);
            cache.insert(ip, (Instant::now(), node.clone()));
        }
    }

    async fn ask(&self, peer: SocketAddr) -> Result<TailnetNode, IdentityError> {
        let unavailable = |e: std::io::Error| IdentityError::Unavailable(e.to_string());
        let mut stream = tokio::net::UnixStream::connect(&self.socket)
            .await
            .map_err(unavailable)?;
        // HTTP/1.0 so the reply is delimited by close, never chunked.
        let request = format!(
            "GET /localapi/v0/whois?addr={} HTTP/1.0\r\nHost: local-tailscaled.sock\r\n\r\n",
            SocketAddr::new(peer.ip().to_canonical(), peer.port())
        );
        stream
            .write_all(request.as_bytes())
            .await
            .map_err(unavailable)?;
        let mut raw = Vec::new();
        (&mut stream)
            .take(WHOIS_MAX_BYTES)
            .read_to_end(&mut raw)
            .await
            .map_err(unavailable)?;
        parse_whois_response(&raw)
    }
}

impl Identity for TailscaleWhois {
    async fn whois(&self, peer: SocketAddr) -> Result<TailnetNode, IdentityError> {
        let ip = peer.ip().to_canonical();
        if let Some(node) = self.cached(ip) {
            return Ok(node);
        }
        let node = tokio::time::timeout(WHOIS_TIMEOUT, self.ask(peer))
            .await
            .map_err(|_| IdentityError::Unavailable("whois timed out".into()))??;
        self.remember(ip, &node);
        Ok(node)
    }
}

#[derive(Deserialize)]
struct WhoIsReply {
    #[serde(rename = "Node")]
    node: WhoIsNode,
}

#[derive(Deserialize)]
struct WhoIsNode {
    #[serde(rename = "Name", default)]
    name: String,
    #[serde(rename = "Addresses", default)]
    addresses: Vec<String>,
    #[serde(rename = "Tags", default)]
    tags: Vec<String>,
}

/// Parse a raw LocalAPI `whois` HTTP response. `404` is "no such peer";
/// any other non-200, or a body that is not a whois reply, is unavailable.
pub fn parse_whois_response(raw: &[u8]) -> Result<TailnetNode, IdentityError> {
    let text = String::from_utf8_lossy(raw);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| IdentityError::Unavailable("truncated whois response".into()))?;
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| IdentityError::Unavailable("malformed whois status line".into()))?;
    match status {
        200 => {}
        404 => return Err(IdentityError::NotAPeer),
        other => {
            return Err(IdentityError::Unavailable(format!("whois answered HTTP {other}")));
        }
    }
    let reply: WhoIsReply = serde_json::from_str(body.trim())
        .map_err(|e| IdentityError::Unavailable(format!("unreadable whois body: {e}")))?;
    let addresses = reply
        .node
        .addresses
        .iter()
        .filter_map(|a| a.split('/').next()?.parse::<IpAddr>().ok())
        .collect();
    Ok(TailnetNode {
        name: reply.node.name,
        addresses,
        tags: reply.node.tags,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_whois_reply_yields_the_node() {
        let raw = b"HTTP/1.0 200 OK\r\nContent-Type: application/json\r\n\r\n\
            {\"Node\":{\"ID\":1,\"Name\":\"node-a.example.ts.net.\",\
            \"Addresses\":[\"100.64.0.7/32\",\"fd7a:115c:a1e0::7/128\"],\
            \"Tags\":[\"tag:worker\"],\"Hostinfo\":{}},\"UserProfile\":{}}";
        let node = parse_whois_response(raw).unwrap();
        assert_eq!(node.name, "node-a.example.ts.net.");
        assert_eq!(node.addresses.len(), 2);
        assert_eq!(node.tags, vec!["tag:worker".to_string()]);
    }

    #[test]
    fn not_found_is_not_a_peer_and_anything_else_is_unavailable() {
        assert_eq!(
            parse_whois_response(b"HTTP/1.0 404 Not Found\r\n\r\nno match"),
            Err(IdentityError::NotAPeer)
        );
        for raw in [
            &b"HTTP/1.0 500 oops\r\n\r\n"[..],
            b"HTTP/1.0 200 OK\r\n\r\nnot json",
            b"garbage",
            b"",
        ] {
            assert!(matches!(parse_whois_response(raw), Err(IdentityError::Unavailable(_))));
        }
    }

    #[tokio::test]
    async fn whois_talks_http_over_the_unix_socket_and_caches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ts.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 1024];
            let n = s.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            s.write_all(
                b"HTTP/1.0 200 OK\r\n\r\n{\"Node\":{\"Name\":\"node-a.x.ts.net.\",\"Addresses\":[]}}",
            )
            .await
            .unwrap();
            req
        });
        let whois = TailscaleWhois::new(path);
        let peer: SocketAddr = "100.64.0.7:5555".parse().unwrap();
        assert_eq!(whois.whois(peer).await.unwrap().name, "node-a.x.ts.net.");
        let req = server.await.unwrap();
        assert!(req.starts_with("GET /localapi/v0/whois?addr=100.64.0.7:5555 HTTP/1.0\r\n"));
        // The listener is gone; a second call is answered from the cache.
        assert_eq!(whois.whois(peer).await.unwrap().name, "node-a.x.ts.net.");
    }

    #[tokio::test]
    async fn a_missing_socket_is_unavailable() {
        let whois = TailscaleWhois::new(PathBuf::from("/nonexistent/ts.sock"));
        let peer: SocketAddr = "100.64.0.7:1".parse().unwrap();
        assert!(matches!(whois.whois(peer).await, Err(IdentityError::Unavailable(_))));
    }
}
