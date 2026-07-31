//! `loom-daemon dispatch` handler (Issue #4712 — split out of `main.rs`).

use anyhow::Result;
use std::path::Path;
use std::path::PathBuf;

use loom_daemon::sweep_registry;
use loom_daemon::types::{Request, Response, SweepKind};

use super::common::{query_daemon_bounded, resolve_dispatch_ack_timeout, resolve_socket_path};

/// Build the `DispatchSweep` IPC request from the `dispatch` subcommand's args
/// (Issue #3952). Pure and side-effect-free so flag plumbing
/// (`--workspace`/`--model`/`--effort`/`--depends-on`) is unit-testable without
/// a socket. Mirrors the field mapping the `mcp__loom__dispatch_sweep` tool uses
/// so both operator surfaces enqueue byte-for-byte-equivalent requests.
fn build_dispatch_request(
    issue: u32,
    workspace: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    depends_on: Option<u32>,
    force: bool,
) -> Request {
    Request::DispatchSweep {
        kind: SweepKind::Issue(issue),
        // The daemon derives its own idempotency key from the issue when this is
        // absent; an operator dispatching by hand has no key to supply.
        idempotency_key: None,
        model,
        effort,
        depends_on,
        workspace_root: workspace,
        // Host-distress circuit-breaker override (#4235): default false, so a
        // tripped breaker refuses the dispatch unless the operator passes --force.
        force,
    }
}

/// Resolve the `dispatch` subcommand's effective `--workspace` (Issue #4299):
/// an explicit `--workspace` always wins; when absent, defaults it from the
/// CLI process's own `cwd` if `cwd` falls under a registered workspace root —
/// the daemon cannot see the client's cwd, so if that resolution is going to
/// happen at all it must happen client-side, before the `DispatchSweep`
/// request is built. This is what fixes `loom-daemon dispatch <N>` run from
/// inside a registered repo previously making no difference. A `cwd` outside
/// every registered root (or an empty registry) leaves the result `None`, and
/// the daemon's own registry-based resolution (`ipc::resolve_dispatch_registry`)
/// then applies.
///
/// Pure and side-effect-free — takes the already-resolved `cwd` and
/// already-loaded `registry` rather than performing I/O itself, so it is
/// unit-testable without touching the real filesystem/env; `cwd`/`registry`
/// are resolved once by [`handle_dispatch_command`] via `std::env::current_dir()`
/// / `WorkspaceRegistry::load_default()`.
fn resolve_cli_dispatch_workspace(
    explicit: Option<String>,
    cwd: &Path,
    registry: &loom_daemon::workspace_registry::WorkspaceRegistry,
) -> Option<String> {
    explicit.or_else(|| {
        loom_daemon::workspace_registry::resolve_client_workspace_default(cwd, registry)
            .map(|root| root.to_string_lossy().into_owned())
    })
}

/// Handle the `dispatch` subcommand (Issue #3952). Connects to the running
/// daemon over its Unix socket and enqueues a sweep via the same `DispatchSweep`
/// request the MCP `dispatch_sweep` tool uses — but with a bounded client-side
/// ack timeout so a wedged daemon can never hang the CLI (the #3945 failure
/// mode). On success prints the sweep id + per-sweep log path and exits 0.
///
/// See [`resolve_cli_dispatch_workspace`] for the `--workspace` default logic
/// applied here (Issue #4299).
pub(crate) async fn handle_dispatch_command(
    issue: u32,
    workspace: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    depends_on: Option<u32>,
    force: bool,
) -> Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let registry =
        loom_daemon::workspace_registry::WorkspaceRegistry::load_default().unwrap_or_default();
    let workspace = resolve_cli_dispatch_workspace(workspace, &cwd, &registry);

    let socket_path = resolve_socket_path()?;
    let request = build_dispatch_request(issue, workspace, model, effort, depends_on, force);
    let ack_timeout = resolve_dispatch_ack_timeout();

    match query_daemon_bounded(&socket_path, &request, ack_timeout).await {
        Ok(Response::SweepDispatched {
            sweep_id,
            pid,
            token_name,
            log_path,
        }) => {
            println!("Dispatched sweep for issue #{issue}");
            println!("  sweep id:  {sweep_id}");
            println!("  pid:       {pid}");
            // Surface a degraded token-name capture distinctly: the daemon fell
            // back to `UNKNOWN_TOKEN_NAME` because the child hadn't logged its
            // account-selection line within the capture window — a hint the
            // dispatch was slow on the daemon side, not that it failed.
            if token_name == sweep_registry::UNKNOWN_TOKEN_NAME {
                println!("  token:     {token_name} (account not captured before ack — slow child startup)");
            } else {
                println!("  token:     {token_name}");
            }
            println!("  log path:  {}", log_path.display());
            println!();
            println!("Tail the sweep log with:");
            println!("  tail -f {}", log_path.display());
            Ok(())
        }
        Ok(Response::Error { message }) => {
            eprintln!("Daemon rejected the dispatch: {message}");
            std::process::exit(1);
        }
        Ok(Response::StructuredError(err)) => {
            eprintln!("Daemon rejected the dispatch: {}", err.message);
            std::process::exit(1);
        }
        // Issue #4494: the typed capability-admission refusal is a REAL client
        // outcome, not an "unexpected response" — render the structured
        // role/runtime/source/unmet payload instead of discarding it, and exit
        // EX_CONFIG(78) so a script can tell a capability mismatch apart from a
        // daemon error (mirroring check-runtime-capabilities.sh's exit codes).
        Ok(Response::RuntimeRejected(rejection)) => {
            eprintln!("{}", rejection.diagnostic());
            std::process::exit(loom_daemon::runtime_admission::EX_CONFIG);
        }
        Ok(other) => {
            eprintln!("Unexpected response from daemon: {other:?}");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!(
                "Daemon did not ack the dispatch within {}s ({e}) — is loom-daemon running?",
                ack_timeout.as_secs()
            );
            eprintln!();
            eprintln!("Start it with:");
            eprintln!("  ./.loom/scripts/cli/loom-daemon-start.sh");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod dispatch_tests {
    //! Tests for the `loom-daemon dispatch` subcommand (Issue #3952): flag
    //! plumbing into the `DispatchSweep` IPC request, a successful round-trip
    //! against a fake daemon, and the bounded-timeout path against a
    //! deliberately-unresponsive socket (the #3945 wedge must never hang).
    use super::{build_dispatch_request, resolve_cli_dispatch_workspace};
    use crate::cli::common::{
        query_daemon_bounded, resolve_dispatch_ack_timeout, DAEMON_IPC_TIMEOUT_ENV,
        DISPATCH_ACK_TIMEOUT,
    };
    use loom_daemon::types::{Request, Response, SweepKind};
    use serial_test::serial;
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    /// `build_dispatch_request` maps every flag into the right `DispatchSweep`
    /// field — the core plumbing acceptance criterion.
    #[test]
    fn build_dispatch_request_plumbs_all_flags() {
        let request = build_dispatch_request(
            3952,
            Some("/some/repo".to_string()),
            Some("sonnet".to_string()),
            Some("high".to_string()),
            Some(3945),
            true,
        );
        match request {
            Request::DispatchSweep {
                kind,
                idempotency_key,
                model,
                effort,
                depends_on,
                workspace_root,
                force,
            } => {
                assert_eq!(kind, SweepKind::Issue(3952));
                assert_eq!(idempotency_key, None);
                assert_eq!(model.as_deref(), Some("sonnet"));
                assert_eq!(effort.as_deref(), Some("high"));
                assert_eq!(depends_on, Some(3945));
                assert_eq!(workspace_root.as_deref(), Some("/some/repo"));
                assert!(force, "--force must plumb through to the request");
            }
            other => panic!("expected DispatchSweep, got {other:?}"),
        }
    }

    /// With no optional flags the request carries only the issue kind and leaves
    /// every override `None`, so the daemon applies its own defaults.
    #[test]
    fn build_dispatch_request_defaults_are_none() {
        let request = build_dispatch_request(42, None, None, None, None, false);
        match request {
            Request::DispatchSweep {
                kind,
                model,
                effort,
                depends_on,
                workspace_root,
                force,
                ..
            } => {
                assert_eq!(kind, SweepKind::Issue(42));
                assert!(model.is_none());
                assert!(effort.is_none());
                assert!(depends_on.is_none());
                assert!(workspace_root.is_none());
                assert!(!force, "force defaults to false");
            }
            other => panic!("expected DispatchSweep, got {other:?}"),
        }
    }

    // ===== `resolve_cli_dispatch_workspace` (Issue #4299) =====

    /// An explicit `--workspace` always wins, regardless of cwd/registry state.
    #[test]
    fn resolve_cli_dispatch_workspace_explicit_always_wins() {
        let registry = loom_daemon::workspace_registry::WorkspaceRegistry::default();
        let resolved = resolve_cli_dispatch_workspace(
            Some("/explicit/repo".to_string()),
            std::path::Path::new("/somewhere/else"),
            &registry,
        );
        assert_eq!(resolved.as_deref(), Some("/explicit/repo"));
    }

    /// Issue #4299's core CLI fix: running `loom-daemon dispatch <N>` from
    /// inside a registered repo (no `--workspace` flag) must default
    /// `workspace_root` to that repo's root.
    #[test]
    fn resolve_cli_dispatch_workspace_defaults_from_cwd_inside_registered_root() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();

        let mut registry = loom_daemon::workspace_registry::WorkspaceRegistry::default();
        registry.add(&repo, None).unwrap();

        let canonical_repo = std::fs::canonicalize(&repo).unwrap();
        let resolved = resolve_cli_dispatch_workspace(None, &repo, &registry);
        assert_eq!(resolved, Some(canonical_repo.to_string_lossy().into_owned()));
    }

    /// A cwd outside every registered root (or an empty registry) leaves the
    /// result `None` — the daemon's own registry-based resolution applies.
    #[test]
    fn resolve_cli_dispatch_workspace_none_when_cwd_unregistered() {
        let dir = tempfile::tempdir().unwrap();
        let registry = loom_daemon::workspace_registry::WorkspaceRegistry::default();
        let resolved = resolve_cli_dispatch_workspace(None, dir.path(), &registry);
        assert_eq!(resolved, None);
    }

    /// A fake daemon that accepts one connection, verifies the received request
    /// is the expected `DispatchSweep`, and replies with `SweepDispatched`. This
    /// exercises the full client round-trip: request serialization + flag
    /// plumbing on the wire + response parsing.
    #[tokio::test]
    async fn round_trip_parses_dispatched_response_and_forwards_flags() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind");

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let (reader, mut writer) = stream.into_split();
            let mut lines = BufReader::new(reader).lines();
            let line = lines.next_line().await.expect("read").expect("line");
            let request: Request = serde_json::from_str(&line).expect("parse request");
            // Assert the flags arrived intact on the wire.
            match request {
                Request::DispatchSweep {
                    kind,
                    model,
                    effort,
                    depends_on,
                    workspace_root,
                    ..
                } => {
                    assert_eq!(kind, SweepKind::Issue(3952));
                    assert_eq!(model.as_deref(), Some("sonnet"));
                    assert_eq!(effort.as_deref(), Some("high"));
                    assert_eq!(depends_on, Some(3945));
                    assert_eq!(workspace_root.as_deref(), Some("/repo"));
                }
                other => panic!("expected DispatchSweep, got {other:?}"),
            }
            let response = Response::SweepDispatched {
                sweep_id: "sweep-abc".to_string(),
                pid: 12345,
                token_name: "agent-2.token".to_string(),
                log_path: std::path::PathBuf::from(".loom/logs/sweep-issue-3952.log"),
            };
            let json = serde_json::to_string(&response).expect("serialize");
            writer.write_all(json.as_bytes()).await.expect("write");
            writer.write_all(b"\n").await.expect("newline");
            writer.flush().await.expect("flush");
        });

        let request = build_dispatch_request(
            3952,
            Some("/repo".to_string()),
            Some("sonnet".to_string()),
            Some("high".to_string()),
            Some(3945),
            false,
        );
        let response = query_daemon_bounded(&socket_path, &request, Duration::from_secs(5))
            .await
            .expect("round-trip ok");

        match response {
            Response::SweepDispatched {
                sweep_id,
                pid,
                token_name,
                log_path,
            } => {
                assert_eq!(sweep_id, "sweep-abc");
                assert_eq!(pid, 12345);
                assert_eq!(token_name, "agent-2.token");
                assert_eq!(log_path, std::path::PathBuf::from(".loom/logs/sweep-issue-3952.log"));
            }
            other => panic!("expected SweepDispatched, got {other:?}"),
        }

        server.await.expect("server task");
    }

    /// Issue #4494: a fake daemon that refuses the dispatch with the typed
    /// `RuntimeRejected` response. The native `loom-daemon dispatch` client
    /// must MODEL that variant — parse it off the wire with its structured
    /// payload intact and render role/runtime/source/unmet — instead of
    /// falling into the generic "Unexpected response from daemon" path that
    /// discarded the whole diagnostic.
    #[tokio::test]
    async fn round_trip_parses_runtime_rejected_response_with_structured_payload() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind");

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let (reader, mut writer) = stream.into_split();
            let mut lines = BufReader::new(reader).lines();
            let _ = lines.next_line().await.expect("read").expect("line");
            let response =
                Response::RuntimeRejected(loom_daemon::runtime_admission::RuntimeRejection {
                    role: "sweep-lifecycle".to_string(),
                    runtime: "codex".to_string(),
                    source: loom_daemon::runtime_admission::RuntimeSource::DefaultConfig,
                    unmet_capabilities: vec!["worktreeIsolation".to_string()],
                    reason: "unmet capabilities: worktreeIsolation".to_string(),
                });
            let json = serde_json::to_string(&response).expect("serialize");
            writer.write_all(json.as_bytes()).await.expect("write");
            writer.write_all(b"\n").await.expect("newline");
            writer.flush().await.expect("flush");
        });

        let request = build_dispatch_request(4494, None, None, None, None, false);
        let response = query_daemon_bounded(&socket_path, &request, Duration::from_secs(5))
            .await
            .expect("round-trip ok");

        let Response::RuntimeRejected(rejection) = response else {
            panic!("expected RuntimeRejected, got {response:?}");
        };
        assert_eq!(rejection.role, "sweep-lifecycle");
        assert_eq!(rejection.runtime, "codex");
        assert_eq!(rejection.unmet_capabilities, vec!["worktreeIsolation"]);

        // The rendered operator diagnostic carries every required field.
        let diagnostic = rejection.diagnostic();
        for expected in [
            "sweep-lifecycle",
            "codex",
            "default-config",
            "worktreeIsolation",
        ] {
            assert!(diagnostic.contains(expected), "{diagnostic}");
        }
        // ...and the CLI's refusal exit code stays distinguishable from a
        // generic daemon error (EX_CONFIG, as the shell checker uses).
        assert_eq!(loom_daemon::runtime_admission::EX_CONFIG, 78);

        server.await.expect("server task");
    }

    /// An unresponsive daemon — accepts the connection but never replies —
    /// must trip the bounded round-trip timeout rather than hang (the #3945
    /// failure mode). The client returns an `Err` well before any 1800s wedge.
    #[tokio::test]
    async fn unresponsive_socket_times_out() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind");

        // Accept the connection but deliberately never write a response, holding
        // the stream open so the client's read blocks until its own timeout.
        let server = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.expect("accept");
            tokio::time::sleep(Duration::from_secs(30)).await;
        });

        let request = build_dispatch_request(3952, None, None, None, None, false);
        let started = std::time::Instant::now();
        let result = query_daemon_bounded(&socket_path, &request, Duration::from_millis(200)).await;
        let elapsed = started.elapsed();

        assert!(result.is_err(), "expected a timeout error, got {result:?}");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("timed out"), "error should mention the timeout, got: {msg}");
        // It must return promptly (well under a second), never hang.
        assert!(elapsed < Duration::from_secs(2), "timeout took too long: {elapsed:?}");

        server.abort();
    }

    /// A missing socket (no daemon listening at all) surfaces a connect error
    /// promptly — the operator's "is the daemon running?" case.
    #[tokio::test]
    async fn absent_socket_errors_fast() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("nonexistent.sock");

        let request = build_dispatch_request(3952, None, None, None, None, false);
        let result = query_daemon_bounded(&socket_path, &request, Duration::from_millis(500)).await;

        assert!(result.is_err(), "expected a connect error, got {result:?}");
    }

    /// The documented middle case (issue #3952 review): a **legitimate, slow**
    /// dispatch. The daemon does real synchronous work before acking — a
    /// `gh issue edit` label flip, up to a 2s dispatch stagger, and up to a 5s
    /// token-name capture window — so a successful ack can land well past the
    /// old hardcoded 5s client bound. This fake daemon sleeps *past* that old
    /// 5s bound before replying `SweepDispatched`, and the CLI's real default
    /// budget ([`DISPATCH_ACK_TIMEOUT`], 30s) must still parse it as the success
    /// it is — never misreport a real dispatch as "did not ack". Neither the
    /// instant-ack round-trip nor the permanently-unresponsive stub exercises
    /// this path.
    #[tokio::test]
    async fn slow_but_legitimate_ack_succeeds() {
        // Sleep beyond the *old* 5s hardcoded bound so this test would have
        // false-failed before the widening, proving the regression is fixed.
        const SLOW_ACK: Duration = Duration::from_millis(5_500);

        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind");

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let (reader, mut writer) = stream.into_split();
            let mut lines = BufReader::new(reader).lines();
            let _ = lines.next_line().await.expect("read").expect("line");
            // Model the daemon's synchronous pre-ack work (label flip + stagger +
            // token-name poll) taking longer than the retired 5s client bound.
            tokio::time::sleep(SLOW_ACK).await;
            let response = Response::SweepDispatched {
                sweep_id: "sweep-slow".to_string(),
                pid: 4242,
                token_name: "agent-1.token".to_string(),
                log_path: std::path::PathBuf::from(".loom/logs/sweep-issue-3952.log"),
            };
            let json = serde_json::to_string(&response).expect("serialize");
            writer.write_all(json.as_bytes()).await.expect("write");
            writer.write_all(b"\n").await.expect("newline");
            writer.flush().await.expect("flush");
        });

        let request = build_dispatch_request(3952, None, None, None, None, false);
        let started = std::time::Instant::now();
        // Use the CLI's real default budget — the exact value a plain
        // `loom-daemon dispatch` resolves to with no env override.
        let result = query_daemon_bounded(&socket_path, &request, DISPATCH_ACK_TIMEOUT).await;
        let elapsed = started.elapsed();

        match result {
            Ok(Response::SweepDispatched { sweep_id, .. }) => {
                assert_eq!(sweep_id, "sweep-slow");
            }
            other => panic!("expected a slow-but-successful SweepDispatched, got {other:?}"),
        }
        // The daemon genuinely took longer than the retired 5s bound — this
        // asserts the test exercised the slow path rather than acking instantly.
        assert!(
            elapsed >= Duration::from_secs(5),
            "slow-ack test should have waited past the old 5s bound, took: {elapsed:?}"
        );

        server.await.expect("server task");
    }

    /// The 30s floor is the default when the override env var is absent — real
    /// headroom over the daemon's documented worst-case internal dispatch budget.
    #[test]
    #[serial]
    fn resolve_dispatch_ack_timeout_defaults_to_floor() {
        std::env::remove_var(DAEMON_IPC_TIMEOUT_ENV);
        assert_eq!(resolve_dispatch_ack_timeout(), DISPATCH_ACK_TIMEOUT);
        assert_eq!(DISPATCH_ACK_TIMEOUT, Duration::from_secs(30));
    }

    /// `LOOM_DAEMON_IPC_TIMEOUT_MS` mirrors `mcp-loom`'s `Math.max` semantics:
    /// it can only *raise* the bound above the 30s floor (never lower it, which
    /// would reintroduce the false-negative), and any invalid / non-positive
    /// value falls back to the floor. A single test owns this env var so its
    /// mutation never races a parallel reader.
    #[test]
    #[serial]
    fn resolve_dispatch_ack_timeout_env_raises_only() {
        // Above the floor → raised.
        std::env::set_var(DAEMON_IPC_TIMEOUT_ENV, "60000");
        assert_eq!(resolve_dispatch_ack_timeout(), Duration::from_secs(60));

        // Below the floor → clamped up to the 30s floor (never lowered).
        std::env::set_var(DAEMON_IPC_TIMEOUT_ENV, "1000");
        assert_eq!(resolve_dispatch_ack_timeout(), DISPATCH_ACK_TIMEOUT);

        // Zero / negative / non-numeric / empty → floor.
        for bad in ["0", "-5", "garbage", "", "  "] {
            std::env::set_var(DAEMON_IPC_TIMEOUT_ENV, bad);
            assert_eq!(
                resolve_dispatch_ack_timeout(),
                DISPATCH_ACK_TIMEOUT,
                "value {bad:?} should fall back to the floor"
            );
        }

        std::env::remove_var(DAEMON_IPC_TIMEOUT_ENV);
    }
}
