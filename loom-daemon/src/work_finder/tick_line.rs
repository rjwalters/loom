//! The multi-workspace loop's `work_finder: tick` INFO line, split out of the
//! size-frozen `work_finder.rs` when #11191 added the disk admission figures
//! to it: the free space, the floor, the in-flight reservation and each
//! repo's disk charge, so a cap of N can be explained from the log afterwards.

use super::TickReport;

/// The tick-level figures the line reports beside `report`'s counters.
#[derive(Debug, Clone)]
pub struct TickLine<'a> {
    pub max_concurrent: usize,
    pub pool_size: usize,
    pub token_limit: usize,
    pub min_workspace_healthy: usize,
    pub disk: usize,
    pub ram: usize,
    pub configured_max: usize,
    pub max_admissions_per_tick: usize,
    pub workspaces: usize,
    /// [`crate::disk_admission::note`]: the disk budget's figures.
    pub disk_note: &'a str,
}

/// Whether the tick did anything worth an INFO line.
#[must_use]
pub fn worth_logging(report: &TickReport) -> bool {
    report.dispatched > 0
        || report.errors > 0
        || report.skipped_quarantined > 0
        || report.skipped_workspace_commands_missing > 0
        || report.skipped_backoff > 0
        || report.skipped_pr_open_backoff > 0
        || report.skipped_noop_cooldown > 0
        || report.skipped_declined > 0
        || report.skipped_prless_retry > 0
        || report.skipped_recheck_interval > 0
        || report.skipped_host_constraint > 0
        || report.skipped_pr_open > 0
        || report.skipped_peer_claim > 0
        || report.deferred_ramp_cap > 0
        || report.deferred_saturation > 0
        || report.deferred_out_of_slice > 0
        || report.deferred_repo_cap > 0
}

/// The line itself.
#[must_use]
pub fn render(report: &TickReport, t: &TickLine<'_>) -> String {
    format!(
        "work_finder: tick — cap {} (pool={}, healthy={} [fallback_root probe only], \
         min_workspace_healthy={} [minimum across every registered workspace's own resolved \
         pool, #7527], disk={} [{}], ram={}, ceiling={}, ramp_cap={}); {} workspace(s), \
         {} seen, {} dispatched, {} labeled-skip, {} in-flight-skip, \
         {} quarantine-skip, {} workspace-commands-missing-skip, \
         {} backoff-skip, {} pr-open-backoff, {} noop-cooldown-skip, \
         {} declined-skip, {} prless-retry-skip, \
         {} recheck-interval-skip, \
         {} host-constraint-skip, \
         {} pr-open-skip, \
         {} peer-claim-skip, \
         {} deferred (capacity), {} deferred (ramp), \
         {} deferred (host saturated), {} deferred (out-of-slice, #6243), \
         {} deferred (repo cap, #9090), {} deferred (build back-off, #9410), \
         {} error(s), {} cross-host-collision(s)",
        t.max_concurrent,
        t.pool_size,
        t.token_limit,
        t.min_workspace_healthy,
        t.disk,
        t.disk_note,
        t.ram,
        t.configured_max,
        t.max_admissions_per_tick,
        t.workspaces,
        report.seen,
        report.dispatched,
        report.skipped_labeled,
        report.skipped_in_flight,
        report.skipped_quarantined,
        report.skipped_workspace_commands_missing,
        report.skipped_backoff,
        report.skipped_pr_open_backoff,
        report.skipped_noop_cooldown,
        report.skipped_declined,
        report.skipped_prless_retry,
        report.skipped_recheck_interval,
        report.skipped_host_constraint,
        report.skipped_pr_open,
        report.skipped_peer_claim,
        report.deferred_capacity,
        report.deferred_ramp_cap,
        report.deferred_saturation,
        report.deferred_out_of_slice,
        report.deferred_repo_cap,
        report.deferred_build_backoff,
        report.errors,
        report.collisions
    )
}

/// Log the line at INFO when the tick did anything.
pub fn log(report: &TickReport, t: &TickLine<'_>) {
    if worth_logging(report) {
        log::info!("{}", render(report, t));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tick_line_names_the_disk_reservation_and_charges() {
        let report = TickReport {
            dispatched: 1,
            ..TickReport::default()
        };
        let note = "free 50GB - floor 3GB - reserved 30GB = 17GB; charges [loom:29GB(observed)]";
        let t = TickLine {
            max_concurrent: 3,
            pool_size: 4,
            token_limit: 4,
            min_workspace_healthy: 4,
            disk: 2,
            ram: 9,
            configured_max: 12,
            max_admissions_per_tick: 2,
            workspaces: 1,
            disk_note: note,
        };
        assert!(worth_logging(&report));
        let line = render(&report, &t);
        assert!(line.contains(&format!("disk=2 [{note}]")), "{line}");
        assert!(line.contains("1 dispatched"), "{line}");
        assert!(!worth_logging(&TickReport::default()));
    }
}
