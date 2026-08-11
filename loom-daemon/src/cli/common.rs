//! Shared client-side IPC helpers used by several `loom-daemon` CLI
//! subcommands (Issue #4712 — split out of `main.rs`'s `cli/` extraction).
//!
//! These are the low-level "connect to the running daemon over its Unix
//! socket, send one request, parse one response" primitives that
//! `status`/`quarantine`/`dispatch`/`watch`/`restart`/`serve`/`fleet status`
//! all build on.

use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use loom_daemon::types::{Request, Response};

/// Resolve the daemon's IPC socket path exactly as the running daemon does in
/// `main()`: honour `LOOM_SOCKET_PATH` (test override) first, else
/// `~/.loom/loom-daemon.sock`.
pub(crate) fn resolve_socket_path() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("LOOM_SOCKET_PATH") {
        return Ok(PathBuf::from(path));
    }
    Ok(crate::daemon_service::resolve_loom_dir()?.join("loom-daemon.sock"))
}

/// Connect to the running daemon over its Unix socket, send a single `request`,
/// and return the parsed `Response`. Both the connect and the round-trip are
/// individually bounded so an unresponsive/wedged daemon cannot hang the CLI.
/// Mirrors `query_daemon_status` but for arbitrary single-frame requests.
pub(crate) async fn query_daemon(socket_path: &Path, request: &Request) -> Result<Response> {
    query_daemon_bounded(socket_path, request, Duration::from_secs(5)).await
}

/// Like [`query_daemon`] but with a caller-supplied bound on both the connect
/// and the round-trip (Issue #3952). Extracted so the `dispatch` subcommand can
/// name its own ack budget and so the timeout path is unit-testable against a
/// deliberately-unresponsive fake socket without a multi-second wait.
pub(crate) async fn query_daemon_bounded(
    socket_path: &Path,
    request: &Request,
    timeout: Duration,
) -> Result<Response> {
    let stream = tokio::time::timeout(timeout, UnixStream::connect(socket_path))
        .await
        .map_err(|_| anyhow!("connect timed out after {}s", timeout.as_secs()))?
        .map_err(|e| anyhow!("connect failed: {e}"))?;
    let (reader, mut writer) = stream.into_split();

    let request_json = serde_json::to_string(request)?;
    let roundtrip = async move {
        writer.write_all(request_json.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;

        let mut lines = BufReader::new(reader).lines();
        let line = lines
            .next_line()
            .await?
            .ok_or_else(|| anyhow!("daemon closed the connection without responding"))?;
        let response: Response = serde_json::from_str(&line)?;
        Ok::<Response, anyhow::Error>(response)
    };

    tokio::time::timeout(timeout, roundtrip)
        .await
        .map_err(|_| anyhow!("round-trip timed out after {}s", timeout.as_secs()))?
}

/// Default bounded ack budget for the `dispatch` subcommand (Issue #3952).
///
/// Dispatch is emphatically **not** an immediate ack: `SweepRegistry::dispatch()`
/// runs synchronously before replying, and its own documented internal budget for
/// a legitimate, successful dispatch is comfortably multi-second. It flips the
/// label via a blocking `gh issue edit` network round-trip, applies up to a 2s
/// dispatch stagger (`DEFAULT_DISPATCH_STAGGER_MS`) under concurrent dispatch,
/// and polls up to 5s (`TOKEN_NAME_CAPTURE_TIMEOUT`) for the child's account
/// name (an explicitly anticipated graceful-degradation window) before falling
/// back to `UNKNOWN_TOKEN_NAME`. A 5s client bound had essentially zero headroom
/// over that and would false-fail on a real success. We therefore mirror
/// `mcp-loom`'s `DISPATCH_TIMEOUT_MS` (`mcp-loom/src/tools/sweeps.ts`) of 30s for
/// the identical underlying IPC call: real margin over the worst case, while
/// still a bounded, finite value that never reproduces the ~1800s wedge of
/// #3945. On expiry the CLI exits nonzero with a clear "is loom-daemon running?"
/// message.
pub(crate) const DISPATCH_ACK_TIMEOUT: Duration = Duration::from_secs(30);

/// Env override for the dispatch ack budget, sharing the exact name
/// `mcp-loom` uses (`LOOM_DAEMON_IPC_TIMEOUT_MS`) so a single operator-facing
/// convention tunes the client-side IPC timeout across both surfaces.
pub(crate) const DAEMON_IPC_TIMEOUT_ENV: &str = "LOOM_DAEMON_IPC_TIMEOUT_MS";

/// Resolve the effective dispatch ack timeout.
///
/// Mirrors `mcp-loom`'s `Math.max(DISPATCH_TIMEOUT_MS, resolveDaemonIpcTimeoutMs())`
/// semantics for `dispatch_sweep`: a positive-integer-millisecond
/// `LOOM_DAEMON_IPC_TIMEOUT_MS` can only ever *raise* the bound above the 30s
/// floor (for a slow forge / heavily-loaded daemon), never lower it — lowering
/// it would reintroduce exactly the false-"did not ack" negative this widening
/// fixes. An absent, empty, non-numeric, zero, or negative value falls back to
/// the {@link DISPATCH_ACK_TIMEOUT} floor.
pub(crate) fn resolve_dispatch_ack_timeout() -> Duration {
    apply_ipc_timeout_env_floor(DISPATCH_ACK_TIMEOUT)
}

/// Apply the shared `LOOM_DAEMON_IPC_TIMEOUT_MS` override (Issue #6011) as a
/// raise-only floor over `base`: a positive-integer-millisecond value can only
/// ever push the effective timeout *above* `base`, never below it — lowering a
/// caller's own budget would reintroduce a false "did not respond" negative on
/// a daemon that is simply slow, not actually unreachable. An absent, empty,
/// non-numeric, zero, or negative value leaves `base` unchanged.
///
/// [`resolve_dispatch_ack_timeout`] was the original (and, until #6011, only)
/// caller of this pattern; `loom-daemon status` now shares it too (see
/// `cli::status::resolve_status_timeout`) so one env var tunes every
/// client-side IPC round-trip in this binary, not just `dispatch`.
pub(crate) fn apply_ipc_timeout_env_floor(base: Duration) -> Duration {
    if let Ok(raw) = std::env::var(DAEMON_IPC_TIMEOUT_ENV) {
        if let Ok(ms) = raw.trim().parse::<u64>() {
            if ms > 0 {
                return Duration::from_millis(ms).max(base);
            }
        }
    }
    base
}
