//! `loom-daemon health` — the I/O half of the one-shot fleet-vitals command
//! (Issue #4761).
//!
//! Everything opinionated (every verdict rule, the #4694 liveness precedence,
//! the transient-vs-persistent role classifier, the exit-code contract) lives
//! in the pure [`loom_daemon::health`] collector. This module only *collects*:
//! one IPC round-trip, one local install-state probe, one `pgrep`, one
//! `.ranking` stat, and one bounded forge fan-out — then hands the result to
//! [`loom_daemon::health::assess`] and renders.
//!
//! # Why every probe here is best-effort
//!
//! A health command that fails to produce a report is worse than useless to
//! the watch loop it exists for. Every collection step degrades to "could not
//! determine" (which the collector renders as a non-green `UNKNOWN` section,
//! exit `1`) rather than aborting — the *only* thing that can stop this
//! command from printing a report is a panic.
//!
//! # Daemon-authoritative vs. caller-process sections (#5061)
//!
//! Not every section's verdict is trustworthy from the same vantage point,
//! which matters most exactly when this command is run somewhere other than
//! the machine that ran `loom-daemon health`'s writer (an SSH probe of a
//! remote fleet host, a non-login shell whose `PATH` differs from an
//! interactive login shell's):
//!
//! - **`liveness`, `dispatch`, `tokens`, `roles`, `observability`** are
//!   **daemon-authoritative** — every fact comes from the daemon's own IPC
//!   round-trip ([`DaemonStatusReport`]) or a local probe of *this host's*
//!   process table/filesystem, never a forge call made by this CLI process.
//!   A verdict here reflects the daemon's own state, not this caller's
//!   environment, so it is trustworthy the same way over SSH as locally.
//! - **`queues`, `throughput`** execute `gh` calls **in this CLI process**
//!   ([`pipeline_snapshot::GhPipelineSource`]), scoped to whatever `gh`
//!   resolves to and however it is authenticated *here* — which can differ
//!   from the daemon's own (already-verified, see `credential_preflight` in
//!   `status`/`--json`) forge credential. A missing/non-executable `gh` in
//!   *this* process (the common case: a non-login SSH shell whose `PATH`
//!   lacks `~/.local/bin` / `/opt/homebrew/bin`, #4875's failure class) is
//!   reported as a single distinct fact rather than a per-repo forge-query
//!   failure, and cross-references the daemon's own `credential_preflight`
//!   verdict when it is available — see `health::assess_queues` /
//!   `assess_throughput` / `gh_unavailable_section`.

use anyhow::Result;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use loom_daemon::daemon_install_state;
use loom_daemon::daemon_pidfile;
use loom_daemon::health::{self, HealthInputs, HealthReport};
use loom_daemon::pipeline_snapshot::{self, GhPipelineSource, PipelineMetrics};
use loom_daemon::types::{DaemonStatusReport, Request, Response};

use super::common::{query_daemon_bounded, resolve_socket_path};

/// Base per-attempt bound on the IPC round-trip on an *unloaded* host —
/// unchanged from before Issue #6103: `health` still advertises a "< 5s
/// typical" total budget across *all* sections on a quiet host, and a daemon
/// that cannot answer within 2s there is already a finding worth reporting
/// (as `alive-but-unresponsive`) rather than waiting on.
///
/// # This is no longer the whole story (#6103)
///
/// On a *busy* host this fixed 2s budget used to disagree with `status`'s own
/// load-scaled 5-30s budget ([`super::status::resolve_status_timeout`]) and
/// the watchdog's 15s-per-tick / 3-consecutive-failure budget
/// (`loom-daemon-watchdog.sh`'s `PROBE_TIMEOUT_SECS` /
/// `LOOM_WATCHDOG_IPC_PROBE_FAIL_THRESHOLD`) — so `health` alone flagged
/// `overall DEGRADED`/exit `1` against a daemon 29 straight watchdog ticks
/// (and 5/5 immediate manual IPC probes) confirmed was healthy. Reconciled
/// without simply raising this number (which only narrows the false-alarm
/// window, never closes it):
///
/// 1. [`resolve_ipc_timeout`] scales this base by observed host load — the
///    same [`super::status::scale_timeout_for_load`] rule `status` uses — and
///    honors the shared `LOOM_DAEMON_IPC_TIMEOUT_MS` floor
///    ([`super::common::apply_ipc_timeout_env_floor`]), so the two commands
///    can no longer silently disagree about the same busy host.
/// 2. [`query_status`] retries **exactly once** on a *timeout*-classified
///    failure (never a hard one) before ever reporting a failure at all — the
///    single-invocation analog of the watchdog's consecutive-failure
///    debounce, via [`loom_daemon::health::ipc_error_is_probe_timeout`].
/// 3. If both attempts still fail, [`loom_daemon::health::assess_liveness`]
///    reports a lone surviving timeout against a demonstrably-alive daemon as
///    `Verdict::Unknown` ("probe budget exceeded"), not `Verdict::Degraded`
///    ("confirmed unhealthy") — so it does not, by itself, flip `overall` to
///    DEGRADED.
const BASE_IPC_TIMEOUT: Duration = Duration::from_secs(2);

/// Resolve the effective per-attempt IPC timeout for this invocation (#6103
/// AC1): [`BASE_IPC_TIMEOUT`] scaled by observed host load via
/// [`super::status::scale_timeout_for_load`] (the identical rule `status`
/// applies to its own IPC budget), then floored — never lowered — by the
/// shared `LOOM_DAEMON_IPC_TIMEOUT_MS` override.
fn resolve_ipc_timeout() -> Duration {
    let logical_cpus = loom_daemon::cpu_headroom::logical_cpu_count();
    let loadavg_1m = loom_daemon::cpu_headroom::read_loadavg_1m();
    let load_per_core = loom_daemon::cpu_headroom::load_per_core_from(loadavg_1m, logical_cpus);
    let scaled = super::status::scale_timeout_for_load(BASE_IPC_TIMEOUT, load_per_core);
    super::common::apply_ipc_timeout_env_floor(scaled)
}

/// Handle `loom-daemon health [--since 30m] [--json]`.
///
/// Never returns `Err` for a *health* problem — an unhealthy fleet is a
/// successful report with a non-zero exit code. `Err` is reserved for the
/// command being unable to run at all (e.g. an unparseable `--since`).
pub(crate) async fn handle_health_command(since: Option<String>, json: bool) -> Result<()> {
    let window = match since.as_deref() {
        Some(raw) => health::parse_since(raw).map_err(|e| anyhow::anyhow!(e))?,
        None => Duration::from_secs(health::DEFAULT_WINDOW_SECS),
    };

    let report = collect(window).await;

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", report.render_human());
    }
    std::io::Write::flush(&mut std::io::stdout()).ok();
    std::process::exit(report.exit_code());
}

/// Collect every input and assess. Split out of [`handle_health_command`] so
/// the exit-code path is the only thing that call site adds.
async fn collect(window: Duration) -> HealthReport {
    // 1. The IPC round-trip — the only source for the dispatch/tokens/roles
    //    sections, and the strongest liveness signal there is.
    let (status, ipc_error) = query_status().await;

    // 2. Local liveness probes. Both are cheap, bounded, and run regardless of
    //    whether IPC succeeded: the collector records them either way so a
    //    `--json` consumer can see the corroborating evidence.
    let install_state = daemon_install_state::probe();
    let pgrep_pids = daemon_install_state::pgrep_daemon_pids();

    // 2b. The pid file, observed against the path the DAEMON resolved (#4774)
    //     when it answered — same rule as `.ranking` below and `status`'s token
    //     probe (#4292): never re-derive from this CLI process's own cwd/env
    //     when the daemon has told us which file it actually writes. Falling
    //     back to a local resolution only for an unreachable / pre-#4774
    //     daemon. Observed unconditionally so `--json` carries the corroborating
    //     evidence either way; the collector decides what it means.
    let pid_file = status
        .as_ref()
        .and_then(|r| r.pid_file.clone())
        .or_else(daemon_pidfile::resolve_pid_file_path)
        .map(|path| daemon_pidfile::observe(&path));

    // 3. `.ranking` staleness, against the pool directory the DAEMON resolved
    //    (#4292) rather than one re-derived from this CLI process's own cwd —
    //    the same rule `status`'s client-side token probe follows.
    let (ranking_present, ranking_age_secs) = probe_ranking(status.as_ref());

    // 4. The forge fan-out for queue depth + review pipeline + throughput.
    //    Only the metrics those sections actually read (#4761's
    //    `PipelineMetrics::HEALTH`, widened by #5021 to carry the review-side
    //    axes the `queues` verdict now consumes), over the requested window,
    //    across the roots the daemon reported. Skipped entirely when the daemon
    //    is unreachable: without its root list there is nothing to query, and
    //    the sections honestly report "not collected".
    //
    //    4a. Before fanning out to N repos, check ONCE whether this process
    //    can even run `gh` at all (#5061). A missing/non-executable `gh` — the
    //    common case being a non-login SSH shell whose PATH lacks
    //    `~/.local/bin` / `/opt/homebrew/bin` (the same failure class as
    //    #4875) — would otherwise fail identically for every managed repo,
    //    rendering as "forge query FAILED for: <every repo>" and reading like
    //    a forge outage rather than what it actually is: a fact about this
    //    caller's own environment. `Some` here means the fan-out below is
    //    skipped entirely (there is no value in spawning N `gh` calls already
    //    known to fail the same way), and `queues`/`throughput` render the
    //    single fact instead — see `health::assess_queues`/`assess_throughput`.
    let gh_unavailable =
        pipeline_snapshot::probe_gh_availability(Path::new(pipeline_snapshot::DEFAULT_GH_BIN))
            .err();
    let pipeline = match (&status, &gh_unavailable) {
        (Some(report), None) => {
            let roots = report
                .per_repo
                .iter()
                .map(|r| r.root.clone())
                .collect::<Vec<_>>();
            let source = Arc::new(
                GhPipelineSource::new()
                    .with_metrics(PipelineMetrics::HEALTH)
                    .with_merge_window(
                        chrono::Duration::from_std(window)
                            .unwrap_or_else(|_| chrono::Duration::hours(24)),
                    ),
            );
            Some(pipeline_snapshot::collect_pipeline_snapshots(source, roots).await)
        }
        _ => None,
    };

    // 5. The daemon log's newest `work_finder:` line (#4824) — a *corroborating*
    //    signal, never a derivation: it exists so the collector refuses to call
    //    the work finder dead while the daemon's own log shows it ticking. One
    //    bounded tail read, and only on the path that can consume it (the
    //    daemon reported no tick) — a reported tick is already the stronger
    //    signal, so probing then would be pure I/O for a field nothing reads.
    let work_finder_log_tick_age_secs = status
        .as_ref()
        .is_some_and(|r| r.last_work_finder_tick.is_none())
        .then(health::probe_work_finder_log_tick_age)
        .flatten();

    health::assess(&HealthInputs {
        at: chrono::Utc::now(),
        window,
        status,
        ipc_error,
        install_state,
        pgrep_pids,
        pid_file,
        ranking_present,
        ranking_age_secs,
        pipeline,
        gh_unavailable,
        // This CLI process's own build commit (#4824), compared daemon-side
        // against `DaemonStatusReport::daemon_build_commit` so a newer CLI
        // querying an older daemon reports build skew rather than a phantom
        // dead work finder.
        cli_build_commit: loom_daemon::self_update::BUILT_COMMIT.to_string(),
        work_finder_log_tick_age_secs,
    })
}

/// One bounded `DaemonStatus` round-trip, collapsed to
/// `(Some(report), None)` / `(None, Some(why))`.
///
/// #6103: a first attempt that merely **timed out** (never a hard failure —
/// see [`loom_daemon::health::ipc_error_is_probe_timeout`]) is retried
/// exactly once, with the same resolved budget, before this function reports
/// a failure at all. A single bounded miss on a busy host is not, by itself,
/// evidence of an unhealthy daemon; this is the one-shot-CLI-invocation
/// substitute for the watchdog's cross-tick consecutive-failure debounce,
/// which has no history to lean on here.
async fn query_status() -> (Option<DaemonStatusReport>, Option<String>) {
    let socket_path = match resolve_socket_path() {
        Ok(p) => p,
        Err(e) => return (None, Some(format!("could not resolve socket path: {e}"))),
    };
    let timeout = resolve_ipc_timeout();
    match query_status_once(&socket_path, timeout).await {
        Ok(report) => (Some(report), None),
        Err(first_err) if health::ipc_error_is_probe_timeout(&first_err) => {
            match query_status_once(&socket_path, timeout).await {
                Ok(report) => (Some(report), None),
                Err(second_err) => (None, Some(second_err)),
            }
        }
        Err(first_err) => (None, Some(first_err)),
    }
}

/// A single connect + `DaemonStatus` attempt, collapsed to the same rendered
/// `Err` string [`query_status`] has always produced. Never itself retried —
/// that decision lives one layer up, in [`query_status`].
async fn query_status_once(
    socket_path: &Path,
    timeout: Duration,
) -> Result<DaemonStatusReport, String> {
    match query_daemon_bounded(socket_path, &Request::DaemonStatus, timeout).await {
        Ok(Response::DaemonStatus(report)) => Ok(*report),
        Ok(Response::Error { message }) => Err(format!("daemon error: {message}")),
        Ok(other) => Err(format!("unexpected response: {other:?}")),
        Err(e) => Err(e.to_string()),
    }
}

/// Stat the resolved pool's `.ranking`: `(present, age_secs)`.
///
/// The pool directory comes from the daemon's own
/// [`DaemonStatusReport::token_pool_dir`] (#4292) when available, falling back
/// to this process's cwd resolution only for a pre-#4292 daemon or an
/// unreachable one.
fn probe_ranking(status: Option<&DaemonStatusReport>) -> (bool, Option<u64>) {
    let dir = match status.and_then(|r| r.token_pool_dir.clone()) {
        Some(dir) => dir,
        None => {
            let Ok(ws) = super::tokens::resolve_tokens_workspace(".") else {
                return (false, None);
            };
            loom_daemon::tokens_pool::paths::resolve_tokens_dir(&ws)
        }
    };
    ranking_state(&dir)
}

/// `(present, age_secs)` for `<dir>/.ranking`. Delegates to
/// [`loom_daemon::capacity::ranking_file_state`] (#5269) — the same probe
/// `ipc::build_daemon_status` uses to populate each registered repo's own
/// `RepoStatus::ranking_present`/`ranking_age_secs`, so this CLI's single
/// anchored-pool probe and the daemon's per-repo snapshot can never disagree
/// about what "present"/"age" means for the same directory.
fn ranking_state(dir: &Path) -> (bool, Option<u64>) {
    loom_daemon::capacity::ranking_file_state(dir)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn ranking_state_reports_absent_when_there_is_no_file() {
        let tmp = tempfile::tempdir().unwrap();
        let (present, age) = ranking_state(tmp.path());
        assert!(!present);
        assert_eq!(age, None);
    }

    #[test]
    fn ranking_state_reports_present_and_fresh_for_a_just_written_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".ranking"), "a|available|0.1\n").unwrap();
        let (present, age) = ranking_state(tmp.path());
        assert!(present);
        assert!(age.unwrap() < 60, "a file just written should read as fresh");
    }

    /// Issue #6103 AC1: `health`'s IPC budget must honor the same
    /// `LOOM_DAEMON_IPC_TIMEOUT_MS` floor `status`/`dispatch` already do — one
    /// env var, every client-side IPC round-trip in this binary.
    #[test]
    #[serial_test::serial]
    fn resolve_ipc_timeout_honors_the_shared_env_floor() {
        std::env::set_var(super::super::common::DAEMON_IPC_TIMEOUT_ENV, "9000");
        let timeout = resolve_ipc_timeout();
        std::env::remove_var(super::super::common::DAEMON_IPC_TIMEOUT_ENV);
        assert_eq!(timeout, Duration::from_secs(9));
    }

    /// With no env override, the resolved timeout must never fall below
    /// [`BASE_IPC_TIMEOUT`] regardless of this test-runner host's actual
    /// load — a real regression here would only ever make it larger, never
    /// smaller.
    #[test]
    #[serial_test::serial]
    fn resolve_ipc_timeout_never_undercuts_the_base_without_an_override() {
        std::env::remove_var(super::super::common::DAEMON_IPC_TIMEOUT_ENV);
        assert!(resolve_ipc_timeout() >= BASE_IPC_TIMEOUT);
    }
}
