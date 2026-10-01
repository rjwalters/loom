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
pub(crate) const DISPATCH_ACK_TIMEOUT: Duration =
    Duration::from_millis(DEFAULT_IPC_TIMEOUT_MS_FLOOR);

/// The same 30s floor, in whole milliseconds — the tranche-2 hyperparameter
/// default (`hyperparameters.process.ipcTimeoutMs`) sources this so the two
/// constants cannot drift.
pub(crate) const DEFAULT_IPC_TIMEOUT_MS_FLOOR: u64 = 30_000;

/// Env override for the dispatch ack budget, sharing the exact name
/// `mcp-loom` uses (`LOOM_DAEMON_IPC_TIMEOUT_MS`) so a single operator-facing
/// convention tunes the client-side IPC timeout across both surfaces. The
/// env var sits above the `hyperparameters.process.ipcTimeoutMs` layer.
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
/// The hyperparameters layer (`hyperparameters.process.ipcTimeoutMs`,
/// startup-anchored — `None` outside a running daemon, so env > default is
/// preserved in CLI-only contexts) is an additional raise-only contributor:
/// the effective floor is the higher of `base` and the layer value, and the
/// env var — still the top tier — can only raise further from there.
///
/// [`resolve_dispatch_ack_timeout`] was the original (and, until #6011, only)
/// caller of this pattern; `loom-daemon status` now shares it too (see
/// `cli::status::resolve_status_timeout`) so one env var tunes every
/// client-side IPC round-trip in this binary, not just `dispatch`.
pub(crate) fn apply_ipc_timeout_env_floor(base: Duration) -> Duration {
    let layer_floor =
        loom_daemon::config_resolver::u64_from_layer_global("process", "ipcTimeoutMs")
            .map(Duration::from_millis);
    let floor = layer_floor.map_or(base, |l| base.max(l));
    ipc_timeout_raise_over_floor(
        std::env::var(DAEMON_IPC_TIMEOUT_ENV)
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok()),
        floor,
    )
}

/// Pure raise-only **env > floor** step: a positive-integer-millisecond env
/// value pushes the effective timeout above `floor`, never below it; an
/// absent, zero, or non-positive value leaves `floor` unchanged. Split out
/// from [`apply_ipc_timeout_env_floor`] so the precedence is unit-testable
/// without touching process-global env state.
fn ipc_timeout_raise_over_floor(env_ms: Option<u64>, floor: Duration) -> Duration {
    env_ms
        .filter(|ms| *ms > 0)
        .map_or(floor, |ms| Duration::from_millis(ms).max(floor))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drift guard: `hyperparams.rs`'s `ProcessParams::default()` sources its
    /// `ipc_timeout_ms` field from the same documented 30_000 literal (its
    /// comment names this test) — if the two ever diverge, the daemon and its
    /// CLI would disagree about the same floor.
    #[test]
    fn ipc_timeout_floor_stays_in_sync_with_the_hyperparams_literal() {
        assert_eq!(
            DEFAULT_IPC_TIMEOUT_MS_FLOOR,
            loom_daemon::hyperparams::Hyperparameters::default()
                .process
                .ipc_timeout_ms
        );
        assert_eq!(DISPATCH_ACK_TIMEOUT, Duration::from_millis(DEFAULT_IPC_TIMEOUT_MS_FLOOR));
    }

    // ===== raise-only precedence (env > hyperparameters layer > base) =====
    //
    // Pure-tier tests: `ipc_timeout_raise_over_floor` takes an already-parsed
    // Option env tier and an already-raised floor, so these never touch
    // process-global env state (no `#[serial]` needed).

    #[test]
    fn env_above_the_floor_raises_it() {
        assert_eq!(
            ipc_timeout_raise_over_floor(Some(60_000), Duration::from_secs(30)),
            Duration::from_secs(60)
        );
    }

    #[test]
    fn env_below_the_floor_is_clamped_up_never_lowered() {
        assert_eq!(
            ipc_timeout_raise_over_floor(Some(1_000), Duration::from_secs(30)),
            Duration::from_secs(30)
        );
    }

    #[test]
    fn absent_zero_or_invalid_env_leaves_the_floor() {
        for env_ms in [None, Some(0)] {
            assert_eq!(
                ipc_timeout_raise_over_floor(env_ms, Duration::from_secs(45)),
                Duration::from_secs(45),
                "env tier {env_ms:?} must fall through to the floor"
            );
        }
    }

    /// The full three-tier stack, composed purely: the layer raises the 30s
    /// base to a 45s floor, then the env tier can only raise further.
    #[test]
    fn layer_raises_the_base_then_env_raises_over_the_layer() {
        let base = Duration::from_secs(30);
        let layer_floor = Some(Duration::from_millis(45_000u64));
        let floor = layer_floor.map_or(base, |l| base.max(l));
        assert_eq!(floor, Duration::from_secs(45), "the layer floor is raise-only over base");

        // Env below the layer floor clamps up to it...
        assert_eq!(ipc_timeout_raise_over_floor(Some(40_000), floor), Duration::from_secs(45));
        // ...and env above it still wins.
        assert_eq!(ipc_timeout_raise_over_floor(Some(60_000), floor), Duration::from_secs(60));
    }
}
