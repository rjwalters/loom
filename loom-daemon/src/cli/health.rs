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

/// Bound on the IPC round-trip. Shorter than `status`'s 5s: `health` advertises
/// a "< 5s typical" total budget across *all* sections, and a daemon that
/// cannot answer within 2s is already a finding this command should report
/// (as `alive-but-unresponsive`) rather than wait on.
const IPC_TIMEOUT: Duration = Duration::from_secs(2);

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
    let pipeline = match &status {
        Some(report) => {
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
        None => None,
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
async fn query_status() -> (Option<DaemonStatusReport>, Option<String>) {
    let socket_path = match resolve_socket_path() {
        Ok(p) => p,
        Err(e) => return (None, Some(format!("could not resolve socket path: {e}"))),
    };
    match query_daemon_bounded(&socket_path, &Request::DaemonStatus, IPC_TIMEOUT).await {
        Ok(Response::DaemonStatus(report)) => (Some(*report), None),
        Ok(Response::Error { message }) => (None, Some(format!("daemon error: {message}"))),
        Ok(other) => (None, Some(format!("unexpected response: {other:?}"))),
        Err(e) => (None, Some(e.to_string())),
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

/// `(present, age_secs)` for `<dir>/.ranking`. Extracted (and pure w.r.t. the
/// filesystem argument) so the age computation is unit-testable.
fn ranking_state(dir: &Path) -> (bool, Option<u64>) {
    let path = dir.join(".ranking");
    let Ok(meta) = std::fs::metadata(&path) else {
        return (false, None);
    };
    let age = meta
        .modified()
        .ok()
        .and_then(|m| m.elapsed().ok())
        .map(|d| d.as_secs());
    (true, age)
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
}
