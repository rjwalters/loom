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
use super::status_render::{
    autonomy_mismatch, build_status_json_value, print_status_human, print_status_json,
};
use super::tokens::resolve_tokens_workspace;

/// Default status IPC round-trip budget on an unloaded host (Issue #3891).
/// Kept as a named constant (rather than inline in [`resolve_status_timeout`])
/// so [`scale_timeout_for_load`]'s doc/tests can refer to "the base" by name.
const DEFAULT_STATUS_TIMEOUT: Duration = Duration::from_secs(5);

/// Hard ceiling on the load-scaled status timeout (Issue #6011 AC1): even an
/// absurd load reading must not hang the CLI indefinitely — this mirrors
/// `cli::common::DISPATCH_ACK_TIMEOUT`'s own bounded-but-generous budget for
/// the identical "give a loaded daemon real headroom, but never forever"
/// tradeoff.
const MAX_SCALED_STATUS_TIMEOUT: Duration = Duration::from_secs(30);

/// The status IPC timeout actually used for one `status` invocation, plus the
/// host-load reading it was (or was not) derived from — Issue #6011 AC2: the
/// timeout should scale with observed host load, *or* the message should
/// report the measured load alongside the timeout, so an operator can see why
/// it fired rather than just watching a bigger number time out.
pub(crate) struct StatusTimeoutInfo {
    pub timeout: Duration,
    pub loadavg_1m: Option<f64>,
    pub load_per_core: Option<f64>,
    pub logical_cpus: usize,
}

/// Scale `base` by observed load-per-core (Issue #6011 AC2). Below 1.0
/// load-per-core (host not saturated) the timeout is left unchanged — most
/// invocations, on a healthy host, pay zero extra latency budget. At or above
/// it, the timeout grows linearly with load-per-core (a host at load 2×/core
/// gets roughly 2× the round-trip budget), capped at
/// [`MAX_SCALED_STATUS_TIMEOUT`] so a corrupted or pathological load reading
/// can never hang the CLI indefinitely.
///
/// `None` load-per-core (no reading available — an unsupported platform, or a
/// transient read failure) leaves `base` unchanged: mirrors
/// [`loom_daemon::cpu_headroom`]'s fail-open convention that absent evidence
/// is never treated as "the host is loaded".
#[must_use]
pub(crate) fn scale_timeout_for_load(base: Duration, load_per_core: Option<f64>) -> Duration {
    let Some(lpc) = load_per_core else {
        return base;
    };
    if !lpc.is_finite() || lpc <= 1.0 {
        return base;
    }
    let scaled_secs = (base.as_secs_f64() * lpc).ceil();
    // `scaled_secs` is finite and >= base.as_secs_f64() (lpc > 1.0), so the
    // cast is always in-range for any load reading that could plausibly occur.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let scaled = Duration::from_secs(scaled_secs as u64);
    scaled.clamp(base, MAX_SCALED_STATUS_TIMEOUT)
}

/// Resolve the effective status IPC timeout for one invocation (Issue #6011
/// AC1/AC2), in priority order:
///
/// 1. `explicit_secs` — the `--timeout-secs` CLI flag, when given. Takes the
///    operator's stated value verbatim (floored at 1s).
/// 2. Otherwise, [`scale_timeout_for_load`] over [`DEFAULT_STATUS_TIMEOUT`]
///    using the freshly-read host load, then [`LOOM_DAEMON_IPC_TIMEOUT_MS`]
///    (shared with `dispatch`, via
///    [`super::common::apply_ipc_timeout_env_floor`]) as a final raise-only
///    floor over that.
///
/// The load reading is always taken and returned alongside the timeout —
/// even when `explicit_secs` bypasses the scaling — so the caller can report
/// it in a timeout message regardless of which path produced the budget.
///
/// [`LOOM_DAEMON_IPC_TIMEOUT_MS`]: super::common::DAEMON_IPC_TIMEOUT_ENV
#[must_use]
pub(crate) fn resolve_status_timeout(explicit_secs: Option<u64>) -> StatusTimeoutInfo {
    let logical_cpus = loom_daemon::cpu_headroom::logical_cpu_count();
    let loadavg_1m = loom_daemon::cpu_headroom::read_loadavg_1m();
    let load_per_core = loom_daemon::cpu_headroom::load_per_core_from(loadavg_1m, logical_cpus);

    let timeout = match explicit_secs {
        Some(secs) => Duration::from_secs(secs.max(1)),
        None => {
            let scaled = scale_timeout_for_load(DEFAULT_STATUS_TIMEOUT, load_per_core);
            super::common::apply_ipc_timeout_env_floor(scaled)
        }
    };

    StatusTimeoutInfo {
        timeout,
        loadavg_1m,
        load_per_core,
        logical_cpus,
    }
}

/// Render `info` for a human-readable timeout error/log line (Issue #6011
/// AC2) — always names the effective timeout, and the load reading behind it
/// when one was available.
fn describe_status_timeout(info: &StatusTimeoutInfo) -> String {
    match (info.loadavg_1m, info.load_per_core) {
        (Some(load), Some(lpc)) => format!(
            "{}s (host load {load:.2} across {} logical CPUs, {lpc:.2}/core)",
            info.timeout.as_secs(),
            info.logical_cpus
        ),
        _ => format!("{}s (host load unavailable)", info.timeout.as_secs()),
    }
}

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
    /// slow-but-live daemon (honor the single resolved timeout budget — see
    /// [`resolve_status_timeout`] — rather than doubling it)
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
///
/// `timeout_info` (Issue #6011) carries the caller-resolved timeout (see
/// [`resolve_status_timeout`]) — already scaled for observed host load and/or
/// an explicit `--timeout-secs`/`LOOM_DAEMON_IPC_TIMEOUT_MS` override — plus
/// the load reading it was derived from, so a genuine round-trip timeout can
/// report *why* it chose that budget.
pub(crate) async fn query_daemon_status(
    socket_path: &Path,
    timeout_info: &StatusTimeoutInfo,
) -> Result<DaemonStatusReport> {
    match query_daemon_status_once(socket_path, timeout_info).await {
        Ok(report) => Ok(report),
        Err(StatusAttemptError::DroppedBeforeReply(_first)) => {
            // One bounded reconnect retry — the transient case only.
            query_daemon_status_once(socket_path, timeout_info)
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
    timeout_info: &StatusTimeoutInfo,
) -> std::result::Result<DaemonStatusReport, StatusAttemptError> {
    let timeout = timeout_info.timeout;
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
        // #6011: name the measured host load alongside the timeout, not just
        // its number — a fixed-budget "timed out after 5s" alone gave an
        // operator no way to tell "the daemon is slow because the host is at
        // load 59" from "the daemon is genuinely wedged".
        Err(_elapsed) => Err(StatusAttemptError::Roundtrip(anyhow!(
            "status round-trip timed out after {}",
            describe_status_timeout(timeout_info)
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
///
/// `timeout_secs` (Issue #6011 AC1) is the `--timeout-secs` flag: an explicit
/// operator-stated IPC round-trip budget, taking priority over both the
/// load-scaled default and `LOOM_DAEMON_IPC_TIMEOUT_MS`. `None` (the default)
/// resolves the timeout via [`resolve_status_timeout`] instead — see that
/// function for the full precedence order.
pub(crate) async fn handle_status_command(
    json: bool,
    pipeline: bool,
    timeout_secs: Option<u64>,
) -> Result<()> {
    let socket_path = resolve_socket_path()?;
    let timeout_info = resolve_status_timeout(timeout_secs);

    let report = match query_daemon_status(&socket_path, &timeout_info).await {
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

    // Worktree footprint per managed repo (#5939) — a host-local filesystem
    // walk, deliberately client-side for the same reason the per-token probe
    // and the pipeline snapshot are: the daemon's IPC handler stays fast. The
    // CLI always shares a host with the daemon it just queried over a Unix
    // socket, so walking the roots the daemon itself reported measures exactly
    // the right filesystem. A root the daemon already flagged missing (#4326)
    // is skipped rather than walked into.
    let worktree_disk: Vec<loom_daemon::worktree_disk_status::WorktreeDiskSummary> = report
        .per_repo
        .iter()
        .filter(|r| !r.root_missing)
        .map(|r| loom_daemon::worktree_disk_status::collect_worktree_disk_summary(&r.root))
        .collect();

    if json {
        print_status_json(
            &report,
            token_usage.as_ref(),
            &update,
            pipeline_snapshots.as_deref(),
            protection.as_ref(),
            Some(&worktree_disk),
        )?;
    } else {
        print_status_human(
            &report,
            token_usage.as_ref(),
            &update,
            pipeline_snapshots.as_deref(),
            protection.as_ref(),
            Some(&worktree_disk),
        );
    }

    // #5409 AC2: "autonomy-desired marker present + work finder off" is a
    // NON-OK state on the reachable path, not just a `WARNING:` line beneath
    // a `Protection: protected` header — before this, `handle_status_command`
    // returned `Ok(())`/exit 0 here unconditionally, so a caller scripting
    // against the exit code alone (rather than grepping the printed text)
    // saw a healthy-looking exit even though autonomous dispatch was
    // silently off. Distinct exit-code namespace from both the
    // unreachable-path `InstallState` codes above (1/3/4) and
    // `loom-daemon fleet status`'s `HealthReport::exit_code()` (0/1/2, a
    // different command entirely) — see `EXIT_AUTONOMY_MISMATCH`'s doc
    // comment.
    if autonomy_mismatch(protection.as_ref(), &report) {
        std::process::exit(daemon_install_state::EXIT_AUTONOMY_MISMATCH);
    }

    Ok(())
}

/// Format an instant as the exact UTC timestamp shape the watchdog log writes
/// on **every** line — `date -u '+%Y-%m-%dT%H:%M:%SZ'` in
/// `defaults/scripts/cli/loom-daemon-watchdog.sh`'s `report()` helper (#5790).
///
/// The two surfaces are the operator's only two views of the same fault, and
/// before #5790 they shared no anchor: `status` printed relative ages only
/// ("heartbeat is fresh (46s ago)"), which are meaningful solely at the instant
/// they are read, while the watchdog log is absolute UTC. A captured `status`
/// invocation therefore could not be reconciled against `daemon-watchdog.log`
/// after the fact, and the two read as contradictory evidence (the original
/// incident report). Emitting the identical format makes the correlation a
/// literal `grep`.
fn watchdog_utc_stamp(t: chrono::DateTime<chrono::Utc>) -> String {
    t.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// The absolute UTC timestamp `secs_ago` seconds before `now`, in the watchdog
/// log's format ([`watchdog_utc_stamp`]) — used to print an absolute anchor
/// alongside each relative age in the unreachable-daemon output (#5790).
///
/// Saturates at `now` when the age cannot be represented (a corrupt or absurd
/// heartbeat mtime must never panic the *error* path this runs on).
fn watchdog_utc_stamp_secs_ago(now: chrono::DateTime<chrono::Utc>, secs_ago: u64) -> String {
    let at = i64::try_from(secs_ago)
        .ok()
        .and_then(chrono::TimeDelta::try_seconds)
        .and_then(|d| now.checked_sub_signed(d))
        .unwrap_or(now);
    watchdog_utc_stamp(at)
}

/// Best-effort `kill -0` liveness probe for a raw pid (Issue #6011 AC4).
///
/// The library crate already has (near-)identical probes —
/// `daemon_install_state::pid_alive`, `sweep_registry::crash_signals::is_pid_alive`
/// — but both are `pub(crate)` to `loom_daemon`, not reachable from this
/// separate binary crate. This is a small standalone copy rather than
/// widening either's visibility for one degraded-path read: unlike those, it
/// runs only on an already-failing `status` command, so it favors simplicity
/// (no bounded-timeout wrapper) over the extra robustness the daemon's own
/// hot-path probes need.
fn cli_pid_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Degraded, socket-free view of possibly-in-flight sweeps (Issue #6011 AC4):
/// when the IPC round-trip cannot complete, the persisted sweep journal
/// (`~/.loom/sweeps.json`, survives a daemon restart — issue #3953) is the
/// only other authoritative "what is this daemon doing right now" source this
/// CLI can read WITHOUT a live connection. Live-pid-filtered via
/// [`cli_pid_alive`] so a stale entry from a long-dead sweep is not
/// misreported as still running.
///
/// Best-effort throughout: any read/resolve failure yields an empty list
/// rather than erroring the already-failing `status` command further.
fn read_degraded_in_flight_sweeps() -> Vec<loom_daemon::sweep_journal::JournalEntry> {
    let Ok(path) = loom_daemon::sweep_journal::default_journal_path() else {
        return Vec::new();
    };
    let mut journal = loom_daemon::sweep_journal::load(&path);
    loom_daemon::sweep_journal::prune_dead(&mut journal, cli_pid_alive);
    journal.entries
}

/// Human-readable rendering of [`read_degraded_in_flight_sweeps`] — a no-op
/// (prints nothing) when the journal is empty or unreadable, so an idle host
/// or a host predating issue #3953's journal does not print a misleading
/// "nothing in flight" line the daemon itself never confirmed.
fn print_degraded_in_flight_sweeps_human() {
    let entries = read_degraded_in_flight_sweeps();
    if entries.is_empty() {
        return;
    }
    eprintln!(
        "Degraded view (read directly from ~/.loom/sweeps.json, no IPC needed) — this host's \
         journal"
    );
    eprintln!("still shows these sweeps in flight:");
    for entry in &entries {
        eprintln!(
            "  issue #{} — pid {} — repo {} — started {}",
            entry.issue,
            entry.pid,
            entry.repo,
            entry.started_at.to_rfc3339()
        );
    }
    eprintln!(
        "This is exactly what a stop/restart risks killing on a systemd-supervised host \
         (#5119) —"
    );
    eprintln!("weigh that cost before restarting.");
    eprintln!();
}

/// Emit the unreachable-daemon `--json` error, state-aware (Issue #4069). The
/// existing `error` prose key is retained for compatibility; `install_state`
/// (when the probe could classify at all) adds a machine-readable enum plus
/// the diagnostic fields a script or human can act on.
///
/// `observed_at` (and `heartbeat.last_write_at`, #5790) are the machine-readable
/// half of the watchdog-log correlation described on [`watchdog_utc_stamp`]:
/// both are additive keys in the watchdog log's own timestamp format, so a
/// scraper joining a captured `status --json` against `daemon-watchdog.log`
/// needs no clock arithmetic of its own.
fn print_status_unreachable_json(
    socket_path: &Path,
    err: &anyhow::Error,
    install_state: Option<&InstallStateReport>,
) -> Result<()> {
    let now = chrono::Utc::now();
    let mut payload = serde_json::json!({
        "error": format!("could not reach loom-daemon at {}: {err}", socket_path.display()),
        "observed_at": watchdog_utc_stamp(now),
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
                "last_write_at": r.heartbeat_age_secs.map(|age| watchdog_utc_stamp_secs_ago(now, age)),
                "stale_threshold_secs": r.heartbeat_stale_threshold_secs,
            },
            "process_age_secs": r.process_age_secs,
            "startup_grace_threshold_secs": r.startup_grace_threshold_secs,
            "watchdog_log": r.watchdog_log_path.display().to_string(),
        });
        // #6011 AC4: the same degraded, socket-free journal read the human
        // path prints (`print_degraded_in_flight_sweeps_human`), only
        // meaningful when a process is actually alive to have a journal
        // entry behind it.
        if r.state == daemon_install_state::InstallState::AliveButUnresponsive {
            let entries = read_degraded_in_flight_sweeps();
            payload["in_flight_from_journal"] = serde_json::json!(entries
                .iter()
                .map(|e| serde_json::json!({
                    "issue": e.issue,
                    "pid": e.pid,
                    "repo": e.repo,
                    "started_at": e.started_at.to_rfc3339(),
                }))
                .collect::<Vec<_>>());
        }
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
///
/// Every branch is prefixed with an absolute `Observed at <UTC>` line in the
/// watchdog log's own timestamp format (#5790) — see [`watchdog_utc_stamp`] for
/// why a relative age alone left `status` and `daemon-watchdog.log` unable to be
/// reconciled.
fn print_status_unreachable_human(
    socket_path: &Path,
    err: &anyhow::Error,
    install_state: Option<&InstallStateReport>,
) {
    let now = chrono::Utc::now();
    eprintln!("Could not reach loom-daemon at {}: {err}", socket_path.display());
    eprintln!(
        "Observed at {} (UTC — the same clock and format the watchdog log stamps \
         every line with; use it to line this report up against that log).",
        watchdog_utc_stamp(now)
    );
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
                // #5409 AC3: this exact string is what operators paste. A bare
                // recovery start with no flags used to be able to silently
                // re-render FLAGS-OFF over what may have been a previously
                // autonomous host (#4693) — the loom-daemon-start.sh recovery
                // path (AC1) now REFUSES that plain form when it detects a
                // downgrade, so recommend a flag here up front rather than
                // have the operator discover the refusal only after pasting
                // the bare command.
                eprintln!("Recover with (pass a flag to state the desired autonomy explicitly —");
                eprintln!("a plain start now refuses if it would silently downgrade a previously");
                eprintln!("autonomous host, #5409):");
                eprintln!("  ./.loom/scripts/cli/loom-daemon-start.sh --work-finder");
                eprintln!(
                    "(add --health-gate too if the main-health gate was also on, or use \
                     --from-config"
                );
                eprintln!("to drive both from .loom/config.json -> autonomous instead)");
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
                            "Heartbeat is fresh ({}s ago, last write {}) — likely an \
                             IPC/socket-layer fault, not a wedged daemon.",
                            r.heartbeat_age_secs.unwrap_or_default(),
                            watchdog_utc_stamp_secs_ago(
                                now,
                                r.heartbeat_age_secs.unwrap_or_default()
                            )
                        );
                    }
                    Some(daemon_install_state::HeartbeatFreshness::Stale) => {
                        eprintln!(
                            "Heartbeat is STALE ({}s ago, last write {}, > {}s threshold) — the \
                             daemon is likely wedged.",
                            r.heartbeat_age_secs.unwrap_or_default(),
                            watchdog_utc_stamp_secs_ago(
                                now,
                                r.heartbeat_age_secs.unwrap_or_default()
                            ),
                            r.heartbeat_stale_threshold_secs.unwrap_or_default()
                        );
                    }
                    Some(daemon_install_state::HeartbeatFreshness::PriorBoot) => {
                        eprintln!(
                            "Heartbeat file is from a PREVIOUS boot ({}s old, last write {}; this \
                             process is only {}s old) — it is not evidence about the current \
                             process. (A daemon that wedged before writing its first heartbeat \
                             this boot would look identical — re-check after the process is well \
                             past startup if you still suspect a wedge.)",
                            r.heartbeat_age_secs.unwrap_or_default(),
                            watchdog_utc_stamp_secs_ago(
                                now,
                                r.heartbeat_age_secs.unwrap_or_default()
                            ),
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

                // #6011 AC4: a degraded, socket-free view of "what is this
                // daemon doing right now" — the persisted sweep journal
                // survives a daemon restart and needs no IPC round-trip, so
                // it is the one useful thing `status` CAN still render while
                // the socket itself is slow/unresponsive.
                print_degraded_in_flight_sweeps_human();

                // #5790 AC4: this is the exact state the original incident
                // report describes — `status` timing out on IPC while the
                // watchdog log showed only OK/DIVERGENCE lines. Point the
                // operator at the log explicitly and give them the timestamp
                // window to look in, so the two views are reconciled rather
                // than read as contradicting each other.
                eprintln!(
                    "Correlate with the watchdog's own view of this window (its lines carry the \
                     same UTC format as the \"Observed at\" stamp above):"
                );
                eprintln!("  {}", r.watchdog_log_path.display());
                eprintln!();
                // Advice gating (#4368): the imperative restart remediation is
                // only warranted for a *current-boot* Stale verdict — the one
                // case where the evidence actually points at a wedge.
                // Fresh/Unknown/PriorBoot get inspect-first guidance instead,
                // so an operator is never steered into restarting a daemon
                // that is merely mid-fault-diagnosis or missing heartbeat
                // evidence, not actually wedged.
                //
                // #6011 AC3: neither branch's remediation ends on an
                // unconditional bare stop/start any more. On a
                // systemd-supervised host that stop tears down the service's
                // cgroup and SIGKILLs every in-flight sweep/role run (#5119)
                // — exactly the outcome the drain machinery exists to avoid,
                // and the worst available action on a host whose status is
                // slow precisely BECAUSE it is busy doing that work. Both
                // branches now lead with `restart --drain` (itself an IPC
                // call, so it can only help when the daemon's IPC path is
                // merely slow, not truly dead) and name the systemd risk
                // explicitly before falling back to the bare stop/start.
                if r.heartbeat_freshness == Some(daemon_install_state::HeartbeatFreshness::Stale) {
                    if let Some(pid) = r.pid {
                        eprintln!(
                            "Do NOT run loom-daemon-start.sh — the singleton guard will refuse"
                        );
                        eprintln!("while pid {pid} is alive. Inspect it directly, or try:");
                    } else {
                        eprintln!(
                            "Do NOT run loom-daemon-start.sh — the singleton guard will refuse"
                        );
                        eprintln!("while the daemon is alive. Inspect it directly, or try:");
                    }
                    eprintln!("  loom-daemon restart --drain");
                    eprintln!(
                        "(this itself needs IPC to accept the request — a GENUINELY wedged \
                         daemon may"
                    );
                    eprintln!(
                        "not ack it either, but it is bounded by its own timeout so it fails \
                         safe rather"
                    );
                    eprintln!(
                        "than hanging. Only once THAT has also failed, fall back to a bare \
                         stop/start —"
                    );
                    eprintln!(
                        "on a systemd-supervised host this SIGKILLs every in-flight sweep/role \
                         run instead"
                    );
                    eprintln!("of finishing them, #5119 — expect that cost before running it):");
                    eprintln!("  ./.loom/scripts/cli/loom-daemon-stop.sh && ./.loom/scripts/cli/loom-daemon-start.sh");
                } else {
                    eprintln!("Inspect before acting — this evidence does not indicate a wedge:");
                    if let Some(pid) = r.pid {
                        eprintln!(
                            "  ps -p {pid} -o pid,etime,command   # confirm what it is actually doing"
                        );
                    }
                    eprintln!("  loom-daemon status --json           # machine-readable detail");
                    eprintln!(
                        "  loom-daemon status --timeout-secs 30 # retry with more IPC headroom \
                         (#6011)"
                    );
                    eprintln!();
                    eprintln!(
                        "If it is still unresponsive after inspecting, prefer a drained restart \
                         over a"
                    );
                    eprintln!(
                        "bare stop/start — on a systemd-supervised host a bare stop tears down \
                         the"
                    );
                    eprintln!(
                        "service's cgroup and SIGKILLs every in-flight sweep/role run instead of \
                         finishing"
                    );
                    eprintln!("them (#5119):");
                    eprintln!("  loom-daemon restart --drain");
                    eprintln!(
                        "Only if THAT also fails to respond, fall back to (accepting the \
                         in-flight-sweep"
                    );
                    eprintln!("kill on systemd):");
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
///
/// `timeout_secs` (issue #5575) bounds BOTH the SSH connect timeout
/// ([`loom_daemon::fleet::status::SshStatusSource::connect_timeout_secs`])
/// and the outer per-host [`tokio::time::timeout`] wrapping the whole
/// collection — the CLI's `--timeout-secs` flag defaults to
/// [`loom_daemon::fleet::status::DEFAULT_TIMEOUT_SECS`], so omitting it
/// preserves the pre-#5575 8s behavior exactly.
pub(crate) async fn handle_fleet_status_command(json: bool, timeout_secs: u64) -> Result<()> {
    use loom_daemon::fleet::status::{
        all_tailnet_hosts_unreachable, collect_fleet_report, update_last_seen_up_at,
        SshStatusSource,
    };
    use loom_daemon::fleet::FleetRegistry;

    // `collect_fleet_report` consumes its registry argument (it owns each
    // `WorkerRecord` into the concurrent per-host collection).
    let registry = FleetRegistry::load_default()?;
    let local = collect_local_fleet_report().await;
    let source: Arc<dyn loom_daemon::fleet::status::HostStatusSource> = Arc::new(SshStatusSource {
        connect_timeout_secs: timeout_secs,
    });
    let timeout = Duration::from_secs(timeout_secs);
    let mut report = collect_fleet_report(source, registry, local, timeout).await;

    // #4952: when every tailnet-addressed roster host is UNREACHABLE, that
    // pattern is indistinguishable from a genuine multi-host outage without
    // checking whether the LOCAL tailnet client is actually the thing that
    // is down (the 2026-08-02 robb-STUDIO incident this issue documents).
    // Cheap, local-only, best-effort: never blocks the report on failure.
    if all_tailnet_hosts_unreachable(&report.hosts) {
        report.local_tailnet = check_local_tailnet_state();
    }

    // #4697: persist "last observed Up" so a LATER poll that finds this host
    // Unreachable can measure elapsed silence against its configured
    // idle-shutdown window (the expected-power-off heuristic in
    // `loom_daemon::fleet::status`).
    //
    // Deliberately RE-LOADS the registry rather than mutating the copy fanned
    // out above: the SSH collection can take up to `timeout_secs`, and
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

/// Handle `loom-daemon fleet roll [<host>|--all]` (issue #5504, epic #4340):
/// roll the `loom-daemon` binary across fleet hosts with a measured
/// process-vs-build-time verdict. Thin clap→module wiring — the SSH
/// orchestration, measured verdict, and per-host fanout all live in
/// [`loom_daemon::fleet::roll`]; this needs the async runtime for the same
/// reason `fleet status` does (a per-host [`tokio::time::timeout`] fanout in
/// `--all` mode).
pub(crate) async fn handle_fleet_roll_command(
    host: Option<String>,
    all: bool,
    timeout_secs: u64,
    json: bool,
) -> Result<()> {
    use loom_daemon::fleet::add_worker::SshRunner;
    use loom_daemon::fleet::roll::{self, RollReport};
    use loom_daemon::fleet::CommandRunner;

    if !all && host.is_none() {
        eprintln!(
            "error: `fleet roll` requires either an SSH_HOST argument or --all (roll every \
             registered fleet worker)."
        );
        std::process::exit(1);
    }

    let timeout = Duration::from_secs(timeout_secs);
    let report: RollReport = roll::run(
        |h| {
            let runner: Arc<dyn CommandRunner + Send + Sync> = Arc::new(SshRunner::new(h));
            runner
        },
        host,
        all,
        timeout,
    )
    .await?;

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

    // #6011: `fleet status` has its own `--timeout-secs` for the SSH fanout
    // (`handle_fleet_status_command`'s own `timeout_secs` param); the LOCAL
    // row's own IPC round-trip resolves independently via the same
    // load-scaled default every standalone `status` invocation uses.
    let timeout_info = resolve_status_timeout(None);
    match query_daemon_status(&socket_path, &timeout_info).await {
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
                // #5939: the worktree census is a filesystem walk over every
                // managed root. `fleet status` fans out across hosts and is
                // latency-sensitive, so the local row omits it — the same
                // reason `pipeline` is `None` here. `loom-daemon status` on
                // the host itself is where an operator reads it.
                None,
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

/// #4952: cheap, local-only self-check for "is the tailnet client on THIS
/// host actually the thing that's down" — run only when
/// [`all_tailnet_hosts_unreachable`](loom_daemon::fleet::status::all_tailnet_hosts_unreachable)
/// flags every tailnet-addressed remote roster host as `Unreachable`, since
/// that pattern is otherwise indistinguishable from a genuine multi-host
/// outage. Never an SSH round-trip (that is [`HostStatusSource`](loom_daemon::fleet::status::HostStatusSource)'s job) —
/// this shells out to the LOCAL `tailscale` CLI exactly once.
///
/// Returns `None` when the check is a no-op — AC 4: a missing `tailscale`
/// CLI must not become a new hard dependency, so an absent binary
/// (`ErrorKind::NotFound`) silently skips the check rather than erroring the
/// whole report (current, pre-#4952 behavior unchanged).
fn check_local_tailnet_state() -> Option<loom_daemon::fleet::status::LocalTailnetState> {
    check_local_tailnet_state_with_path_override(None)
}

/// [`check_local_tailnet_state`] with the spawned `tailscale` child's `PATH`
/// overridable rather than mutating the parent process's environment.
///
/// Exists so the "tailscale CLI absent" branch (AC 4) can be tested without
/// `std::env::set_var("PATH", …)`: `PATH` is process-global and Rust's test
/// harness runs tests as threads in one process, so replacing it outright
/// races every concurrently-running test that spawns a bare-name subprocess
/// (#5961/#5969). `check_local_tailnet_state` itself does not read `PATH` —
/// only the spawned `tailscale` command's own bare-name lookup does — so the
/// override is applied to the child's environment via [`Command::env`]
/// rather than injected as a search-path parameter the way
/// `resolve_tool_path_in`/`probe_gh_availability_with_search_path` do.
fn check_local_tailnet_state_with_path_override(
    path_override: Option<&std::ffi::OsStr>,
) -> Option<loom_daemon::fleet::status::LocalTailnetState> {
    use loom_daemon::fleet::status::LocalTailnetState;

    let mut command = std::process::Command::new("tailscale");
    command
        .args(["status", "--json"])
        .stdin(std::process::Stdio::null());
    if let Some(path) = path_override {
        command.env("PATH", path);
    }

    let output = match command.output() {
        Ok(out) => out,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        // Some other launch failure (permissions, etc.) — degrade rather
        // than erroring the report, same as `SafehousedProbe::Unknown`.
        Err(_) => return Some(LocalTailnetState::Unknown),
    };

    classify_tailscale_status_output(&output.stdout)
}

/// Parse `tailscale status --json`'s stdout into a [`LocalTailnetState`]
/// (split out from [`check_local_tailnet_state`] so it is unit-testable
/// without shelling out). `BackendState: "Running"` is up; any other
/// `BackendState` value (`"Stopped"`, `"NeedsLogin"`, …) is down for this
/// purpose — the self-check only needs to distinguish "the local tailnet
/// client can currently reach the tailnet" from "it cannot," not classify
/// every backend state individually. Unparseable output degrades to
/// `Unknown` rather than misreporting either up or down.
#[must_use]
fn classify_tailscale_status_output(
    stdout: &[u8],
) -> Option<loom_daemon::fleet::status::LocalTailnetState> {
    use loom_daemon::fleet::status::LocalTailnetState;

    let text = String::from_utf8_lossy(stdout);
    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(value) => match value.get("BackendState").and_then(|v| v.as_str()) {
            Some("Running") => Some(LocalTailnetState::Up),
            Some(_) => Some(LocalTailnetState::Down),
            None => Some(LocalTailnetState::Unknown),
        },
        Err(_) => Some(LocalTailnetState::Unknown),
    }
}

#[cfg(test)]
mod local_tailnet_tests {
    //! #4952: the local tailnet self-check. `classify_tailscale_status_output`
    //! is exercised directly against canned JSON (no process spawn, no PATH
    //! mutation, safe to run in parallel with everything else). The
    //! CLI-absent path (AC 4) is exercised end-to-end through
    //! `check_local_tailnet_state_with_path_override`, which overrides only
    //! the spawned `tailscale` child's own `PATH` (#5961/#5969) — never the
    //! parent process's — so this needs no `#[serial]` guard and cannot race
    //! any other test in this binary.
    use super::{check_local_tailnet_state_with_path_override, classify_tailscale_status_output};
    use loom_daemon::fleet::status::LocalTailnetState;

    #[test]
    fn classifies_running_backend_state_as_up() {
        let stdout = br#"{"BackendState":"Running","Self":{}}"#;
        assert_eq!(classify_tailscale_status_output(stdout), Some(LocalTailnetState::Up));
    }

    #[test]
    fn classifies_stopped_backend_state_as_down() {
        let stdout = br#"{"BackendState":"Stopped"}"#;
        assert_eq!(classify_tailscale_status_output(stdout), Some(LocalTailnetState::Down));
    }

    #[test]
    fn classifies_other_non_running_backend_states_as_down() {
        // NeedsLogin, NeedsMachineAuth, etc. all mean "cannot currently reach
        // the tailnet" for this self-check's purposes — not just "Stopped".
        let stdout = br#"{"BackendState":"NeedsLogin"}"#;
        assert_eq!(classify_tailscale_status_output(stdout), Some(LocalTailnetState::Down));
    }

    #[test]
    fn classifies_unparseable_output_as_unknown_not_up_or_down() {
        assert_eq!(
            classify_tailscale_status_output(b"not json at all"),
            Some(LocalTailnetState::Unknown)
        );
    }

    #[test]
    fn classifies_missing_backend_state_field_as_unknown() {
        assert_eq!(
            classify_tailscale_status_output(br#"{"Self":{}}"#),
            Some(LocalTailnetState::Unknown)
        );
    }

    /// AC 4: an absent `tailscale` CLI must be a silent no-op (`None`), not
    /// an error or an `Unknown` state — current, pre-#4952 behavior
    /// unchanged.
    #[test]
    fn check_local_tailnet_state_is_none_when_tailscale_cli_absent() {
        let empty_dir = tempfile::tempdir().unwrap();
        // A PATH override containing only an empty directory guarantees
        // `tailscale` cannot resolve for the spawned child, regardless of
        // what's installed on the real host running this test. Applied to
        // the child's own environment via `Command::env` (#5961/#5969), so
        // this cannot race any concurrently-running test's own bare-name
        // subprocess lookups.
        let result =
            check_local_tailnet_state_with_path_override(Some(empty_dir.path().as_os_str()));

        assert_eq!(
            result, None,
            "absent tailscale CLI must skip the check silently, got {result:?}"
        );
    }
}

#[cfg(test)]
mod watchdog_correlation_tests {
    //! #5790 AC4: `loom-daemon status`'s unreachable/degraded output and
    //! `daemon-watchdog.log` must be reconcilable by timestamp. These lock in
    //! the shared format (`date -u '+%Y-%m-%dT%H:%M:%SZ'`, the watchdog's
    //! `report()` helper) and the relative-age → absolute-instant arithmetic
    //! that turns "46s ago" into something greppable in that log.
    use super::{watchdog_utc_stamp, watchdog_utc_stamp_secs_ago};
    use chrono::TimeZone;

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> chrono::DateTime<chrono::Utc> {
        chrono::Utc
            .with_ymd_and_hms(y, mo, d, h, mi, s)
            .single()
            .expect("valid instant")
    }

    /// The stamp must match the watchdog log's line prefix byte for byte:
    /// second precision, `T` separator, literal `Z`, no fractional part.
    #[test]
    fn stamp_matches_watchdog_log_format() {
        assert_eq!(watchdog_utc_stamp(at(2026, 8, 9, 6, 3, 32)), "2026-08-09T06:03:32Z");
    }

    /// A relative age resolves to the absolute instant the event happened —
    /// the whole point of AC4 (a captured "fresh (46s ago)" is meaningless
    /// against a log line read hours later).
    #[test]
    fn secs_ago_resolves_to_the_absolute_instant() {
        let now = at(2026, 8, 9, 6, 3, 32);
        assert_eq!(watchdog_utc_stamp_secs_ago(now, 46), "2026-08-09T06:02:46Z");
        assert_eq!(watchdog_utc_stamp_secs_ago(now, 0), "2026-08-09T06:03:32Z");
        // Crosses a day boundary rather than clamping within the date.
        assert_eq!(watchdog_utc_stamp_secs_ago(now, 7 * 3600), "2026-08-08T23:03:32Z");
    }

    /// This runs on the *error* path: a corrupt/absurd heartbeat mtime must
    /// degrade to `now`, never panic or overflow.
    #[test]
    fn absurd_age_saturates_at_now_instead_of_panicking() {
        let now = at(2026, 8, 9, 6, 3, 32);
        assert_eq!(watchdog_utc_stamp_secs_ago(now, u64::MAX), "2026-08-09T06:03:32Z");
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
    use super::{query_daemon_status, resolve_status_timeout};
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
            journal_adopted_at_startup: 0,
            in_flight: vec![],
            unregistered_locked: vec![],
            token_pool_size: 4,
            token_pool_dir: Some(std::path::PathBuf::from("/repo/a/.loom/tokens")),
            disk_headroom: 10,
            ram_headroom: 10,
            logical_cpus: 8,
            loadavg_1m: Some(1.25),
            cpu_idle_fraction: Some(0.90),
            capacity_bound: false,
            preflight_advisory_active: false,
            preflight_advisory_message: None,
            preflight_advisory_changed_at: None,
            configured_max: 5,
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
            role_runner_host_env_override: None,
            role_runner_shard: None,
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
            role_last_tick: vec![],
            // #6102: role-agent load alongside the sweep in-flight list.
            active_role_agents: 2,
            role_agent_max_concurrent: Some(7),
            daemon_pid: Some(99917),
            pid_file: Some(std::path::PathBuf::from("/repo/a/.loom/.daemon.pid")),
            daemon_build_commit: Some("18887b5c".to_string()),
            daemon_built_at_raw: Some("2026-08-02T03:09:51Z".to_string()),
            work_finder_interval_secs: Some(60),
            observability_host_id_mismatch: None,
            observability_export: None,
            peer_claims: None,
            deep_clean: Vec::new(),
            idle_exit: None,
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

        // #6011: an explicit `--timeout-secs` override bypasses host-load
        // scaling so this test's timing assertions are deterministic
        // regardless of the CI runner's actual load average.
        let timeout_info = resolve_status_timeout(Some(5));
        let started = std::time::Instant::now();
        let result = query_daemon_status(&socket_path, &timeout_info).await;
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

        let timeout_info = resolve_status_timeout(Some(5));
        let result = query_daemon_status(&socket_path, &timeout_info).await;
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

        let timeout_info = resolve_status_timeout(Some(5));
        let started = std::time::Instant::now();
        let result = query_daemon_status(&socket_path, &timeout_info).await;
        let elapsed = started.elapsed();

        assert!(result.is_err(), "expected a connect error for an absent socket");
        assert!(
            elapsed < Duration::from_secs(2),
            "absent-socket path took too long: {elapsed:?}"
        );
    }
}

#[cfg(test)]
mod timeout_scaling_tests {
    //! Issue #6011 AC1/AC2: the status IPC timeout must be configurable (flag
    //! and/or env) and must scale with (or at least report) observed host
    //! load, so a loaded host does not misclassify a merely-slow daemon as
    //! unreachable. These lock in the pure scaling function
    //! ([`scale_timeout_for_load`]) and the precedence order
    //! [`resolve_status_timeout`] applies across flag / env / load-scaling.
    use super::{
        resolve_status_timeout, scale_timeout_for_load, DEFAULT_STATUS_TIMEOUT,
        MAX_SCALED_STATUS_TIMEOUT,
    };
    use crate::cli::common::DAEMON_IPC_TIMEOUT_ENV;
    use serial_test::serial;
    use std::time::Duration;

    #[test]
    fn unsaturated_load_leaves_the_base_timeout_unchanged() {
        assert_eq!(
            scale_timeout_for_load(DEFAULT_STATUS_TIMEOUT, Some(0.5)),
            DEFAULT_STATUS_TIMEOUT
        );
        // Exactly 1.0 load-per-core (as many runnable threads as cores) is
        // the boundary — still not scaled, matching `is_host_saturated`'s own
        // `>=` semantics being a strictly-above trigger here.
        assert_eq!(
            scale_timeout_for_load(DEFAULT_STATUS_TIMEOUT, Some(1.0)),
            DEFAULT_STATUS_TIMEOUT
        );
    }

    #[test]
    fn missing_load_reading_leaves_the_base_timeout_unchanged() {
        // Fail-open (mirrors `cpu_headroom`'s own convention): absent
        // evidence is never treated as "the host is loaded".
        assert_eq!(scale_timeout_for_load(DEFAULT_STATUS_TIMEOUT, None), DEFAULT_STATUS_TIMEOUT);
    }

    /// The exact incident this issue reports (robb-studio, 2026-08-11): 28
    /// cores, load average 59 ⇒ ~2.11 load-per-core. The fixed 5s budget that
    /// misclassified a 6s-to-answer daemon as unreachable must grow well past
    /// 6s here.
    #[test]
    fn saturated_host_scales_the_timeout_up() {
        let load_per_core = 59.0 / 28.0;
        let scaled = scale_timeout_for_load(DEFAULT_STATUS_TIMEOUT, Some(load_per_core));
        assert!(
            scaled > Duration::from_secs(6),
            "expected real headroom over the reported 6s daemon response, got {scaled:?}"
        );
        assert!(scaled <= MAX_SCALED_STATUS_TIMEOUT);
    }

    #[test]
    fn absurd_load_is_capped_at_the_ceiling_not_left_unbounded() {
        assert_eq!(
            scale_timeout_for_load(DEFAULT_STATUS_TIMEOUT, Some(1_000.0)),
            MAX_SCALED_STATUS_TIMEOUT
        );
    }

    #[test]
    fn non_finite_load_reading_leaves_the_base_timeout_unchanged() {
        assert_eq!(
            scale_timeout_for_load(DEFAULT_STATUS_TIMEOUT, Some(f64::NAN)),
            DEFAULT_STATUS_TIMEOUT
        );
        assert_eq!(
            scale_timeout_for_load(DEFAULT_STATUS_TIMEOUT, Some(f64::INFINITY)),
            DEFAULT_STATUS_TIMEOUT
        );
    }

    /// AC1: an explicit `--timeout-secs` must win over both load-scaling and
    /// the `LOOM_DAEMON_IPC_TIMEOUT_MS` env floor — an operator's stated
    /// value is taken verbatim.
    #[test]
    #[serial]
    fn explicit_flag_takes_precedence_over_env_and_scaling() {
        std::env::set_var(DAEMON_IPC_TIMEOUT_ENV, "60000");
        let info = resolve_status_timeout(Some(3));
        assert_eq!(info.timeout, Duration::from_secs(3));
        std::env::remove_var(DAEMON_IPC_TIMEOUT_ENV);
    }

    /// AC1: with no flag, `LOOM_DAEMON_IPC_TIMEOUT_MS` still raises the
    /// effective timeout — the same convention `dispatch`'s ack budget
    /// already honors (issue #6011 shares
    /// `cli::common::apply_ipc_timeout_env_floor` between the two).
    #[test]
    #[serial]
    fn env_var_raises_the_default_when_no_flag_given() {
        std::env::set_var(DAEMON_IPC_TIMEOUT_ENV, "45000");
        let info = resolve_status_timeout(None);
        assert_eq!(info.timeout, Duration::from_secs(45));
        std::env::remove_var(DAEMON_IPC_TIMEOUT_ENV);
    }

    /// With neither an explicit flag nor the env var set, the resolved
    /// timeout must never fall below the 5s baseline regardless of this
    /// test-runner host's actual load — a real regression here would only
    /// ever make the timeout larger, never smaller.
    #[test]
    #[serial]
    fn default_resolution_never_undercuts_the_baseline() {
        std::env::remove_var(DAEMON_IPC_TIMEOUT_ENV);
        let info = resolve_status_timeout(None);
        assert!(info.timeout >= DEFAULT_STATUS_TIMEOUT);
    }
}

#[cfg(test)]
mod degraded_journal_tests {
    //! Issue #6011 AC4: a socket-free, best-effort view of possibly-in-flight
    //! sweeps read from the persisted sweep journal, for when the IPC
    //! round-trip itself is what is slow/unresponsive.
    use super::{cli_pid_alive, read_degraded_in_flight_sweeps};
    use loom_daemon::sweep_journal::{self, JournalEntry, SweepJournal};
    use serial_test::serial;

    #[test]
    fn cli_pid_alive_is_true_for_this_running_test_process() {
        assert!(cli_pid_alive(std::process::id()));
    }

    #[test]
    fn cli_pid_alive_is_false_for_an_implausible_pid() {
        // Deliberately NOT `u32::MAX`: as a `pid_t` that is `-1`, which `kill`
        // treats specially (signal every process the caller may signal) —
        // always "succeeds" regardless of whether any such pid is alive.
        // `999_999` is comfortably above any real pid on this host yet still
        // an ordinary positive `pid_t`.
        assert!(!cli_pid_alive(999_999));
    }

    /// The core AC4 invariant: a live entry survives, a dead one (an
    /// implausible/never-issued pid) is filtered out — so a crashed sweep's
    /// stale journal record is never misreported as still running.
    #[test]
    #[serial]
    fn read_degraded_in_flight_sweeps_filters_dead_pids() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("sweeps.json");
        std::env::set_var(sweep_journal::JOURNAL_PATH_ENV, &path);

        let journal = SweepJournal {
            version: 1,
            entries: vec![
                JournalEntry {
                    repo: "repo-a".to_string(),
                    issue: 42,
                    pid: std::process::id(), // this test process: definitely alive
                    started_at: chrono::Utc::now(),
                },
                JournalEntry {
                    repo: "repo-b".to_string(),
                    issue: 43,
                    // NOT `u32::MAX` — see `cli_pid_alive_is_false_for_an_implausible_pid`
                    // for why that value is unsuitable here (`kill`'s `-1`
                    // special case).
                    pid: 999_999,
                    started_at: chrono::Utc::now(),
                },
            ],
        };
        sweep_journal::save(&path, &journal).expect("save journal");

        let entries = read_degraded_in_flight_sweeps();
        assert_eq!(entries.len(), 1, "dead-pid entry should have been pruned: {entries:?}");
        assert_eq!(entries[0].issue, 42);

        std::env::remove_var(sweep_journal::JOURNAL_PATH_ENV);
    }

    /// A missing journal (no daemon has ever dispatched a sweep on this host,
    /// or this host predates issue #3953) must degrade to an empty list, not
    /// an error — the whole point of this being a best-effort read on an
    /// already-failing `status` command.
    #[test]
    #[serial]
    fn read_degraded_in_flight_sweeps_empty_when_journal_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("does-not-exist.json");
        std::env::set_var(sweep_journal::JOURNAL_PATH_ENV, &path);

        assert!(read_degraded_in_flight_sweeps().is_empty());

        std::env::remove_var(sweep_journal::JOURNAL_PATH_ENV);
    }
}
