//! `loom-daemon status` lines for the two conditions where the daemon has
//! stopped doing something and will not resume on its own judgement alone —
//! the same pairing `health::holds` assesses (#7590, #7708):
//!
//! - worktree removals backed off after repeated/permission-class failures;
//! - token pools holding **all** sweep dispatch because not one account in
//!   them can spawn.
//!
//! Both render from a plain projection already on the status wire, and both
//! return their lines instead of printing them — so a test asserts on the
//! text directly rather than capturing stdout. The caller in
//! `status_render::print_status_human` prints whatever comes back, in order,
//! at the position the #7590 block used to occupy.

use loom_daemon::types::DaemonStatusReport;

/// Every stalled-activity warning line this report implies, in print order:
/// backed-off worktree removals first (#7590), then held token pools
/// (#7708). Empty — the steady state — means nothing is printed at all.
///
/// Each line already carries its own leading blank line, matching the
/// surrounding blocks in `print_status_human`.
pub(crate) fn render_lines(report: &DaemonStatusReport) -> Vec<String> {
    let mut lines = render_stuck_worktree_lines(report);
    lines.extend(render_pool_hold_lines(report));
    lines
}

/// Backed-off worktree removals (Issue #7590): a removal that failed with a
/// permission-class cause (root-owned build-cache directories are the
/// motivating case), or that failed `REMOVAL_FAILURE_CAP` consecutive times
/// of any cause, is no longer retried every reap tick — see
/// `health::holds::assess_worktree_reaper` for the same signal rolled up into
/// a verdict.
fn render_stuck_worktree_lines(report: &DaemonStatusReport) -> Vec<String> {
    if report.stuck_worktree_reclaims.is_empty() {
        return Vec::new();
    }
    let mut lines = vec![format!(
        "\nWARNING: {} worktree removal(s) backed off after repeated/permission-class \
         failures (#7590) — will not self-resolve without operator intervention:",
        report.stuck_worktree_reclaims.len()
    )];
    lines.extend(report.stuck_worktree_reclaims.iter().map(|r| {
        format!(
            "  {}-{} ({} attempt(s) since {}) in {}: {}",
            r.kind,
            r.number,
            r.attempt_count,
            r.first_failure_at,
            r.repo_root.display(),
            r.cause
        )
    }));
    lines
}

/// Held token pools (Issue #7708, surfaced by #7990): one line per held pool,
/// naming the pool, how dead it is (`0/N` spawnable), how long it has been
/// held, and the clear estimate.
///
/// This is the answer to "why is nothing dispatching?" — before #7990 the
/// only place that answer existed was an edge-triggered daemon log line, so
/// an operator who had not been tailing the log at the moment the hold armed
/// had nothing on this surface to find.
fn render_pool_hold_lines(report: &DaemonStatusReport) -> Vec<String> {
    if report.pool_exhaustion_holds.is_empty() {
        return Vec::new();
    }
    let mut lines = vec![format!(
        "\nWARNING: {} token pool(s) EXHAUSTED — ALL sweep dispatch is held for every \
         workspace resolving to them (#7708). Run `loom-daemon tokens check --ranking`, \
         or `loom-daemon tokens unblock <name>`:",
        report.pool_exhaustion_holds.len()
    )];
    lines.extend(report.pool_exhaustion_holds.iter().map(|h| {
        format!(
            "  {}: 0/{} accounts spawnable — pool exhausted hold until {} (held since {}{})",
            h.dir.display(),
            h.total,
            h.next_clear_at.to_rfc3339(),
            h.since.to_rfc3339(),
            if h.wrapper_observed {
                ", armed by a real token-selection death"
            } else {
                ""
            }
        )
    }));
    lines
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{render_lines, render_pool_hold_lines};
    use chrono::{Duration, TimeZone, Utc};
    use loom_daemon::types::{DaemonStatusReport, PoolExhaustionHoldStatus, StuckWorktreeReclaim};
    use std::path::PathBuf;

    fn now() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 17, 12, 0, 0).unwrap()
    }

    fn hold(dir: &str, total: usize) -> PoolExhaustionHoldStatus {
        PoolExhaustionHoldStatus {
            dir: PathBuf::from(dir),
            total,
            since: now() - Duration::minutes(42),
            next_clear_at: now() + Duration::minutes(5),
            wrapper_observed: false,
        }
    }

    fn report_with(holds: Vec<PoolExhaustionHoldStatus>) -> DaemonStatusReport {
        DaemonStatusReport {
            pool_exhaustion_holds: holds,
            ..DaemonStatusReport::default()
        }
    }

    #[test]
    fn a_healthy_host_renders_nothing() {
        assert!(render_lines(&DaemonStatusReport::default()).is_empty());
    }

    /// AC3: one line per held pool, naming the clear estimate.
    #[test]
    fn one_line_per_held_pool_names_the_clear_estimate() {
        let report = report_with(vec![hold("/pool/a", 6), hold("/pool/b", 2)]);
        let lines = render_pool_hold_lines(&report);
        assert_eq!(lines.len(), 3, "one header + one line per pool: {lines:?}");
        assert!(lines[0].contains("2 token pool(s) EXHAUSTED"));
        assert!(lines[1].starts_with("  /pool/a: 0/6 accounts spawnable"));
        assert!(lines[1].contains(&format!(
            "pool exhausted hold until {}",
            (now() + Duration::minutes(5)).to_rfc3339()
        )));
        assert!(lines[2].starts_with("  /pool/b: 0/2 accounts spawnable"));
    }

    #[test]
    fn a_wrapper_observed_hold_says_so() {
        let mut h = hold("/pool/a", 6);
        h.wrapper_observed = true;
        let lines = render_pool_hold_lines(&report_with(vec![h]));
        assert!(lines[1].contains("armed by a real token-selection death"));
    }

    /// The #7590 block keeps its own wording and prints ahead of the pool
    /// holds — this module changed where those lines are built, not what
    /// they say or when they appear.
    #[test]
    fn stuck_worktree_lines_come_first_and_are_unchanged() {
        let mut report = report_with(vec![hold("/pool/a", 6)]);
        report.stuck_worktree_reclaims = vec![StuckWorktreeReclaim {
            repo_root: PathBuf::from("/repo/a"),
            kind: "issue".to_string(),
            number: 42,
            path: PathBuf::from("/repo/a/.loom/worktrees/issue-42"),
            cause: "Permission denied".to_string(),
            first_failure_at: now() - Duration::hours(3),
            last_attempt_at: now(),
            attempt_count: 7,
        }];
        let lines = render_lines(&report);
        assert!(lines[0].contains("1 worktree removal(s) backed off"));
        assert!(lines[1].starts_with("  issue-42 (7 attempt(s) since "));
        assert!(lines[1].ends_with(" in /repo/a: Permission denied"));
        assert!(lines[2].contains("1 token pool(s) EXHAUSTED"));
    }
}
