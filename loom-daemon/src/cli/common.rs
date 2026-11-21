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

use crate::types::{Request, Response};

/// Resolve the daemon's IPC socket path exactly as the running daemon does in
/// `main()`: honour `LOOM_SOCKET_PATH` (test override) first, else
/// `~/.loom/loom-daemon.sock`.
///
/// When `LOOM_SOCKET_PATH` is unset and the default socket does not exist (a
/// plain ssh session against a daemon whose systemd user unit sets a custom
/// path), fall back to the `LOOM_SOCKET_PATH` of this user's running dispatcher,
/// read from `/proc/<pid>/environ` (#12597).
pub(crate) fn resolve_socket_path() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("LOOM_SOCKET_PATH") {
        return Ok(PathBuf::from(path));
    }
    let default = resolve_loom_dir()?.join("loom-daemon.sock");
    if default.exists() {
        return Ok(default);
    }
    let pids = crate::daemon_install_state::pgrep_daemon_pids();
    Ok(pick_socket_path(
        default,
        pids.iter().filter_map(|&pid| daemon_env_socket_path(pid)),
    ))
}

/// First `candidates` entry whose socket file exists, else `default`.
fn pick_socket_path(default: PathBuf, candidates: impl IntoIterator<Item = PathBuf>) -> PathBuf {
    candidates
        .into_iter()
        .find(|p| p.exists())
        .unwrap_or(default)
}

/// `LOOM_SOCKET_PATH` from a NUL-separated `/proc/<pid>/environ` blob.
fn socket_path_from_environ(environ: &[u8]) -> Option<PathBuf> {
    environ
        .split(|&b| b == 0)
        .find_map(|kv| kv.strip_prefix(b"LOOM_SOCKET_PATH="))
        .filter(|v| !v.is_empty())
        .and_then(|v| std::str::from_utf8(v).ok())
        .map(PathBuf::from)
}

/// The `LOOM_SOCKET_PATH` of process `pid`; `None` when unreadable or unset.
fn daemon_env_socket_path(pid: u32) -> Option<PathBuf> {
    socket_path_from_environ(&std::fs::read(format!("/proc/{pid}/environ")).ok()?)
}

/// Message for an unreachable daemon when the socket file is missing at
/// `socket_path` yet a dispatcher process for this user is alive (`pids`), so
/// "Is the daemon running?" would be wrong. `None` means use the generic text.
pub(crate) fn socket_missing_hint(socket_path: &Path, pids: &[u32]) -> Option<String> {
    let pid = pids.first()?;
    if socket_path.exists() {
        return None;
    }
    Some(format!(
        "Socket not found at {}, but a loom-daemon process (pid {pid}) is running.\n\
         It was likely started with a different socket path; set LOOM_SOCKET_PATH to it.",
        socket_path.display()
    ))
}

/// Print [`socket_missing_hint`] for the live dispatchers; true when printed.
pub(crate) fn eprint_socket_missing_hint(socket_path: &Path) -> bool {
    let pids = crate::daemon_install_state::pgrep_daemon_pids();
    match socket_missing_hint(socket_path, &pids) {
        Some(msg) => {
            eprintln!("{msg}");
            true
        }
        None => false,
    }
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

/// Total budget for the root request, from connect through the complete reply.
const PRIMARY_ROOT_LOOKUP_TIMEOUT: Duration = Duration::from_secs(2);

/// Ask the running daemon for its primary workspace root (#14534).
///
/// The resolver is synchronous and can already be inside a Tokio runtime.
/// Drive the socket on a separate thread so the caller can use the same IPC
/// client from either context. An unavailable root leaves other routes open.
pub(crate) fn primary_workspace_root_from_daemon() -> Option<PathBuf> {
    let socket_path = resolve_socket_path().ok()?;
    primary_workspace_root_at(&socket_path, PRIMARY_ROOT_LOOKUP_TIMEOUT)
}

fn primary_workspace_root_at(socket_path: &Path, timeout: Duration) -> Option<PathBuf> {
    let response = std::thread::scope(|scope| {
        std::thread::Builder::new()
            .spawn_scoped(scope, || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .ok()?;
                runtime.block_on(async {
                    let query = query_daemon(socket_path, &Request::GetPrimaryWorkspaceRoot);
                    tokio::time::timeout(timeout, query).await.ok()?.ok()
                })
            })
            .ok()?
            .join()
            .ok()?
    })?;
    match response {
        Response::PrimaryWorkspaceRoot { path } => {
            let path = PathBuf::from(path);
            path.is_absolute().then_some(path)
        }
        _ => None,
    }
}

/// Handle the two daemon-failure arms shared by every CLI subcommand that
/// sends one request and renders one reply (Issue #13912): a daemon-reported
/// `Response::Error` and an unreachable daemon (`Err`). Both print their
/// diagnostic to stderr and exit 1 — byte-identical to the arms this
/// deduplicates, so user-visible output and exit codes are unchanged. Any
/// other response is returned unwrapped for the caller to match its expected
/// variants directly (no `Ok(...)` wrapper) with its own `other =>` catch-all
/// printing "Unexpected response from daemon".
///
/// Deliberately does NOT fold in the install-state socket hint that
/// `restart` special-cases (#4069): those arms are not verbatim copies of the
/// five this helper serves, and unifying them would change stderr text.
pub(crate) fn exit_on_daemon_failure(socket_path: &Path, result: Result<Response>) -> Response {
    match result {
        Ok(Response::Error { message }) => {
            eprintln!("Daemon error: {message}");
            std::process::exit(1);
        }
        Ok(response) => response,
        Err(e) => {
            eprintln!("Could not reach loom-daemon at {}: {e}", socket_path.display());
            eprintln!();
            eprintln!("Is the daemon running? Start it with:");
            eprintln!("  ./.loom/scripts/cli/loom-daemon-start.sh");
            std::process::exit(1);
        }
    }
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

/// Resolve the loom directory (moved from `daemon_service` so the library CLI tree
/// does not reach into the binary crate; `daemon_service` re-exports it).
///
/// Thin `Result` wrapper over [`crate::autonomy_marker::resolve_loom_dir`] —
/// the single implementation of the rule (#13833). `None` (a socket path with
/// no parent and no home directory) becomes one error.
pub fn resolve_loom_dir() -> Result<PathBuf> {
    crate::autonomy_marker::resolve_loom_dir()
        .ok_or_else(|| anyhow!("no home directory and no resolvable LOOM_SOCKET_PATH parent"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader as StdBufReader, Write};
    use std::os::unix::net::{UnixListener, UnixStream as StdUnixStream};

    /// Read one root request and let the test control when the reply arrives.
    fn fake_daemon(
        socket: &Path,
        reply: impl FnOnce(StdUnixStream) + Send + 'static,
    ) -> std::thread::JoinHandle<()> {
        let listener = UnixListener::bind(socket).unwrap();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            StdBufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            assert!(matches!(
                serde_json::from_str::<Request>(&line).unwrap(),
                Request::GetPrimaryWorkspaceRoot
            ));
            reply(stream);
        })
    }

    #[test]
    fn primary_root_lookup_returns_the_daemons_absolute_root() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("d.sock");
        let reply = serde_json::to_string(&Response::PrimaryWorkspaceRoot {
            path: "/srv/loom/primary".into(),
        })
        .unwrap();
        let daemon = fake_daemon(&socket, move |mut stream| writeln!(stream, "{reply}").unwrap());
        assert_eq!(
            primary_workspace_root_at(&socket, Duration::from_secs(2)),
            Some(PathBuf::from("/srv/loom/primary"))
        );
        daemon.join().unwrap();
    }

    #[test]
    fn primary_root_lookup_refuses_a_relative_root_and_foreign_replies() {
        let dir = tempfile::tempdir().unwrap();
        for (i, reply) in [
            r#"{"type":"PrimaryWorkspaceRoot","payload":{"path":"relative/root"}}"#,
            r#"{"type":"Pong"}"#,
            "not json",
        ]
        .into_iter()
        .enumerate()
        {
            let socket = dir.path().join(format!("d{i}.sock"));
            let daemon =
                fake_daemon(&socket, move |mut stream| writeln!(stream, "{reply}").unwrap());
            assert_eq!(primary_workspace_root_at(&socket, Duration::from_secs(2)), None, "{reply}");
            daemon.join().unwrap();
        }
    }

    #[test]
    fn primary_root_lookup_is_none_without_a_daemon_and_bounded_when_it_is_wedged() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            primary_workspace_root_at(&dir.path().join("absent.sock"), Duration::from_secs(1)),
            None
        );
        let socket = dir.path().join("wedged.sock");
        let daemon = fake_daemon(&socket, |_stream| std::thread::sleep(Duration::from_millis(600)));
        let started = std::time::Instant::now();
        assert_eq!(primary_workspace_root_at(&socket, Duration::from_millis(150)), None);
        assert!(started.elapsed() < Duration::from_millis(550), "lookup must honour its bound");
        daemon.join().unwrap();
    }

    #[tokio::test]
    async fn primary_root_lookup_times_out_even_while_the_reply_keeps_arriving() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("slow.sock");
        let daemon = fake_daemon(&socket, |mut stream| {
            // Each piece arrives within the idle timeout. A client that restarts
            // its timer on every read would accept the eventual valid reply.
            for _ in 0..12 {
                if stream.write_all(b" ").is_err() {
                    return; // The client cancelled the exchange at its deadline.
                }
                std::thread::sleep(Duration::from_millis(40));
            }
            let _ = writeln!(
                stream,
                r#"{{"type":"PrimaryWorkspaceRoot","payload":{{"path":"/late"}}}}"#
            );
        });
        assert_eq!(primary_workspace_root_at(&socket, Duration::from_millis(200)), None);
        daemon.join().unwrap();
    }

    #[test]
    fn environ_parsing_finds_socket_path() {
        let blob = b"HOME=/h\0LOOM_SOCKET_PATH=/run/x/loom.sock\0PATH=/bin\0";
        assert_eq!(socket_path_from_environ(blob), Some(PathBuf::from("/run/x/loom.sock")));
        assert_eq!(socket_path_from_environ(b"HOME=/h\0"), None);
        assert_eq!(socket_path_from_environ(b"LOOM_SOCKET_PATH=\0"), None);
    }

    #[test]
    fn pick_prefers_first_existing_candidate_else_default() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.sock");
        let b = dir.path().join("b.sock");
        let c = dir.path().join("c.sock");
        std::fs::write(&b, "").unwrap();
        std::fs::write(&c, "").unwrap();
        let default = PathBuf::from("/nonexistent/default.sock");
        assert_eq!(pick_socket_path(default.clone(), [a.clone(), b.clone(), c]), b);
        assert_eq!(pick_socket_path(default.clone(), [a]), default);
        assert_eq!(pick_socket_path(default.clone(), []), default);
    }

    #[test]
    fn hint_only_when_socket_missing_and_pid_alive() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("gone.sock");
        let msg = socket_missing_hint(&missing, &[42]).unwrap();
        assert!(msg.contains("Socket not found at"));
        assert!(msg.contains("pid 42"));
        assert!(msg.contains("LOOM_SOCKET_PATH"));
        assert!(socket_missing_hint(&missing, &[]).is_none());
        let present = dir.path().join("here.sock");
        std::fs::write(&present, "").unwrap();
        assert!(socket_missing_hint(&present, &[42]).is_none());
    }
}
