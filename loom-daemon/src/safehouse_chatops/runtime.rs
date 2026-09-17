//! The live inbound-steering task and the executor that maps an accepted
//! [`Command`] onto the daemon's **existing** typed IPC surface (Issue #7893).
//!
//! # Why the enum maps to `Request`, not to handlers
//!
//! Every ChatOps verb already exists as a [`crate::types::Request`] variant that
//! `loom-daemon`'s own CLI and the `mcp-loom` tools drive. [`command_to_request`]
//! is therefore a pure, total, ten-line mapping — not a second implementation of
//! dispatch/cancel/status. That is the point: the ChatOps surface can only ever
//! do things the operator could already do over IPC, and a reviewer can confirm
//! that by reading one function.
//!
//! [`IpcExecutor`] then round-trips that `Request` over the daemon's own Unix
//! socket exactly as `loom-daemon status` would. A loopback client is a little
//! unusual, but it is what keeps this module free of registry/workspace
//! plumbing and guarantees the ChatOps path and the CLI path cannot drift.
//!
//! # Transport
//!
//! One dedicated safehouse connection, built from Phase 1's
//! [`SafehouseClient`] — no second wire-protocol implementation, and the same
//! capped-backoff reconnect shape as `safehouse::run_coordination`. It is
//! deliberately *not* the peer-claim coordination connection: that one is bound
//! to `claims_room`, which an operator may have routed to a dedicated
//! machine-chatter room (#4713) that no human is joined to, and its writer is
//! dedicated to claim ads.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

use super::{ChatOpsConfig, ChatOpsRouter, Command, Decision};
use crate::event_bus::EventBus;
use crate::safehouse::{build_send_request, Envelope, SafehouseClient, SafehouseConfig};
use crate::types::{Request, Response, SweepKind};

/// Reconnect backoff bounds, mirroring `safehouse`'s own defaults.
const MIN_BACKOFF: Duration = Duration::from_secs(2);
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// Bound on one loopback IPC round-trip. `DispatchSweep` is the slow one — it
/// flips a forge label and waits on the child's token name — so this mirrors the
/// 30s budget `cli::common::DISPATCH_ACK_TIMEOUT` documents for the identical
/// call rather than the 5s used for cheap reads.
const IPC_TIMEOUT: Duration = Duration::from_secs(30);

/// Grace period between SIGTERM and SIGKILL for a ChatOps `cancel`, matching the
/// MCP layer's default.
const CANCEL_GRACE_SECS: u64 = 10;

/// Envelope type used for replies. `chat` is the envelope-v1 type for
/// human-facing prose, which is what a steering reply is.
const REPLY_KIND: &str = "chat";

/// A boxed future, so the executor trait stays object-safe without pulling in
/// `async-trait` (not a dependency of this crate).
pub type ExecFuture<'a> = Pin<Box<dyn Future<Output = String> + Send + 'a>>;

/// Executes an accepted command and renders a one-line room reply.
///
/// A trait so the live task is testable against a recording fake without a
/// running daemon.
pub trait CommandExecutor: Send + Sync {
    fn execute<'a>(&'a self, command: &'a Command) -> ExecFuture<'a>;
}

/// Map an accepted command onto the existing typed IPC surface.
///
/// `None` for [`Command::Confirm`] only: a confirm is resolved by the router
/// into the command it was issued for, so it must never reach an executor. That
/// is an invariant of [`ChatOpsRouter::handle_at`], and returning `None` here
/// makes violating it inert rather than surprising.
///
/// `unblock <issue>` maps to [`Request::ClearQuarantine`] — the daemon's
/// existing operator-reachable "make this issue dispatchable again" release
/// path (`loom-daemon quarantine clear <issue>`). It deliberately does **not**
/// touch forge labels: labels remain the coordination substrate and are Phase
/// 3b's operator-agent territory, not the daemon's.
#[must_use]
pub fn command_to_request(command: &Command) -> Option<Request> {
    match command {
        Command::Status => Some(Request::DaemonStatus),
        Command::Dispatch { issue } => Some(Request::DispatchSweep {
            kind: SweepKind::Issue(*issue),
            // The issue number is the natural idempotency key: a double-typed
            // `dispatch 42` returns the running sweep rather than racing a
            // second one.
            idempotency_key: Some(format!("chatops-issue-{issue}")),
            model: None,
            effort: None,
            depends_on: None,
            workspace_root: None,
            force: false,
        }),
        Command::Cancel { sweep } => Some(Request::CancelSweep {
            sweep_id: sweep.clone(),
            grace_secs: CANCEL_GRACE_SECS,
            workspace_root: None,
        }),
        Command::Unblock { issue } => Some(Request::ClearQuarantine {
            issue: *issue,
            workspace_root: None,
        }),
        Command::Watch { number } => Some(Request::RegisterWatch {
            kind: crate::watch_registry::WatchKind::Issue,
            number: *number,
            repo: None,
            workspace_root: None,
            note: Some("registered via safehouse chatops".to_owned()),
        }),
        Command::Confirm { .. } => None,
    }
}

/// Render a daemon [`Response`] as one room line.
///
/// Deliberately terse and lossy — the room is a steering surface, not a
/// dashboard. Anything unrecognized falls back to the variant-agnostic
/// "accepted" line rather than dumping a serialized payload into a human's chat.
#[must_use]
pub fn render_response(command: &Command, response: &Response) -> String {
    let verb = command.summary();
    match response {
        Response::DaemonStatus(report) => format!(
            "status: {} sweep(s) in flight, {} token(s) in pool",
            report.in_flight.len(),
            report.token_pool_size
        ),
        Response::SweepDispatched {
            sweep_id,
            pid,
            token_name,
            ..
        } => format!("dispatched {sweep_id} (pid {pid}, account {token_name})"),
        Response::SweepCancelled {
            sweep_id,
            was_running,
            sigkill_sent,
            ..
        } => {
            if *was_running {
                let how = if *sigkill_sent { "SIGKILL" } else { "SIGTERM" };
                format!("cancelled {sweep_id} ({how})")
            } else {
                format!("{sweep_id} was not running; nothing to cancel")
            }
        }
        Response::QuarantineCleared {
            issue,
            was_quarantined,
        } => {
            if *was_quarantined {
                format!("cleared the quarantine on #{issue}")
            } else {
                format!("#{issue} was not quarantined; nothing to clear")
            }
        }
        Response::WatchRegistered {
            watch,
            already_present,
        } => {
            if *already_present {
                format!("already watching #{} ({})", watch.number, watch.id)
            } else {
                format!("watching #{} ({})", watch.number, watch.id)
            }
        }
        Response::Error { message } => format!("`{verb}` failed: {message}"),
        Response::StructuredError(err) => format!("`{verb}` failed: {err}"),
        _ => format!("`{verb}` accepted"),
    }
}

/// The real executor: one bounded loopback round-trip per command over the
/// daemon's own IPC socket.
pub struct IpcExecutor {
    socket: PathBuf,
    timeout: Duration,
}

impl IpcExecutor {
    #[must_use]
    pub fn new(socket: PathBuf) -> Self {
        Self {
            socket,
            timeout: IPC_TIMEOUT,
        }
    }

    /// Resolve the daemon's IPC socket the same way the daemon itself does:
    /// `LOOM_SOCKET_PATH` (the test-isolation override) first, else
    /// `~/.loom/loom-daemon.sock`.
    #[must_use]
    pub fn resolve_socket() -> Option<PathBuf> {
        if let Ok(path) = std::env::var("LOOM_SOCKET_PATH") {
            let path = path.trim();
            if !path.is_empty() {
                return Some(PathBuf::from(path));
            }
        }
        dirs::home_dir().map(|home| home.join(".loom").join("loom-daemon.sock"))
    }

    async fn round_trip(&self, request: &Request) -> anyhow::Result<Response> {
        let stream =
            tokio::time::timeout(self.timeout, tokio::net::UnixStream::connect(&self.socket))
                .await
                .map_err(|_| anyhow::anyhow!("connect timed out"))?
                .map_err(|err| anyhow::anyhow!("connect failed: {err}"))?;
        let (reader, mut writer) = stream.into_split();
        let line = serde_json::to_string(request)?;
        let exchange = async move {
            writer.write_all(line.as_bytes()).await?;
            writer.write_all(b"\n").await?;
            writer.flush().await?;
            let mut lines = tokio::io::BufReader::new(reader).lines();
            let reply = lines
                .next_line()
                .await?
                .ok_or_else(|| anyhow::anyhow!("daemon closed the connection"))?;
            Ok::<Response, anyhow::Error>(serde_json::from_str(&reply)?)
        };
        tokio::time::timeout(self.timeout, exchange)
            .await
            .map_err(|_| anyhow::anyhow!("round-trip timed out"))?
    }
}

impl CommandExecutor for IpcExecutor {
    fn execute<'a>(&'a self, command: &'a Command) -> ExecFuture<'a> {
        Box::pin(async move {
            let Some(request) = command_to_request(command) else {
                return format!("`{}` is not executable", command.summary());
            };
            match self.round_trip(&request).await {
                Ok(response) => render_response(command, &response),
                Err(err) => {
                    log::warn!(
                        "safehouse chatops: `{}` could not reach the daemon IPC socket ({err:#})",
                        command.summary()
                    );
                    format!("`{}` failed: {err}", command.summary())
                }
            }
        })
    }
}

/// Spawn the inbound-steering task, or return `None` for a byte-for-byte no-op.
///
/// `None` when safehouse is disabled, when no `safehouse.chatops` block resolves
/// (the off-by-default contract), or when no socket path resolves — mirroring
/// `safehouse::spawn_peer_coordination`'s contract exactly.
#[must_use]
pub fn spawn(
    safehouse: SafehouseConfig,
    chatops: ChatOpsConfig,
    events: Option<Arc<EventBus>>,
    runtime: &tokio::runtime::Handle,
) -> Option<tokio::task::JoinHandle<()>> {
    if !safehouse.enabled {
        return None;
    }
    let socket = crate::safehouse::resolve_socket(&safehouse)?;
    let Some(ipc_socket) = IpcExecutor::resolve_socket() else {
        log::warn!(
            "safehouse chatops: enabled but the daemon IPC socket path did not resolve — \
             inbound steering off"
        );
        return None;
    };
    log::info!(
        "safehouse chatops: inbound steering enabled (persona={}, senders={}, confirm_ttl={}s)",
        safehouse.persona,
        chatops.allowed_senders.len(),
        chatops.confirm_ttl.as_secs()
    );
    let router = Arc::new(ChatOpsRouter::new(chatops, safehouse.persona.clone(), events));
    let executor: Arc<dyn CommandExecutor> = Arc::new(IpcExecutor::new(ipc_socket));
    Some(runtime.spawn(async move {
        run(safehouse, socket, router, executor, MIN_BACKOFF, MAX_BACKOFF).await;
    }))
}

/// The inbound loop: read room events, route them, reply, execute.
///
/// Every failure degrades to a reconnect or a `warn` — inbound steering never
/// blocks or fails a sweep, exactly like every other safehouse path.
pub async fn run(
    config: SafehouseConfig,
    socket: PathBuf,
    router: Arc<ChatOpsRouter>,
    executor: Arc<dyn CommandExecutor>,
    min_backoff: Duration,
    max_backoff: Duration,
) {
    let room = router.config().room(&config).map(ToOwned::to_owned);
    let mut backoff = min_backoff;
    let mut warned = false;
    loop {
        let client = match SafehouseClient::connect(&socket, &config.persona, room.clone()).await {
            Ok(client) => {
                if warned {
                    log::info!("safehouse chatops: reconnected to {}", socket.display());
                }
                backoff = min_backoff;
                warned = false;
                client
            }
            Err(err) => {
                if !warned {
                    log::warn!(
                        "safehouse chatops: cannot reach safehoused at {} ({err:#}); \
                         inbound steering paused, dispatch unaffected",
                        socket.display()
                    );
                    warned = true;
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(max_backoff);
                continue;
            }
        };
        let (reader, mut writer, mut next_id, room) = client.into_parts();
        let mut lines = reader.lines();
        loop {
            let line = match lines.next_line().await {
                Ok(Some(line)) => line,
                Ok(None) => break,
                Err(err) => {
                    log::debug!("safehouse chatops: read error ({err}); reconnecting");
                    break;
                }
            };
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
                // Malformed line — dropped, which is fail *closed*: a line we
                // cannot parse never reaches the router, so it can never be
                // executed. The connection stays up (one bad line is not a
                // reason to stop reading the room).
                continue;
            };
            if value.get("event").is_none() {
                continue; // a reply echo to one of our own sends
            }
            let Some((sender, text)) = super::inbound_command(&value, router.persona()) else {
                continue; // not addressed to us
            };
            let decision = router.handle(&sender, &text);
            let mut replies: Vec<(String, String)> = Vec::new();
            if let Some(reply) = decision.reply() {
                replies.push((sender.clone(), reply));
            }
            if let Decision::Execute { command, sender } = &decision {
                replies.push((sender.clone(), executor.execute(command).await));
            }
            let mut broken = false;
            for (to, body) in replies {
                if !send_reply(&mut writer, &mut next_id, room.as_deref(), &to, &body).await {
                    broken = true;
                    break;
                }
            }
            if broken {
                break;
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(max_backoff);
    }
}

/// Write one reply envelope. Returns `false` when the connection is unusable
/// and the caller should reconnect.
async fn send_reply(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    next_id: &mut u64,
    room: Option<&str>,
    to: &str,
    body: &str,
) -> bool {
    let envelope = Envelope {
        to: to.to_owned(),
        kind: REPLY_KIND.to_owned(),
        task_id: None,
        body: body.to_owned(),
        meta: None,
    };
    let id = *next_id;
    *next_id = next_id.wrapping_add(1);
    let request = match build_send_request(&envelope, id, room) {
        Ok(request) => request,
        Err(err) => {
            // A malformed reply is our bug, not a transport failure: log it and
            // keep the connection.
            log::warn!("safehouse chatops: refusing to send invalid reply ({err:#})");
            return true;
        }
    };
    let Ok(mut line) = serde_json::to_string(&request) else {
        return true;
    };
    line.push('\n');
    if writer.write_all(line.as_bytes()).await.is_err() || writer.flush().await.is_err() {
        log::debug!("safehouse chatops: reply write failed; reconnecting");
        return false;
    }
    true
}
