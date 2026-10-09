//! What the reaper does with a sweep whose process just ended, beyond the
//! registry bookkeeping: reap the process group it left behind (#4980) and
//! remove the run target dir it built into (#11031).
use super::*;

impl SweepRegistry {
    /// Called once per ended sweep process, from the reaper's death path and
    /// from [`finish_cancel`](Self::finish_cancel) (an operator cancel, or the
    /// watchdog's auto-cancel just before its re-dispatch, so the abandoned
    /// run's dir does not sit next to the fresh one).
    ///
    /// `reap_group` is false on the cancel path, which has already signalled
    /// the group itself.
    pub(crate) fn on_sweep_process_end(
        &mut self,
        sweep_id: &str,
        kind: &SweepKind,
        pid: u32,
        pgid: Option<u32>,
        reap_group: bool,
    ) {
        if let Some(pgid) = pgid.filter(|_| reap_group) {
            let issue = match kind {
                SweepKind::Issue(n) => Some(*n),
                SweepKind::PrSet(_) => None,
            };
            self.reap_orphaned_group(sweep_id, issue, pgid);
        }
        let _ = crate::run_target_dir::sweep_end::reclaim_at_sweep_end(
            &self.config.workspace_root,
            sweep_id,
            pid,
            pgid,
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests;
