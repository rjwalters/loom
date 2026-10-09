//! Client-side glue for `fleet-config render`'s live-reload signal (#9597):
//! confirm live-reloadable changes against a running daemon's `DaemonStatus`
//! (the "signal the running daemon over IPC to re-resolve" half of the
//! issue), and record restart-required changes in
//! [`loom_daemon::fleet_store::pending_restart`] so `status` can surface them
//! until the daemon that was running at render time actually restarts.
//!
//! Reuses the existing `DaemonStatus` request rather than adding a new IPC
//! variant: every knob [`loom_daemon::fleet_store::reload`] classifies as
//! live is, by construction, re-read fresh by its consumer with no
//! daemon-side caching — so a `DaemonStatus` round-trip taken *after* the
//! write already reflects the rendered value, and doubles as this module's
//! pid source for the restart-required marker.

use std::path::Path;
use std::time::Duration;

use chrono::Utc;
use loom_daemon::fleet_store::pending_restart;
use loom_daemon::types::{DaemonStatusReport, Request, Response};

use super::common::{query_daemon_bounded, resolve_socket_path};

/// Bound on the confirmation round-trip — this is best-effort observability,
/// not a correctness gate, so it must never make `render` feel hung waiting
/// on a wedged daemon.
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(3);

/// Report a write's live/restart-required split. `workspace` is the render's
/// own `--workspace` root (already resolved to an absolute path), used to
/// match the right entry in `DaemonStatusReport::per_repo` for role-runner
/// confirmation.
pub(crate) fn report(workspace: &Path, live: &[String], restart_required: &[String]) {
    if live.is_empty() && restart_required.is_empty() {
        return;
    }
    let status = fetch_status();
    for line in render_lines(workspace, live, restart_required, status.as_ref()) {
        println!("{line}");
    }
    if restart_required.is_empty() {
        return;
    }
    let Some(pid) = status.as_ref().and_then(|s| s.daemon_pid) else {
        return;
    };
    match pending_restart::record(restart_required.to_vec(), pid, Utc::now()) {
        Ok(()) => println!(
            "  recorded against daemon pid {pid} — `loom-daemon status` reports this until it \
             restarts"
        ),
        Err(e) => eprintln!("note: could not record the pending-restart marker: {e:#}"),
    }
}

/// Pure: the lines [`report`] prints, given whatever `status` (or none) it
/// fetched. Split out so the message shapes are unit-testable without a
/// socket.
fn render_lines(
    workspace: &Path,
    live: &[String],
    restart_required: &[String],
    status: Option<&DaemonStatusReport>,
) -> Vec<String> {
    let mut lines = Vec::new();
    let Some(status) = status else {
        if !live.is_empty() {
            lines.push(format!(
                "live-reloadable (no restart needed): {} — no running daemon to confirm against \
                 right now; applies automatically once one is running",
                live.join(", ")
            ));
        }
        if !restart_required.is_empty() {
            lines.push(format!(
                "restart-required: {} — no daemon is currently running, so the next one to \
                 start reads this directly",
                restart_required.join(", ")
            ));
        }
        return lines;
    };
    if !live.is_empty() {
        let pid_note = status
            .daemon_pid
            .map_or_else(String::new, |p| format!(" (pid {p})"));
        lines.push(format!(
            "live-reloadable (applying now, no restart needed){pid_note}: {}",
            live.join(", ")
        ));
        if live.iter().any(|p| p.starts_with("autonomous.workFinder")) {
            lines.push(format!(
                "  work-finder ceiling now resolves to {} (dynamic cap {})",
                status.configured_max, status.dynamic_cap
            ));
        }
        if live.iter().any(|p| p.starts_with("autonomous.roleRunner")) {
            if let Some(repo) = matching_repo(workspace, status) {
                lines.push(format!(
                    "  role runner (this workspace): enabled={}, roles={:?}",
                    repo.role_runner_enabled, repo.role_runner_roles
                ));
            }
        }
    }
    if !restart_required.is_empty() {
        lines.push(format!(
            "restart-required (will apply on the daemon's next restart): {}",
            restart_required.join(", ")
        ));
    }
    lines
}

/// The `per_repo` entry matching `workspace`, comparing canonicalized paths
/// so a symlinked/relative `--workspace` still lines up with the daemon's
/// own normalized registry root.
fn matching_repo<'a>(
    workspace: &Path,
    status: &'a DaemonStatusReport,
) -> Option<&'a loom_daemon::types::RepoStatus> {
    let canon = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    status.per_repo.iter().find(|r| {
        r.root == workspace
            || r.root == canon
            || std::fs::canonicalize(&r.root).ok().as_deref() == Some(canon.as_path())
    })
}

pub(crate) fn fetch_status() -> Option<DaemonStatusReport> {
    let socket_path = resolve_socket_path().ok()?;
    match block_on_query(&socket_path)? {
        Response::DaemonStatus(report) => Some(*report),
        _ => None,
    }
}

/// Run the one-shot `DaemonStatus` round-trip from synchronous code —
/// `fleet_config.rs`'s `cmd_render` is a plain `fn`, shared by every
/// `fleet-config` sub-verb, most of which do no IPC at all. The
/// `block_in_place` plus `Handle::current().block_on` combination is safe
/// from any call site already running on the daemon binary's multi-thread
/// `#[tokio::main]` runtime (every production call site); a plain `#[test]`
/// that entered no runtime at all falls back to spinning up a throwaway one.
fn block_on_query(socket_path: &Path) -> Option<Response> {
    let fut = query_daemon_bounded(socket_path, &Request::DaemonStatus, CONFIRM_TIMEOUT);
    if tokio::runtime::Handle::try_current().is_ok() {
        tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(fut)).ok()
    } else {
        tokio::runtime::Runtime::new().ok()?.block_on(fut).ok()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use loom_daemon::types::RepoStatus;

    use super::*;
    use crate::cli::status::sample_report::sample_report;

    #[test]
    fn unreachable_daemon_notes_both_buckets_and_never_touches_the_marker() {
        let lines = render_lines(
            Path::new("/repo/a"),
            &["autonomous.roleRunner.enabled".to_string()],
            &["autonomous.hostBreaker.enabled".to_string()],
            None,
        );
        assert!(lines[0].starts_with("live-reloadable"), "{lines:?}");
        assert!(lines[0].contains("no running daemon"), "{lines:?}");
        assert!(lines[1].starts_with("restart-required"), "{lines:?}");
    }

    #[test]
    fn reachable_daemon_confirms_work_finder_ceiling() {
        let status = sample_report();
        let lines = render_lines(
            Path::new("/repo/a"),
            &["autonomous.workFinder.maxConcurrent".to_string()],
            &[],
            Some(&status),
        );
        assert!(lines[0].contains("applying now"), "{lines:?}");
        assert!(
            lines[1].contains(&format!("ceiling now resolves to {}", status.configured_max)),
            "{lines:?}"
        );
    }

    #[test]
    fn reachable_daemon_confirms_role_runner_for_the_matching_repo() {
        let mut status = sample_report();
        status.per_repo = vec![RepoStatus {
            root: PathBuf::from("/repo/a"),
            maintain_only: None,
            role_runner_enabled: true,
            role_runner_roles: vec!["judge".to_string()],
            ..sample_repo_status()
        }];
        let lines = render_lines(
            Path::new("/repo/a"),
            &["autonomous.roleRunner.enabled".to_string()],
            &[],
            Some(&status),
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("enabled=true") && l.contains("judge")),
            "{lines:?}"
        );
    }

    #[test]
    fn restart_required_only_prints_no_live_confirmation() {
        let status = sample_report();
        let lines = render_lines(
            Path::new("/repo/a"),
            &[],
            &["fleet.autoApply".to_string()],
            Some(&status),
        );
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with("restart-required"), "{lines:?}");
    }

    fn sample_repo_status() -> RepoStatus {
        RepoStatus {
            root: PathBuf::new(),
            maintain_only: None,
            priority: 0,
            in_flight_count: 0,
            health_gate_halted: false,
            quarantined_issues: vec![],
            health_gate_not_evaluated: false,
            health_gate_not_evaluated_reason: None,
            health_gate_enabled: None,
            health_gate_verdict_at: None,
            root_missing: false,
            health_gate_deferred: false,
            health_gate_deferred_reason: None,
            health_gate_verdict_tier: None,
            role_runner_enabled: false,
            role_runner_roles: vec![],
            role_runner_intervals: std::collections::BTreeMap::new(),
            role_runner_on_idle_roles: vec![],
            role_runner_on_idle_promotions: vec![],
            role_runner_env_override: None,
            role_runner_shard: None,
            token_pool_dir: None,
            ranking_present: false,
            ranking_age_secs: None,
            stash_total_count: 0,
            stash_quarantine_count: 0,
            stash_oldest_age_secs: None,
            stash_non_quarantine_unrecoverable_count: 0,
            stash_non_quarantine_unrecoverable_oldest_age_secs: None,
            sweep_command_missing: false,
        }
    }
}
