//! The captain's HTTP listener (#12449): `loom-daemon fleet-dispatch serve`.
//!
//! Hand-rolled HTTP/1.1 over [`tokio::net::TcpListener`], in the style of
//! [`crate::serve`]: one request per connection, `Connection: close`, no
//! framework. In order, per connection:
//!
//! 1. a connection cap ([`Options::max_connections`]): over it, `503` at once;
//! 2. **identity before anything is read**: the remote address goes through
//!    [`Identity::whois`] and the node through the roster
//!    ([`Roster::host_for`]). Not a tailnet peer, or a node no dispatcher
//!    entry names: `403`. The identity backend down: `503` (a worker fails
//!    open on it);
//! 3. the request head, under a timeout and a size cap; `POST` to a known
//!    path only (`404` / `405`), a `Content-Length` (`411`) no larger than
//!    [`MAX_BODY_BYTES`] (`413`);
//! 4. the body, under the same timeout, parsed strictly (`400`); the body's
//!    `host_id` must be the caller's mapped host (`403`);
//! 5. the table answers ([`Server::handle`]), and one
//!    `loom.fleet_dispatch.call` span records the call.
//!
//! Every response, error or not, carries `server_epoch`. Nothing panics on a
//! malformed request; a bad connection ends that connection only.
//!
//! State is soft ([`super::table`]): nothing is persisted, and a new process
//! takes a new epoch and a grace window. [`spawn_maintenance`] expires rows
//! and flushes counters on [`Options::sweep_interval`], and reloads the
//! roster from its [`RosterSource`] on [`Options::roster_reload`], keeping the
//! last good roster when a reload fails.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::Result;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;

use super::identity::{Identity, IdentityError};
use super::proto::{
    AckResponse, ClaimRequest, ClaimResponse, ErrorResponse, HeartbeatRequest, HeartbeatResponse,
    RefuseRequest, ReleaseRequest, CLAIM_PATH, HEARTBEAT_PATH, PATHS, REFUSE_PATH, RELEASE_PATH,
};
use super::roster::{Roster, RosterSource};
use super::table::{ClaimDecision, Mono, Policy, RefuseOutcome, Table};
use super::telemetry::{self, Call, Counters};

/// Default TCP port of `loom-daemon fleet-dispatch serve`.
pub const DEFAULT_PORT: u16 = 7433;
/// Largest request body read.
pub const MAX_BODY_BYTES: usize = 256 * 1024;
/// Largest request head read.
const MAX_HEAD_BYTES: usize = 16 * 1024;
/// How long a closing connection drains unread request bytes.
const LINGER: Duration = Duration::from_secs(1);

/// The listener's knobs.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// Connections served at once; more are answered `503`.
    pub max_connections: usize,
    /// Time allowed to read the head, and again the body.
    pub read_timeout: Duration,
    /// How often rows are expired and counters flushed.
    pub sweep_interval: Duration,
    /// How often the roster is reloaded.
    pub roster_reload: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            max_connections: 256,
            read_timeout: Duration::from_secs(5),
            sweep_interval: Duration::from_secs(15),
            roster_reload: Duration::from_secs(300),
        }
    }
}

/// The server's monotonic clock.
pub trait Clock: Send + Sync + 'static {
    /// Now, since the clock's origin.
    fn now(&self) -> Mono;
}

/// [`Instant`]-backed: the origin is construction.
#[derive(Debug, Clone, Copy)]
pub struct SystemClock(Instant);

impl SystemClock {
    /// A clock whose origin is now.
    #[must_use]
    pub fn start() -> Self {
        Self(Instant::now())
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Mono {
        Mono(self.0.elapsed())
    }
}

/// One answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub status: u16,
    pub body: String,
}

/// The captain: identity, roster, table and counters.
pub struct Server<I: Identity> {
    identity: I,
    clock: Arc<dyn Clock>,
    epoch: String,
    roster: RwLock<Arc<Roster>>,
    table: Mutex<Table>,
    counters: Mutex<Counters>,
    seq: AtomicU64,
}

impl<I: Identity> Server<I> {
    /// A server under `epoch` (a fresh one per process) with `roster`.
    /// Its grace window starts at the clock's `now`.
    pub fn new(
        identity: I,
        clock: Arc<dyn Clock>,
        roster: Roster,
        policy: Policy,
        epoch: String,
    ) -> Self {
        let table = Table::new(epoch.clone(), policy, clock.now(), roster.fleet_mode_hosts());
        Self {
            identity,
            clock,
            epoch,
            roster: RwLock::new(Arc::new(roster)),
            table: Mutex::new(table),
            counters: Mutex::new(Counters::default()),
            seq: AtomicU64::new(0),
        }
    }

    /// This process's epoch.
    #[must_use]
    pub fn epoch(&self) -> &str {
        &self.epoch
    }

    /// Replace the roster (a reload).
    pub fn set_roster(&self, roster: Roster) {
        let expected = roster.fleet_mode_hosts();
        if let Ok(mut r) = self.roster.write() {
            *r = Arc::new(roster);
        }
        if let Ok(mut t) = self.table.lock() {
            t.set_expected(expected);
        }
    }

    fn roster(&self) -> Arc<Roster> {
        self.roster
            .read()
            .map_or_else(|p| Arc::clone(&p.into_inner()), |r| Arc::clone(&r))
    }

    /// Run `f` on the table, after expiring what is due (counted).
    fn with_table<T>(&self, f: impl FnOnce(&mut Table, Mono, &mut Counters) -> T) -> T {
        let now = self.clock.now();
        let mut table = self
            .table
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut counters = self
            .counters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let expired = table.expire(now);
        counters.expired(expired);
        f(&mut table, now, &mut counters)
    }

    /// Expire due rows and flush the counters as metric points.
    pub fn sweep(&self) {
        self.with_table(|_, _, _| ());
        let points = self
            .counters
            .lock()
            .map(|mut c| c.take_points())
            .unwrap_or_default();
        if !points.is_empty() {
            crate::observability::ops::emit_metrics(points);
        }
    }

    /// Resolve `peer` to its roster host id. `Err` is the reply to send.
    pub async fn identify(&self, peer: SocketAddr) -> Result<String, Reply> {
        match self.identity.whois(peer).await {
            Ok(node) => self
                .roster()
                .host_for(&node)
                .map(str::to_string)
                .ok_or_else(|| {
                    log::warn!(
                        "fleet-dispatch: refusing {peer}: tailnet node `{}` is not in the roster",
                        node.name
                    );
                    self.error(403, "caller is not a roster host")
                }),
            Err(IdentityError::NotAPeer) => {
                log::warn!("fleet-dispatch: refusing {peer}: not a tailnet peer");
                Err(self.error(403, "caller is not a tailnet peer"))
            }
            Err(e @ IdentityError::Unavailable(_)) => {
                log::warn!("fleet-dispatch: cannot identify {peer}: {e}");
                Err(self.error(503, "caller identity unavailable"))
            }
        }
    }

    /// Answer `host`'s already-identified request `body` on `path`.
    pub fn handle(&self, host: &str, path: &str, body: &[u8]) -> (Reply, Call) {
        let mut call = Call {
            host: Some(host.to_string()),
            endpoint: path.to_string(),
            server_epoch: self.epoch.clone(),
            result: "ok",
            ..Call::default()
        };
        let reply = match path {
            HEARTBEAT_PATH => self.heartbeat(host, body, &mut call),
            CLAIM_PATH => self.claim(host, body, &mut call),
            REFUSE_PATH => self.refuse(host, body, &mut call),
            RELEASE_PATH => self.release(host, body, &mut call),
            _ => Err(self.error(404, "not found")),
        };
        let reply = reply.unwrap_or_else(|e| e);
        call.status = reply.status;
        if reply.status != 200 {
            call.result = "error";
        }
        (reply, call)
    }

    fn parse<T: DeserializeOwned + HostBody>(&self, host: &str, body: &[u8]) -> Result<T, Reply> {
        let parsed: T = serde_json::from_slice(body)
            .map_err(|e| self.error(400, &format!("malformed request body: {e}")))?;
        if parsed.host_id() != host {
            log::warn!(
                "fleet-dispatch: refusing a request from `{host}` that names host `{}`",
                parsed.host_id()
            );
            return Err(self.error(403, "host_id does not match the caller"));
        }
        Ok(parsed)
    }

    fn heartbeat(&self, host: &str, body: &[u8], call: &mut Call) -> Result<Reply, Reply> {
        let hb: HeartbeatRequest = self.parse(host, body)?;
        let grace = self.with_table(|t, now, _| t.heartbeat(now, host, &hb));
        call.reason = grace.then(|| "grace".to_string());
        Ok(self.ok(&HeartbeatResponse {
            server_epoch: self.epoch.clone(),
            grace,
        }))
    }

    fn claim(&self, host: &str, body: &[u8], call: &mut Call) -> Result<Reply, Reply> {
        let req: ClaimRequest = self.parse(host, body)?;
        call.item = Some((req.repo.clone(), req.item, req.step.clone()));
        let max = self.roster().max_concurrent(host);
        let decision = self.with_table(|t, now, counters| {
            let d = t.claim(now, host, &req.repo, req.item, &req.step, max);
            if let ClaimDecision::Refused { reason, .. } = &d {
                counters.claim_refused(reason.as_str());
            }
            d
        });
        let response = match decision {
            ClaimDecision::Granted {
                assignment_id,
                ttl_s,
                ..
            } => {
                call.result = "granted";
                call.assignment_id = Some(assignment_id.clone());
                ClaimResponse::granted(&self.epoch, assignment_id, ttl_s)
            }
            ClaimDecision::Refused {
                reason,
                held_by,
                retry_after_s,
            } => {
                call.result = "refused";
                call.reason = Some(reason.as_str().to_string());
                ClaimResponse::refused(&self.epoch, reason, held_by, retry_after_s)
            }
        };
        Ok(self.ok(&response))
    }

    fn refuse(&self, host: &str, body: &[u8], call: &mut Call) -> Result<Reply, Reply> {
        let req: RefuseRequest = self.parse(host, body)?;
        call.assignment_id = Some(req.assignment_id.clone());
        call.reason = Some(req.reason.clone());
        let outcome = self.with_table(|t, now, counters| {
            if let Some(((repo, item), row)) = t.row_by_id(&req.assignment_id) {
                call.item = Some((repo.clone(), *item, row.step.clone()));
            }
            counters.refused(&req.reason);
            let outcome = t.refuse(now, host, &req.assignment_id, &req.reason);
            if outcome == (RefuseOutcome::Held { granted: true }) {
                counters.claim_lost_after_grant += 1;
            }
            outcome
        });
        Ok(self.ok(&AckResponse {
            server_epoch: self.epoch.clone(),
            known: outcome != RefuseOutcome::Unknown,
        }))
    }

    fn release(&self, host: &str, body: &[u8], call: &mut Call) -> Result<Reply, Reply> {
        let req: ReleaseRequest = self.parse(host, body)?;
        call.assignment_id = Some(req.assignment_id.clone());
        call.reason = Some(req.outcome.clone());
        let known = self.with_table(|t, _, _| {
            if let Some(((repo, item), row)) = t.row_by_id(&req.assignment_id) {
                call.item = Some((repo.clone(), *item, row.step.clone()));
            }
            t.release(host, &req.assignment_id)
        });
        Ok(self.ok(&AckResponse {
            server_epoch: self.epoch.clone(),
            known,
        }))
    }

    fn ok<T: serde::Serialize>(&self, body: &T) -> Reply {
        match serde_json::to_string(body) {
            Ok(body) => Reply { status: 200, body },
            Err(e) => self.error(500, &format!("encoding the response failed: {e}")),
        }
    }

    /// An error reply carrying `server_epoch`.
    #[must_use]
    pub fn error(&self, status: u16, message: &str) -> Reply {
        let body = serde_json::to_string(&ErrorResponse {
            server_epoch: self.epoch.clone(),
            error: message.to_string(),
        })
        .unwrap_or_else(|_| format!(r#"{{"server_epoch":"{}","error":"error"}}"#, self.epoch));
        Reply { status, body }
    }

    fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::Relaxed)
    }
}

/// A request body that names its host.
trait HostBody {
    fn host_id(&self) -> &str;
}

macro_rules! host_body {
    ($($t:ty),*) => {$(
        impl HostBody for $t {
            fn host_id(&self) -> &str {
                &self.host_id
            }
        }
    )*};
}
host_body!(HeartbeatRequest, ClaimRequest, RefuseRequest, ReleaseRequest);

/// A parsed request head.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Head {
    method: String,
    path: String,
    content_length: Option<usize>,
}

/// Read the request line and headers, bounded by [`MAX_HEAD_BYTES`].
async fn read_head<R: tokio::io::AsyncBufRead + Unpin>(reader: &mut R) -> Result<Head, String> {
    let mut total = 0usize;
    let mut request_line: Option<String> = None;
    let mut content_length = None;
    loop {
        let mut line = String::new();
        let n = (&mut *reader)
            .take((MAX_HEAD_BYTES - total + 1) as u64)
            .read_line(&mut line)
            .await
            .map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("connection closed inside the request head".into());
        }
        total += n;
        if total > MAX_HEAD_BYTES {
            return Err("request head too large".into());
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if request_line.is_none() {
            request_line = Some(trimmed.to_string());
            continue;
        }
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                content_length = Some(
                    value
                        .trim()
                        .parse::<usize>()
                        .map_err(|_| "bad Content-Length".to_string())?,
                );
            } else if name.trim().eq_ignore_ascii_case("transfer-encoding") {
                return Err("Transfer-Encoding is not supported".into());
            }
        }
    }
    let line = request_line.unwrap_or_default();
    let mut parts = line.split_whitespace();
    let (Some(method), Some(target), Some(version)) = (parts.next(), parts.next(), parts.next())
    else {
        return Err("malformed request line".into());
    };
    if !version.starts_with("HTTP/1.") {
        return Err("unsupported HTTP version".into());
    }
    Ok(Head {
        method: method.to_string(),
        path: target.split('?').next().unwrap_or(target).to_string(),
        content_length,
    })
}

async fn write_reply(stream: &mut TcpStream, reply: &Reply) -> std::io::Result<()> {
    let reason = match reply.status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        411 => "Length Required",
        413 => "Payload Too Large",
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    };
    let response = format!(
        "HTTP/1.1 {} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{}",
        reply.status,
        reply.body.len(),
        reply.body
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await
}

/// Serve one connection from `peer`.
async fn serve_connection<I: Identity>(
    mut stream: TcpStream,
    peer: SocketAddr,
    server: Arc<Server<I>>,
    opts: Options,
) {
    let started_at = chrono::Utc::now();
    let clock = Instant::now();
    let (reply, mut call) = answer(&mut stream, peer, &server, opts).await;
    call.latency_ms = u64::try_from(clock.elapsed().as_millis()).unwrap_or(u64::MAX);
    if let Err(e) = write_reply(&mut stream, &reply).await {
        log::debug!("fleet-dispatch: writing to {peer} failed: {e}");
    }
    telemetry::emit_call(&call, started_at, server.next_seq());
    linger_close(stream).await;
}

/// Close after a reply that may have left request bytes unread (a refusal
/// sent before the body was read): half-close, then discard what the client
/// still sends, briefly and boundedly. Closing a socket with unread input
/// resets it, and a reset can destroy the reply before the client reads it.
async fn linger_close(mut stream: TcpStream) {
    if stream.shutdown().await.is_err() {
        return;
    }
    let drain = async {
        let mut sink = [0u8; 4096];
        let mut left = MAX_BODY_BYTES + MAX_HEAD_BYTES;
        while left > 0 {
            match stream.read(&mut sink).await {
                Ok(0) | Err(_) => break,
                Ok(n) => left = left.saturating_sub(n),
            }
        }
    };
    let _ = tokio::time::timeout(LINGER, drain).await;
}

type Answered = (Reply, Call);

async fn answer<I: Identity>(
    stream: &mut TcpStream,
    peer: SocketAddr,
    server: &Server<I>,
    opts: Options,
) -> Answered {
    let mut call = Call {
        endpoint: "other".into(),
        server_epoch: server.epoch().to_string(),
        result: "error",
        ..Call::default()
    };
    let fail = |reply: Reply, mut call: Call, why: &str| {
        call.status = reply.status;
        call.reason = Some(why.to_string());
        (reply, call)
    };
    let host = match server.identify(peer).await {
        Ok(h) => h,
        Err(reply) => {
            let why = if reply.status == 403 {
                "forbidden"
            } else {
                "identity_unavailable"
            };
            return fail(reply, call, why);
        }
    };
    call.host = Some(host.clone());
    let mut reader = BufReader::new(stream);
    let head = match tokio::time::timeout(opts.read_timeout, read_head(&mut reader)).await {
        Ok(Ok(head)) => head,
        Ok(Err(e)) => return fail(server.error(400, &e), call, "malformed"),
        Err(_) => return fail(server.error(408, "request head timed out"), call, "timeout"),
    };
    if PATHS.contains(&head.path.as_str()) {
        call.endpoint.clone_from(&head.path);
    } else {
        return fail(server.error(404, "not found"), call, "not_found");
    }
    if head.method != "POST" {
        return fail(server.error(405, "only POST is supported"), call, "method");
    }
    let Some(len) = head.content_length else {
        return fail(server.error(411, "Content-Length required"), call, "length");
    };
    if len > MAX_BODY_BYTES {
        return fail(server.error(413, "request body too large"), call, "too_large");
    }
    let mut body = vec![0u8; len];
    match tokio::time::timeout(opts.read_timeout, reader.read_exact(&mut body)).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => return fail(server.error(400, &e.to_string()), call, "malformed"),
        Err(_) => return fail(server.error(408, "request body timed out"), call, "timeout"),
    }
    server.handle(&host, &head.path, &body)
}

/// Run the accept loop on `listener`. Returns only on a fatal `accept`
/// error; a failed connection never ends the loop.
pub async fn run<I: Identity>(
    listener: TcpListener,
    server: Arc<Server<I>>,
    opts: Options,
) -> Result<()> {
    let permits = Arc::new(Semaphore::new(opts.max_connections.max(1)));
    loop {
        let (mut stream, peer) = listener.accept().await?;
        let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
            let reply = server.error(503, "too many connections");
            tokio::spawn(async move {
                let _ = write_reply(&mut stream, &reply).await;
            });
            continue;
        };
        let server = Arc::clone(&server);
        tokio::spawn(async move {
            serve_connection(stream, peer, server, opts).await;
            drop(permit);
        });
    }
}

/// Spawn the expiry/counter sweep and the roster reload.
pub fn spawn_maintenance<I: Identity>(
    server: Arc<Server<I>>,
    source: Arc<dyn RosterSource>,
    opts: Options,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut sweep = tokio::time::interval(opts.sweep_interval);
        let mut reload = tokio::time::interval(opts.roster_reload);
        sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        reload.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first reload tick fires at once; the roster was just loaded.
        reload.tick().await;
        loop {
            tokio::select! {
                _ = sweep.tick() => server.sweep(),
                _ = reload.tick() => {
                    let source = Arc::clone(&source);
                    match tokio::task::spawn_blocking(move || source.load()).await {
                        Ok(Ok(roster)) => server.set_roster(roster),
                        Ok(Err(e)) => log::warn!(
                            "fleet-dispatch: roster reload failed, keeping the last good one: {e:#}"
                        ),
                        Err(e) => log::warn!("fleet-dispatch: roster reload task failed: {e}"),
                    }
                }
            }
        }
    })
}

#[cfg(test)]
#[path = "server_tests.rs"]
mod tests;
