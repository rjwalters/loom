//! `loom-daemon status` / `loom-daemon fleet status` client-side handling
//! (Issue #4712 — split out of `main.rs`): the reachability round-trip
//! (with its retry-classification), and the two entry points
//! (`handle_status_command`, `handle_fleet_status_command`) that render it.
//! Rendering itself (JSON/human tables, gate-verdict classification) lives
//! in `cli::status_render`.

use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use loom_daemon::daemon_install_state::{self, InstallStateReport};
use loom_daemon::self_update;
use loom_daemon::types::{DaemonStatusReport, Request, Response};

use super::common::resolve_socket_path;
use super::status_render::{build_status_json_value, print_status_human, print_status_json};
use super::tokens::resolve_tokens_workspace;

/// How a single [`query_daemon_status_once`] attempt failed (#4279), so the
/// caller retries ONLY the transient "daemon dropped the connection before
/// replying" case — never a clean "socket absent" or a slow-daemon timeout.
enum StatusAttemptError {
    /// Connect phase failed — socket absent, connection refused, or the connect
    /// itself timed out. A reconnect cannot help (the daemon is simply not
    /// listening), so this is never retried: fast-fail preserves the operator's
    /// "is the daemon running?" latency.
    Connect(anyhow::Error),
    /// The daemon accepted the connection but dropped it before writing a full
    /// response line — either a clean pre-response EOF or, on Linux, a RST that
    /// surfaces as a `ConnectionReset`/`BrokenPipe`/`UnexpectedEof` read/write
    /// error (see [`classify_roundtrip_error`]). This is the transient
    /// contention failure #4279 retries exactly once — under concurrent-sweep
    /// load a per-connection task can briefly drop a `status` connection that
    /// the very next one answers.
    DroppedBeforeReply(anyhow::Error),
    /// The round-trip failed for a non-transient reason: it timed out against a
    /// slow-but-live daemon (honor the single 5s budget rather than doubling it)
    /// or the response frame was malformed / an explicit daemon error. Retrying
    /// would not change the outcome, so it is not retried.
    Roundtrip(anyhow::Error),
}

impl StatusAttemptError {
    /// Unwrap to the underlying diagnostic surfaced to the operator.
    fn into_inner(self) -> anyhow::Error {
        match self {
            Self::Connect(e) | Self::DroppedBeforeReply(e) | Self::Roundtrip(e) => e,
        }
    }
}

/// Connect to the running daemon over its Unix socket, send a single
/// `DaemonStatus` request, and return the parsed report (Issue #3891).
///
/// Both the connect and the round-trip are individually bounded so an
/// unresponsive/wedged daemon cannot hang the CLI. A single bounded reconnect
/// retry (#4279) absorbs a transient dropped connection — a daemon under
/// concurrent-sweep load can accept then close a `status` connection with zero
/// bytes written, which the client would otherwise surface as a bare EOF that a
/// stdout-capturing monitor misreads as an empty status. A clean "socket absent"
/// or a slow-daemon timeout is deliberately NOT retried. Errors (after the one
/// retry, where applicable) when the daemon is unreachable or the response is
/// malformed.
async fn query_daemon_status(socket_path: &Path) -> Result<DaemonStatusReport> {
    const TIMEOUT: Duration = Duration::from_secs(5);

    match query_daemon_status_once(socket_path, TIMEOUT).await {
        Ok(report) => Ok(report),
        Err(StatusAttemptError::DroppedBeforeReply(_first)) => {
            // One bounded reconnect retry — the transient case only.
            query_daemon_status_once(socket_path, TIMEOUT)
                .await
                .map_err(StatusAttemptError::into_inner)
        }
        Err(other) => Err(other.into_inner()),
    }
}

/// A single connect + `DaemonStatus` round-trip attempt, classifying any failure
/// so [`query_daemon_status`] can decide whether to retry (#4279).
async fn query_daemon_status_once(
    socket_path: &Path,
    timeout: Duration,
) -> std::result::Result<DaemonStatusReport, StatusAttemptError> {
    let stream = tokio::time::timeout(timeout, UnixStream::connect(socket_path))
        .await
        .map_err(|_| {
            StatusAttemptError::Connect(anyhow!("connect timed out after {}s", timeout.as_secs()))
        })?
        .map_err(|e| StatusAttemptError::Connect(anyhow!("connect failed: {e}")))?;
    let (reader, mut writer) = stream.into_split();

    // The round-trip yields `Ok(None)` on a clean pre-response EOF (the retryable
    // drop), `Ok(Some(report))` on success, and `Err(_)` for either a
    // malformed/error frame OR a pre-response read/write I/O error (e.g. the
    // Linux RST drop) — the timeout wrapper below routes each `Err(_)` through
    // `classify_roundtrip_error` to decide whether it is the retryable drop.
    let roundtrip = async move {
        let request_json = serde_json::to_string(&Request::DaemonStatus)?;
        writer.write_all(request_json.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;

        let mut lines = BufReader::new(reader).lines();
        match lines.next_line().await? {
            None => Ok(None),
            Some(line) => {
                let response: Response = serde_json::from_str(&line)?;
                match response {
                    // `Response::DaemonStatus` is boxed (issue #4292); unbox
                    // here so the retry-aware `Result<Option<DaemonStatusReport>>`
                    // signature (and its callers' field accesses) stays unchanged.
                    Response::DaemonStatus(report) => Ok(Some(*report)),
                    Response::Error { message } => Err(anyhow!("daemon error: {message}")),
                    other => Err(anyhow!("unexpected response: {other:?}")),
                }
            }
        }
    };

    match tokio::time::timeout(timeout, roundtrip).await {
        Err(_elapsed) => Err(StatusAttemptError::Roundtrip(anyhow!(
            "status round-trip timed out after {}s",
            timeout.as_secs()
        ))),
        Ok(Err(e)) => Err(classify_roundtrip_error(e)),
        Ok(Ok(None)) => Err(StatusAttemptError::DroppedBeforeReply(anyhow!(
            "daemon closed the connection without responding"
        ))),
        Ok(Ok(Some(report))) => Ok(report),
    }
}

/// Classify a round-trip `Err` from [`query_daemon_status_once`]'s I/O closure as
/// retryable or not (#4279). A read/write I/O error that fires before a full
/// response line arrived is the SAME transient drop as a clean pre-response EOF:
/// on Linux a peer that closes the socket with unread request bytes still queued
/// in its kernel receive buffer replies with RST, so the client's read surfaces
/// `ConnectionReset` (os error 104) instead of the clean EOF macOS reports — both
/// mean "the daemon dropped us before replying". `ConnectionReset`, `BrokenPipe`,
/// and `UnexpectedEof` are therefore reclassified as the retryable
/// [`StatusAttemptError::DroppedBeforeReply`] (reusing the same friendly
/// diagnostic as the EOF path so the operator message is platform-independent).
/// Malformed-JSON responses and explicit `Response::Error` replies are NOT
/// `io::Error`s, so they stay non-retryable [`StatusAttemptError::Roundtrip`].
fn classify_roundtrip_error(e: anyhow::Error) -> StatusAttemptError {
    let dropped_before_reply = e.downcast_ref::<std::io::Error>().is_some_and(|io_err| {
        matches!(
            io_err.kind(),
            std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::UnexpectedEof
        )
    });
    if dropped_before_reply {
        StatusAttemptError::DroppedBeforeReply(anyhow!(
            "daemon closed the connection without responding"
        ))
    } else {
        StatusAttemptError::Roundtrip(e)
    }
}

/// Collect per-token usage via an in-process call to
/// [`loom_daemon::tokens_pool::check::run_check`] — the same native probe
/// `loom-daemon tokens check --json` (`TokensAction::Check`) runs, called
/// directly rather than shelled out to (issue #4080, epic #4081 Phase 2).
/// `loom-daemon status` runs client-side with no supervision requirement, and
/// the probe code is already linked into this binary, so there is no reason
/// to pay a subprocess round-trip the way the historical `loom-tokens` /
/// `python3 -m` two-tier shell-out did. Best-effort — never panics, never
/// propagates an error.
///
/// `tokens_dir` is the pool directory to probe — pass the daemon's own
/// [`DaemonStatusReport::token_pool_dir`] (issue #4292), not a directory
/// re-resolved from this *client* process's own cwd. Before #4292 this probed
/// `resolve_tokens_workspace(".")` independently, so `loom-daemon status` run
/// from a directory other than the daemon's own workspace could report a
/// stale/false (e.g. 0/0 healthy) token picture even though the *daemon*
/// itself had a perfectly healthy pool — the CLI and the daemon disagreed on
/// which pool "the" pool was. Falling back to `None` only when the daemon
/// report predates #4292 keeps the pre-existing cwd-based behavior for that
/// one legacy case (an old daemon binary talking to a newer CLI).
fn collect_token_usage(tokens_dir: Option<&Path>) -> Option<serde_json::Value> {
    use loom_daemon::tokens_pool::check::{
        self, CheckOptions, CurlTransport, DEFAULT_PROBE_MODEL, DEFAULT_PROBE_PROMPT,
    };

    let tokens_dir = match tokens_dir {
        Some(dir) => dir.to_path_buf(),
        None => {
            let ws = resolve_tokens_workspace(".").ok()?;
            loom_daemon::tokens_pool::paths::resolve_tokens_dir(&ws)
        }
    };

    let opts = CheckOptions {
        source: check::resolve_source(None),
        write_ranking: false,
        probe_prompt: DEFAULT_PROBE_PROMPT,
        model: DEFAULT_PROBE_MODEL,
        stagger: true,
    };
    let report = check::run_check(&tokens_dir, &opts, &CurlTransport);
    Some(report.to_json())
}

/// Handle the `status` subcommand — render the running daemon's autonomous-mode
/// operability snapshot (Issue #3891). Fetches the daemon-native part over IPC
/// and layers on the client-side per-token usage probe.
///
/// `pipeline` opts into the forge-side pipeline snapshot (Issue #3977): per
/// managed repo, open `loom:issue`/`loom:building` counts, open PR counts by
/// review-state label, and PRs merged in the last 24h. Like the per-token
/// usage table, this is collected client-side (several `gh` calls per repo)
/// rather than inside the IPC handler, and is opt-in specifically because
/// those extra forge calls are too slow to bundle into the default view.
pub(crate) async fn handle_status_command(json: bool, pipeline: bool) -> Result<()> {
    let socket_path = resolve_socket_path()?;

    let report = match query_daemon_status(&socket_path).await {
        Ok(report) => report,
        Err(e) => {
            // Issue #4069 (AC3 of #4011): classify WHY the daemon is
            // unreachable using the same autonomy-desired marker + heartbeat
            // `loom-daemon-watchdog.sh` reads, so `status` and the watchdog
            // log can never disagree. Purely local, read-only, never fails
            // the command — `install_state` is `None` only when no loom dir
            // can be resolved at all, in which case we fall back to the
            // pre-#4069 generic message.
            let install_state = daemon_install_state::probe();
            let exit_code = install_state
                .as_ref()
                .map_or(daemon_install_state::EXIT_NOT_EXPECTED, |r| r.state.exit_code());
            if json {
                print_status_unreachable_json(&socket_path, &e, install_state.as_ref())?;
            } else {
                print_status_unreachable_human(&socket_path, &e, install_state.as_ref());
            }
            std::process::exit(exit_code);
        }
    };

    // Per-token usage is a slow per-account network probe the daemon deliberately
    // does NOT perform inside the IPC handler; collect it client-side here —
    // but against the SAME pool directory the daemon itself resolved (#4292),
    // not one independently re-derived from this CLI invocation's own cwd.
    let token_usage = collect_token_usage(report.token_pool_dir.as_deref());

    // Self-update staleness (#3968): purely local, read-only — compares the
    // commit baked into THIS `loom-daemon status` binary against the source
    // checkout's current HEAD, when that checkout is still on this machine.
    // Advisory only; never triggers a rebuild or restart (see
    // `.loom/scripts/cli/loom-daemon-update.sh` for the opt-in update flow).
    let update = self_update::check();

    // Forge-side pipeline snapshot (#3977) — opt-in, fetched in the same
    // priority order as `report.per_repo` so the rendered table lines up with
    // the "Managed repos" dispatch table above it.
    let pipeline_snapshots = if pipeline {
        let roots: Vec<PathBuf> = report.per_repo.iter().map(|r| r.root.clone()).collect();
        let source = Arc::new(loom_daemon::pipeline_snapshot::GhPipelineSource::new());
        Some(loom_daemon::pipeline_snapshot::collect_pipeline_snapshots(source, roots).await)
    } else {
        None
    };

    // Watchdog protection state (#4354, AC4 of #4331) — the REACHABLE-path
    // counterpart to the `install_state` classification above. A healthy daemon
    // answering over IPC can still be unprotected: no autonomy-desired marker
    // (crash protection disarmed) or no watchdog job/timer provisioned (nothing
    // scheduled to notice a future death). Both facts are host-local and visible
    // to this CLI process, so nothing is plumbed through the IPC report.
    // Read-only, and never fails the command: an unanswerable probe degrades to
    // `unknown` and `status` still exits 0.
    let protection = daemon_install_state::probe_protection();

    if json {
        print_status_json(
            &report,
            token_usage.as_ref(),
            &update,
            pipeline_snapshots.as_deref(),
            protection.as_ref(),
        )?;
    } else {
        print_status_human(
            &report,
            token_usage.as_ref(),
            &update,
            pipeline_snapshots.as_deref(),
            protection.as_ref(),
        );
    }
    Ok(())
}

/// Emit the unreachable-daemon `--json` error, state-aware (Issue #4069). The
/// existing `error` prose key is retained for compatibility; `install_state`
/// (when the probe could classify at all) adds a machine-readable enum plus
/// the diagnostic fields a script or human can act on.
fn print_status_unreachable_json(
    socket_path: &Path,
    err: &anyhow::Error,
    install_state: Option<&InstallStateReport>,
) -> Result<()> {
    let mut payload = serde_json::json!({
        "error": format!("could not reach loom-daemon at {}: {err}", socket_path.display()),
    });
    if let Some(r) = install_state {
        payload["install_state"] = serde_json::json!({
            "state": r.state.as_str(),
            "started_at": r.started_at,
            "pid": r.pid,
            "liveness_detail": r.liveness_detail,
            "heartbeat": {
                "freshness": r.heartbeat_freshness.map(daemon_install_state::HeartbeatFreshness::as_str),
                "age_secs": r.heartbeat_age_secs,
                "stale_threshold_secs": r.heartbeat_stale_threshold_secs,
            },
            "process_age_secs": r.process_age_secs,
            "startup_grace_threshold_secs": r.startup_grace_threshold_secs,
            "watchdog_log": r.watchdog_log_path.display().to_string(),
        });
    }
    println!("{}", serde_json::to_string_pretty(&payload)?);
    Ok(())
}

/// Emit the unreachable-daemon human-readable error, state-aware (Issue
/// #4069). Remediation advice differs per state: `NotExpected` /
/// `ExpectedButDead` suggest a start; `AliveStarting` (#4213) reports a normal
/// in-progress startup and prints NO remediation; `AliveButUnresponsive` does
/// NOT suggest a start either (the singleton guard would refuse it) and instead
/// points at the live pid.
fn print_status_unreachable_human(
    socket_path: &Path,
    err: &anyhow::Error,
    install_state: Option<&InstallStateReport>,
) {
    eprintln!("Could not reach loom-daemon at {}: {err}", socket_path.display());
    eprintln!();

    match install_state {
        None => {
            // Undiagnosable (no loom dir could be resolved) — the pre-#4069
            // generic fallback.
            eprintln!("Is the daemon running? Start it with:");
            eprintln!("  ./.loom/scripts/cli/loom-daemon-start.sh");
        }
        Some(r) => match r.state {
            daemon_install_state::InstallState::NotExpected => {
                eprintln!(
                    "No autonomy-desired marker found — a daemon is not currently expected \
                     to be running on this host."
                );
                eprintln!();
                eprintln!("Start it with:");
                eprintln!("  ./.loom/scripts/cli/loom-daemon-start.sh");
            }
            daemon_install_state::InstallState::ExpectedButDead => {
                let started = r.started_at.as_deref().unwrap_or("unknown");
                eprintln!(
                    "A daemon is EXPECTED (autonomy-desired marker present, started {started}) \
                     but is NOT running: {}.",
                    r.liveness_detail.as_deref().unwrap_or("no liveness detail")
                );
                eprintln!(
                    "Autonomous dispatch has stopped — this is the silent-autonomy-loss \
                     scenario (#4011)."
                );
                eprintln!();
                eprintln!("Recover with:");
                eprintln!("  ./.loom/scripts/cli/loom-daemon-start.sh");
                eprintln!("See {} for prior divergence reports.", r.watchdog_log_path.display());
            }
            daemon_install_state::InstallState::AliveStarting => {
                let detail = r.liveness_detail.as_deref().unwrap_or("process alive");
                let grace = r.startup_grace_threshold_secs.unwrap_or_default();
                eprintln!("The daemon process IS alive ({detail}) but is not responding over IPC.");
                eprintln!(
                    "It is still STARTING (process age {}s ≤ {grace}s grace) — its IPC socket has \
                     not bound yet (normal for up to ~{grace}s after a bootout/bootstrap restart).",
                    r.process_age_secs.unwrap_or_default()
                );
                eprintln!();
                eprintln!(
                    "This is NOT a fault — no action needed. Re-run `loom-daemon status` in a few \
                     seconds; the socket should bind and status will succeed."
                );
                // Deliberately NOT printing the stop/start remediation: doing so
                // during every normal restart is exactly the ghost-chase #4213
                // set out to prevent.
            }
            daemon_install_state::InstallState::AliveButUnresponsive => {
                let detail = r.liveness_detail.as_deref().unwrap_or("process alive");
                eprintln!("The daemon process IS alive ({detail}) but is not responding over IPC.");
                match r.heartbeat_freshness {
                    Some(daemon_install_state::HeartbeatFreshness::Fresh) => {
                        eprintln!(
                            "Heartbeat is fresh ({}s ago) — likely an IPC/socket-layer fault, \
                             not a wedged daemon.",
                            r.heartbeat_age_secs.unwrap_or_default()
                        );
                    }
                    Some(daemon_install_state::HeartbeatFreshness::Stale) => {
                        eprintln!(
                            "Heartbeat is STALE ({}s ago, > {}s threshold) — the daemon is likely \
                             wedged.",
                            r.heartbeat_age_secs.unwrap_or_default(),
                            r.heartbeat_stale_threshold_secs.unwrap_or_default()
                        );
                    }
                    Some(daemon_install_state::HeartbeatFreshness::PriorBoot) => {
                        eprintln!(
                            "Heartbeat file is from a PREVIOUS boot ({}s old; this process is \
                             only {}s old) — it is not evidence about the current process. (A \
                             daemon that wedged before writing its first heartbeat this boot \
                             would look identical — re-check after the process is well past \
                             startup if you still suspect a wedge.)",
                            r.heartbeat_age_secs.unwrap_or_default(),
                            r.process_age_secs.unwrap_or_default()
                        );
                    }
                    _ => {
                        eprintln!(
                            "Heartbeat status unknown (no heartbeat file, or disabled) — \
                             liveness-only signal."
                        );
                    }
                }
                eprintln!();
                // Advice gating (#4368): the imperative stop/start remediation
                // is only warranted for a *current-boot* Stale verdict — the
                // one case where the evidence actually points at a wedge.
                // Fresh/Unknown/PriorBoot get inspect-first guidance instead,
                // so an operator is never steered into restarting a daemon
                // that is merely mid-fault-diagnosis or missing heartbeat
                // evidence, not actually wedged.
                if r.heartbeat_freshness == Some(daemon_install_state::HeartbeatFreshness::Stale) {
                    if let Some(pid) = r.pid {
                        eprintln!(
                            "Do NOT run loom-daemon-start.sh — the singleton guard will refuse"
                        );
                        eprintln!(
                            "while pid {pid} is alive. Inspect it directly, or restart explicitly:"
                        );
                    } else {
                        eprintln!(
                            "Do NOT run loom-daemon-start.sh — the singleton guard will refuse"
                        );
                        eprintln!(
                            "while the daemon is alive. Inspect it directly, or restart explicitly:"
                        );
                    }
                    eprintln!("  ./.loom/scripts/cli/loom-daemon-stop.sh && ./.loom/scripts/cli/loom-daemon-start.sh");
                } else {
                    eprintln!("Inspect before acting — this evidence does not indicate a wedge:");
                    if let Some(pid) = r.pid {
                        eprintln!(
                            "  ps -p {pid} -o pid,etime,command   # confirm what it is actually doing"
                        );
                    }
                    eprintln!("  loom-daemon status --json           # machine-readable detail");
                    eprintln!("If it is still unresponsive after inspecting, restart explicitly:");
                    eprintln!("  ./.loom/scripts/cli/loom-daemon-stop.sh && ./.loom/scripts/cli/loom-daemon-start.sh");
                }
            }
        },
    }
}

/// Handle `loom-daemon fleet status` (#4342, epic #4340): collect the local
/// host's own status in-process, fan out to every registered fleet worker
/// concurrently, merge, render, and exit non-zero unless every roster host is
/// `UP`. Thin clap→module wiring: the merge/render/exit-code logic lives in
/// [`loom_daemon::fleet::status`]; only the local-host collection (which needs
/// this binary's own socket/install-state machinery) lives here.
pub(crate) async fn handle_fleet_status_command(json: bool) -> Result<()> {
    use loom_daemon::fleet::status::{
        collect_fleet_report, update_last_seen_up_at, SshStatusSource,
    };
    use loom_daemon::fleet::FleetRegistry;

    // `collect_fleet_report` consumes its registry argument (it owns each
    // `WorkerRecord` into the concurrent per-host collection).
    let registry = FleetRegistry::load_default()?;
    let local = collect_local_fleet_report().await;
    let source: Arc<dyn loom_daemon::fleet::status::HostStatusSource> =
        Arc::new(SshStatusSource::new());
    let timeout = Duration::from_secs(loom_daemon::fleet::status::DEFAULT_TIMEOUT_SECS);
    let report = collect_fleet_report(source, registry, local, timeout).await;

    // #4697: persist "last observed Up" so a LATER poll that finds this host
    // Unreachable can measure elapsed silence against its configured
    // idle-shutdown window (the expected-power-off heuristic in
    // `loom_daemon::fleet::status`).
    //
    // Deliberately RE-LOADS the registry rather than mutating the copy fanned
    // out above: the SSH collection can take up to DEFAULT_TIMEOUT_SECS, and
    // `fleet status` is now a writer of a file other commands (`add-worker`,
    // `drain`) also write. Saving the pre-collection snapshot would silently
    // clobber anything they committed during that window; re-loading narrows
    // the lost-update window to the few milliseconds between here and `save`.
    //
    // Best-effort throughout: neither a failed re-load nor a failed save may
    // block reporting/exiting on the status the operator actually asked for.
    match FleetRegistry::load_default() {
        Ok(mut fresh) => {
            if update_last_seen_up_at(&mut fresh, &report, chrono::Utc::now()) {
                if let Err(e) = fresh.save_default() {
                    eprintln!(
                        "warning: could not persist fleet registry last-seen-up timestamp(s): {e}"
                    );
                }
            }
        }
        Err(e) => {
            eprintln!("warning: could not re-read the fleet registry to record last-seen-up: {e}");
        }
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", report.render_human());
    }
    std::process::exit(report.exit_code());
}

/// Collect the local host's own [`loom_daemon::fleet::status::HostReport`] —
/// in-process, over the daemon's Unix socket (never `ssh localhost`, per
/// #4342's implementation guidance). Reuses the exact same
/// [`build_status_json_value`] payload shape `loom-daemon status --json`
/// emits, so the local row's fields line up with every remote host's
/// self-reported `status --json` (#4069's unreachable-daemon classification is
/// reused for the down case too).
async fn collect_local_fleet_report() -> loom_daemon::fleet::status::HostReport {
    use loom_daemon::fleet::status::HostReport;

    let socket_path = match resolve_socket_path() {
        Ok(path) => path,
        Err(e) => {
            return HostReport::local_down(format!("could not resolve daemon socket path: {e}"))
        }
    };

    match query_daemon_status(&socket_path).await {
        Ok(daemon_report) => {
            let token_usage = collect_token_usage(daemon_report.token_pool_dir.as_deref());
            let update = self_update::check();
            // Same host-local protection probe the reachable `status` path runs
            // (#4354), so the local fleet row carries the `protection` field a
            // remote host's own `status --json` would self-report — payload-shape
            // parity is the whole point of sharing this builder.
            let protection = daemon_install_state::probe_protection();
            let value = build_status_json_value(
                &daemon_report,
                token_usage.as_ref(),
                &update,
                None,
                protection.as_ref(),
            );
            HostReport::local_up(value)
        }
        Err(e) => {
            // Reuse the #4069 install-state classification so the local row's
            // "why is it down" detail matches what `loom-daemon status --json`
            // itself would report for this same daemon.
            let install_state = daemon_install_state::probe();
            let detail = match install_state {
                Some(r) => format!("{} ({e})", r.state.as_str()),
                None => format!("daemon unreachable: {e}"),
            };
            HostReport::local_down(detail)
        }
    }
}

#[cfg(test)]
pub(crate) mod status_client_tests {
    //! empty-output failure mode. A daemon under concurrent-sweep load could
    //! accept a `status` connection and drop it with zero bytes written; the
    //! client surfaced that as a bare EOF. These tests lock in the two
    //! invariants: (1) an EOF (accept-then-close) yields a non-zero-worthy
    //! `Err` with a diagnostic — never an empty success — and (2) a single
    //! reconnect retry absorbs a transient first-connection drop.
    use super::query_daemon_status;
    use chrono::Utc;
    use loom_daemon::types::{
        CapacityReport, CredentialPreflightReport, DaemonStatusReport, Response,
    };
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    /// A fully-populated report the fake daemon can serialize back to the client
    /// on a successful round-trip. Every field is compiler-checked, so a schema
    /// change surfaces here rather than as a silently-skewed wire payload.
    ///
    /// `pub(super)` so the reachable-path status-rendering tests (#4354) build
    /// their payload from this same fixture instead of duplicating the whole
    /// struct.
    pub(crate) fn sample_report() -> DaemonStatusReport {
        DaemonStatusReport {
            in_flight: vec![],
            unregistered_locked: vec![],
            token_pool_size: 4,
            token_pool_dir: Some(std::path::PathBuf::from("/repo/a/.loom/tokens")),
            disk_headroom: 10,
            logical_cpus: 8,
            loadavg_1m: Some(1.25),
            cpu_idle_fraction: Some(0.90),
            capacity_bound: false,
            preflight_advisory_active: false,
            preflight_advisory_message: None,
            configured_max: 5,
            per_token_concurrency: 2,
            dynamic_cap: 3,
            main_health_gate_halted: false,
            main_health_gate_not_evaluated: false,
            main_health_gate_not_evaluated_reason: None,
            main_health_gate_enabled: Some(true),
            main_health_gate_verdict_at: Some(Utc::now()),
            main_health_gate_deferred: false,
            main_health_gate_deferred_reason: None,
            main_health_gate_verdict_tier: None,
            capacity: CapacityReport {
                ranking_present: true,
                total_accounts: 4,
                healthy_accounts: 3,
                exhausted_accounts: 1,
                token_axis_limit: 3,
                token_bound: true,
            },
            per_repo: vec![],
            credential_preflight: Some(CredentialPreflightReport {
                ok: true,
                mechanism: "test-fixture".to_string(),
                fingerprint: None,
                message: "test fixture — not a real preflight".to_string(),
                checked_at: Utc::now(),
            }),
            draining: false,
            drain_deadline: None,
            drain_note: None,
            auto_update_enabled: false,
            auto_update_last_check: None,
            auto_update_last_roll: None,
            auto_update_consecutive_failures: 0,
            auto_update_backoff_secs: None,
            auto_update_terminal_reason: None,
            auto_update_note: None,
            host_breaker: None,
            admission_brake: None,
            rate_limit_breaker: None,
            safehouse: None,
            work_finder_enabled: Some(true),
            last_work_finder_tick: None,
            role_tick_records: vec![],
            daemon_pid: Some(99917),
            pid_file: Some(std::path::PathBuf::from("/repo/a/.loom/.daemon.pid")),
            daemon_build_commit: Some("18887b5c".to_string()),
            work_finder_interval_secs: Some(60),
            observability_host_id_mismatch: None,
        }
    }

    /// The core #4279 invariant: a daemon that accepts the connection and closes
    /// it before writing any response byte (the silent-EOF failure mode) must
    /// surface an `Err` with a diagnostic — never an empty "successful" report.
    /// The client retries once, so the fake server drops BOTH connections.
    #[tokio::test]
    async fn status_eof_yields_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind");

        // Accept and immediately drop every connection (the initial attempt plus
        // the one bounded reconnect retry).
        let server = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                drop(stream);
            }
        });

        let started = std::time::Instant::now();
        let result = query_daemon_status(&socket_path).await;
        let elapsed = started.elapsed();

        assert!(result.is_err(), "accept-then-close must be an error, not an empty success");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("closed the connection without responding"),
            "error should name the dropped connection, got: {msg}"
        );
        // Both attempts hit an immediate EOF, so this returns promptly — the
        // reconnect retry never stretches into the 5s round-trip budget.
        assert!(elapsed < Duration::from_secs(2), "EOF path took too long: {elapsed:?}");

        server.abort();
    }

    /// The single-reconnect acceptance criterion: the daemon drops the first
    /// `status` connection (transient contention) but answers the second. The
    /// client's one bounded retry must absorb the drop and return the report.
    #[tokio::test]
    async fn status_retry_succeeds_after_transient_drop() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind");

        let server = tokio::spawn(async move {
            // First connection: accept and drop without replying.
            let (first, _) = listener.accept().await.expect("accept #1");
            drop(first);

            // Second connection: read the request line, then reply with a valid
            // DaemonStatus frame.
            let (stream, _) = listener.accept().await.expect("accept #2");
            let (reader, mut writer) = stream.into_split();
            let mut lines = BufReader::new(reader).lines();
            let _ = lines
                .next_line()
                .await
                .expect("read")
                .expect("request line");
            let json = serde_json::to_string(&Response::DaemonStatus(Box::new(sample_report())))
                .expect("serialize response");
            writer.write_all(json.as_bytes()).await.expect("write");
            writer.write_all(b"\n").await.expect("newline");
            writer.flush().await.expect("flush");
        });

        let result = query_daemon_status(&socket_path).await;
        match result {
            Ok(report) => assert_eq!(report.token_pool_size, 4),
            Err(e) => panic!("retry should have absorbed the first-connection drop, got: {e}"),
        }

        server.await.expect("server task");
    }

    /// A missing socket (no daemon listening at all) must fail fast WITHOUT a
    /// reconnect retry — a clean "socket absent" is not the transient case, so
    /// retrying would only fail twice for no benefit.
    #[tokio::test]
    async fn status_absent_socket_errors_fast() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("nonexistent.sock");

        let started = std::time::Instant::now();
        let result = query_daemon_status(&socket_path).await;
        let elapsed = started.elapsed();

        assert!(result.is_err(), "expected a connect error for an absent socket");
        assert!(
            elapsed < Duration::from_secs(2),
            "absent-socket path took too long: {elapsed:?}"
        );
    }
}
