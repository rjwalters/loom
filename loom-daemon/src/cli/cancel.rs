//! `loom-daemon cancel` handler (Issue #4980).
//!
//! CLI parity for the `mcp__loom__cancel_sweep` MCP tool. Before this existed,
//! the only sanctioned way to stop a wedged sweep was the MCP tool — which lives
//! in the operator's own Claude session on their own machine. Over ssh (the
//! normal way a fleet worker is reached) there was no lever at all except raw
//! `kill`, and hand-killing the tracked wrapper pid is exactly what produced the
//! 2026-08-03 incident: a surviving `claude` agent that noticed its subprocesses
//! had died and *relaunched* them, against an issue whose claim had already been
//! returned to the queue.
//!
//! Deliberately a thin client over the **same** `CancelSweep` IPC request the
//! MCP tool sends (`mcp-loom/src/tools/sweeps.ts` → `sendDaemonRequest`). The
//! termination itself — SIGTERM to the process group, grace window, SIGKILL
//! escalation, lock release, label restore, event emission — happens entirely
//! daemon-side in `cancel_sweep_nonblocking`, so the two operator surfaces
//! cannot drift apart: there is one implementation and two callers.

use anyhow::Result;
use std::path::PathBuf;
use std::time::Duration;

use loom_daemon::types::{Request, Response, SweepInfo, SweepKind, SweepState};

use super::common::{query_daemon_bounded, resolve_socket_path};
use super::dispatch::resolve_cli_dispatch_workspace;

/// Default seconds between the daemon's SIGTERM and its SIGKILL escalation.
///
/// Matches the `grace_secs` default the `cancel_sweep` MCP tool sends, so the
/// two operator surfaces behave identically for the same sweep unless the
/// operator explicitly asks otherwise.
pub(crate) const DEFAULT_CANCEL_GRACE_SECS: u64 = 30;

/// Client-side ceiling on the cancel round-trip.
///
/// The daemon acks only after the *whole* cancel completes (it polls the grace
/// window before replying), so the budget must exceed `grace_secs` or a
/// perfectly successful cancel reads as a timeout. The buffer mirrors
/// `mcp-loom`'s `CANCEL_TIMEOUT_BUFFER_MS` treatment of the identical call.
pub(crate) const CANCEL_ACK_BUFFER: Duration = Duration::from_secs(15);

/// Build the `CancelSweep` IPC request. Pure and side-effect-free so the flag
/// plumbing is unit-testable without a socket — mirrors `build_dispatch_request`
/// (#3952) and the field mapping the `mcp__loom__cancel_sweep` tool uses, so
/// both operator surfaces put byte-for-byte-equivalent frames on the wire.
fn build_cancel_request(sweep_id: String, grace_secs: u64, workspace: Option<String>) -> Request {
    Request::CancelSweep {
        sweep_id,
        grace_secs,
        workspace_root: workspace,
    }
}

/// Pick the sweep to cancel for `--issue N` out of a `ListSweeps` result.
///
/// Only **live** (`Running` / `Pending`) entries are candidates: terminal
/// entries linger in the registry for an hour after they exit
/// (`TERMINAL_RETENTION_SECS`), and cancelling one of those would be a
/// no-op ack that misleads the operator into thinking the sweep they can still
/// see in `ps` has been dealt with.
///
/// Ambiguity is an error rather than a guess — two live sweeps for one issue is
/// itself a bug worth surfacing, and picking one silently could leave the other
/// running while reporting success. Pure so the selection rules are testable
/// without a daemon.
fn select_sweep_for_issue(sweeps: &[SweepInfo], issue: u32) -> Result<String, String> {
    let live: Vec<&SweepInfo> = sweeps
        .iter()
        .filter(|info| matches!(info.kind, SweepKind::Issue(n) if n == issue))
        .filter(|info| matches!(info.state, SweepState::Running | SweepState::Pending))
        .collect();
    match live.as_slice() {
        [] => Err(format!(
            "no running sweep found for issue #{issue}. List what the daemon is tracking with \
             `loom-daemon status`, or cancel by sweep id directly."
        )),
        [only] => Ok(only.sweep_id.clone()),
        many => Err(format!(
            "issue #{issue} has {} live sweeps ({}) — refusing to guess. Cancel by sweep id.",
            many.len(),
            many.iter()
                .map(|info| info.sweep_id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Handle the `cancel` subcommand (Issue #4980).
///
/// `--issue N` is resolved to a sweep id client-side via one extra `ListSweeps`
/// round-trip rather than by teaching the daemon a second cancel-addressing
/// mode: the wire protocol stays exactly the one `cancel_sweep` already speaks,
/// and there is no new server-side code path that could diverge from the MCP
/// tool's.
pub(crate) async fn handle_cancel_command(
    sweep_id: Option<String>,
    issue: Option<u32>,
    grace_secs: u64,
    workspace: Option<String>,
) -> Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let registry =
        loom_daemon::workspace_registry::WorkspaceRegistry::load_default().unwrap_or_default();
    let workspace = resolve_cli_dispatch_workspace(workspace, &cwd, &registry);

    let socket_path = resolve_socket_path()?;
    let ack_timeout = Duration::from_secs(grace_secs) + CANCEL_ACK_BUFFER;

    let sweep_id = match (sweep_id, issue) {
        (Some(id), _) => id,
        (None, Some(issue)) => {
            let list = Request::ListSweeps {
                state_filter: None,
                workspace_root: workspace.clone(),
                all_workspaces: false,
            };
            match query_daemon_bounded(&socket_path, &list, ack_timeout).await {
                Ok(Response::SweepList { sweeps }) => {
                    match select_sweep_for_issue(&sweeps, issue) {
                        Ok(id) => id,
                        Err(message) => {
                            eprintln!("{message}");
                            std::process::exit(1);
                        }
                    }
                }
                Ok(Response::Error { message }) => {
                    eprintln!("Daemon rejected the sweep lookup: {message}");
                    std::process::exit(1);
                }
                Ok(Response::StructuredError(err)) => {
                    eprintln!("Daemon rejected the sweep lookup: {}", err.message);
                    std::process::exit(1);
                }
                Ok(other) => {
                    eprintln!("Unexpected response from daemon: {other:?}");
                    std::process::exit(1);
                }
                Err(e) => {
                    daemon_unreachable(&e.to_string(), ack_timeout);
                }
            }
        }
        // clap enforces that one of the two is present; this is unreachable in
        // practice and stays a plain error rather than a panic.
        (None, None) => {
            eprintln!("Specify a sweep id or --issue <N>.");
            std::process::exit(2);
        }
    };

    let request = build_cancel_request(sweep_id.clone(), grace_secs, workspace);
    match query_daemon_bounded(&socket_path, &request, ack_timeout).await {
        Ok(Response::SweepCancelled {
            sweep_id,
            pid,
            sigkill_sent,
            was_running,
        }) => {
            if was_running {
                println!("Cancelled sweep {sweep_id}");
                println!("  pid:       {pid} (process group torn down)");
                println!(
                    "  escalated: {}",
                    if sigkill_sent {
                        "SIGKILL (did not exit within the grace window)"
                    } else {
                        "no — exited on SIGTERM"
                    }
                );
            } else {
                // Idempotent by design: a monitor/operator retry against a sweep
                // that already finished is a success, not an error.
                println!("Sweep {sweep_id} was already terminal — nothing to cancel (pid {pid}).");
            }
            Ok(())
        }
        Ok(Response::Error { message }) => {
            eprintln!("Daemon rejected the cancel: {message}");
            std::process::exit(1);
        }
        Ok(Response::StructuredError(err)) => {
            eprintln!("Daemon rejected the cancel: {}", err.message);
            std::process::exit(1);
        }
        Ok(other) => {
            eprintln!("Unexpected response from daemon: {other:?}");
            std::process::exit(1);
        }
        Err(e) => daemon_unreachable(&e.to_string(), ack_timeout),
    }
}

/// Render the "is the daemon running?" diagnostic and exit nonzero. Mirrors the
/// `dispatch` subcommand's wording so both operator surfaces fail the same way.
fn daemon_unreachable(error: &str, ack_timeout: Duration) -> ! {
    eprintln!(
        "Daemon did not ack the cancel within {}s ({error}) — is loom-daemon running?",
        ack_timeout.as_secs()
    );
    eprintln!();
    eprintln!("Start it with:");
    eprintln!("  ./.loom/scripts/cli/loom-daemon-start.sh");
    std::process::exit(1);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod cancel_tests {
    //! Tests for the `loom-daemon cancel` subcommand (Issue #4980): flag
    //! plumbing into the `CancelSweep` IPC request, `--issue N` → sweep-id
    //! selection, and a round-trip against a fake daemon proving the CLI puts
    //! the same frame on the wire the MCP `cancel_sweep` tool does.
    use super::{build_cancel_request, select_sweep_for_issue, DEFAULT_CANCEL_GRACE_SECS};
    use crate::cli::common::query_daemon_bounded;
    use chrono::Utc;
    use loom_daemon::types::{Request, Response, SweepInfo, SweepKind, SweepState};
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    fn sweep(sweep_id: &str, issue: u32, state: SweepState) -> SweepInfo {
        SweepInfo {
            sweep_id: sweep_id.to_string(),
            kind: SweepKind::Issue(issue),
            pid: 4242,
            pgid: Some(4242),
            token_name: "agent-1.token".to_string(),
            runtime: "claude".to_string(),
            runtime_source: None,
            log_path: std::path::PathBuf::from(".loom/logs/sweep.log"),
            idempotency_key: None,
            started_at: Utc::now(),
            state,
            latest_phase: None,
            pr_number: None,
            model: None,
            effort: None,
            depends_on: None,
            repo: None,
        }
    }

    /// `build_cancel_request` maps every flag into the right `CancelSweep`
    /// field — the core plumbing acceptance criterion.
    #[test]
    fn build_cancel_request_plumbs_all_flags() {
        let request = build_cancel_request(
            "sweep-issue-4980-1".to_string(),
            45,
            Some("/some/repo".to_string()),
        );
        match request {
            Request::CancelSweep {
                sweep_id,
                grace_secs,
                workspace_root,
            } => {
                assert_eq!(sweep_id, "sweep-issue-4980-1");
                assert_eq!(grace_secs, 45);
                assert_eq!(workspace_root.as_deref(), Some("/some/repo"));
            }
            other => panic!("expected CancelSweep, got {other:?}"),
        }
    }

    /// With no `--workspace` the request leaves `workspace_root` unset so the
    /// daemon's own default-workspace resolution applies, and the default grace
    /// matches the MCP tool's.
    #[test]
    fn build_cancel_request_defaults_match_the_mcp_tool() {
        let request = build_cancel_request("sweep-x".to_string(), DEFAULT_CANCEL_GRACE_SECS, None);
        match request {
            Request::CancelSweep {
                grace_secs,
                workspace_root,
                ..
            } => {
                assert_eq!(grace_secs, 30, "must match mcp-loom's cancel_sweep default");
                assert!(workspace_root.is_none());
            }
            other => panic!("expected CancelSweep, got {other:?}"),
        }
    }

    /// `--issue N` resolves to the one LIVE sweep for that issue.
    #[test]
    fn select_sweep_for_issue_picks_the_live_entry() {
        let sweeps = vec![
            sweep("sweep-other", 99, SweepState::Running),
            sweep(
                "sweep-old",
                4980,
                SweepState::Exited {
                    code: Some(0),
                    at: Utc::now(),
                },
            ),
            sweep("sweep-live", 4980, SweepState::Running),
        ];
        assert_eq!(select_sweep_for_issue(&sweeps, 4980).unwrap(), "sweep-live");
    }

    /// A terminal entry is NOT a candidate: those linger in the registry for an
    /// hour after exit, and "cancelling" one would ack success while whatever
    /// the operator is actually looking at keeps running.
    #[test]
    fn select_sweep_for_issue_ignores_terminal_entries() {
        let sweeps = vec![sweep(
            "sweep-dead",
            4980,
            SweepState::Crashed { at: Utc::now() },
        )];
        let err = select_sweep_for_issue(&sweeps, 4980).unwrap_err();
        assert!(err.contains("no running sweep"), "{err}");
    }

    /// Two live sweeps for one issue is itself a bug — refuse rather than guess,
    /// which would leave one running while reporting success.
    #[test]
    fn select_sweep_for_issue_refuses_to_guess_when_ambiguous() {
        let sweeps = vec![
            sweep("sweep-a", 4980, SweepState::Running),
            sweep("sweep-b", 4980, SweepState::Pending),
        ];
        let err = select_sweep_for_issue(&sweeps, 4980).unwrap_err();
        assert!(err.contains("refusing to guess"), "{err}");
        assert!(err.contains("sweep-a") && err.contains("sweep-b"), "{err}");
    }

    /// Full client round-trip: the CLI's frame arrives on the wire as the exact
    /// `CancelSweep` request the MCP `cancel_sweep` tool sends, and the
    /// `SweepCancelled` reply parses back with its outcome fields intact.
    #[tokio::test]
    async fn round_trip_sends_cancel_sweep_and_parses_the_outcome() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind");

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let (reader, mut writer) = stream.into_split();
            let mut lines = BufReader::new(reader).lines();
            let line = lines.next_line().await.expect("read").expect("line");
            let request: Request = serde_json::from_str(&line).expect("parse request");
            match request {
                Request::CancelSweep {
                    sweep_id,
                    grace_secs,
                    workspace_root,
                } => {
                    assert_eq!(sweep_id, "sweep-issue-4980-1");
                    assert_eq!(grace_secs, 30);
                    assert_eq!(workspace_root.as_deref(), Some("/repo"));
                }
                other => panic!("expected CancelSweep, got {other:?}"),
            }
            let response = Response::SweepCancelled {
                sweep_id: "sweep-issue-4980-1".to_string(),
                pid: 4242,
                sigkill_sent: true,
                was_running: true,
            };
            let json = serde_json::to_string(&response).expect("serialize");
            writer.write_all(json.as_bytes()).await.expect("write");
            writer.write_all(b"\n").await.expect("newline");
            writer.flush().await.expect("flush");
        });

        let request = build_cancel_request(
            "sweep-issue-4980-1".to_string(),
            DEFAULT_CANCEL_GRACE_SECS,
            Some("/repo".to_string()),
        );
        let response = query_daemon_bounded(&socket_path, &request, Duration::from_secs(5))
            .await
            .expect("round-trip ok");

        match response {
            Response::SweepCancelled {
                sweep_id,
                pid,
                sigkill_sent,
                was_running,
            } => {
                assert_eq!(sweep_id, "sweep-issue-4980-1");
                assert_eq!(pid, 4242);
                assert!(sigkill_sent);
                assert!(was_running);
            }
            other => panic!("expected SweepCancelled, got {other:?}"),
        }

        server.await.expect("server task");
    }

    /// A missing socket (no daemon listening at all) surfaces a connect error
    /// promptly rather than hanging — the operator's "is the daemon running?"
    /// case, and the reason this command can be trusted over ssh.
    #[tokio::test]
    async fn absent_socket_errors_fast() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("nonexistent.sock");

        let request = build_cancel_request("sweep-x".to_string(), 1, None);
        let result = query_daemon_bounded(&socket_path, &request, Duration::from_millis(500)).await;

        assert!(result.is_err(), "expected a connect error, got {result:?}");
    }
}
