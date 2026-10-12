//! What the reaper does with a sweep whose process just ended, beyond the
//! registry bookkeeping: remove the run target dir it built into (#11031).
//!
//! Signalling the process group the sweep left behind is NOT done here. The
//! reaper's death path owns that through
//! [`await_group_drain`](SweepRegistry::await_group_drain) (#4980, #11076),
//! and the cancel path through `signal_sweep`. This runs after them.
use super::*;

impl SweepRegistry {
    /// Schedule the removal of an ended sweep's run target dir(s), off the
    /// registry lock. Called once per ended sweep, at its terminal transition:
    ///
    /// - from the reaper's death path, only on [`GroupDrain::Drained`]. While
    ///   the dead leader's group is [`GroupDrain::Draining`] the sweep is not
    ///   over (#11076: a live member may be the wrapper's retry of the same
    ///   session, building into the same dir), so the entry stays live and
    ///   nothing is scheduled;
    /// - from [`finish_cancel`](Self::finish_cancel) (an operator cancel, or
    ///   the watchdog's auto-cancel just before its re-dispatch, so the
    ///   abandoned run's dir does not sit next to the fresh one).
    ///
    /// It never signals anything. `Drained` can also mean the group could not
    /// be waited on (the #11076 release cap, a recycled group id), and a
    /// cancel's SIGKILL may not have landed yet, so the removal thread still
    /// runs every gate itself and keeps the dir when the group has a member.
    pub(crate) fn on_sweep_process_end(&mut self, sweep_id: &str, pid: u32, pgid: Option<u32>) {
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
