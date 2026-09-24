//! Retained-child reaping and identity-aware liveness checks.
use super::*;

impl SweepRegistry {
    /// Determine whether a sweep's child has terminated, reaping it when it
    /// has. Prefers the retained `Child` handle: `try_wait()` reaps an exited
    /// child (no zombie) and yields the real exit status. Falls back to the
    /// **identity-paired** liveness probe ([`pid_identity::tracked_pid_alive`])
    /// for reconstructed entries with no handle.
    ///
    /// Issue #7935: that fallback used to be a bare `kill(pid, 0)`, which knows
    /// nothing about *which* process wears the pid number today. A leader that
    /// died while no daemon was running — a crash, a restart, an `auto_update`
    /// roll — can have its pid recycled onto an unrelated process before the
    /// next daemon starts, and the bare probe then reports "alive" forever:
    /// the entry never goes terminal, `restart --drain` never drains, and
    /// [`reap_orphaned_group`](Self::reap_orphaned_group) (which only ever
    /// fires at the terminal transition) never runs for the real leader.
    /// Pairing the pid with the tracked process's start time — compared against
    /// this entry's own `started_at` — makes a recycled pid read as dead. The
    /// probe is fail-safe in the #4691 direction: an underivable start time
    /// leaves the pre-#7935 verdict untouched.
    ///
    /// Returns `(is_dead, exit_code)`. On a handle-observed exit the handle is
    /// removed from `self.children`; `exit_code` is `None` when the child was
    /// terminated by a signal (no clean code) or when liveness came from the
    /// fallback probe.
    pub(crate) fn poll_liveness(&mut self, sweep_id: &str, pid: u32) -> (bool, Option<i32>) {
        let started_at = self.entries.get(sweep_id).map(|info| info.started_at);
        if let Some(child) = self.children.get_mut(sweep_id) {
            match child.try_wait() {
                Ok(Some(status)) => {
                    crate::observability::lifecycle::child_exited(
                        &self.config.workspace_root,
                        sweep_id,
                        if status.success() {
                            "success"
                        } else if status.code().is_some() {
                            "failure"
                        } else {
                            "signal"
                        },
                    );
                    let code = status.code();
                    self.children.remove(sweep_id);
                    (true, code)
                }
                Ok(None) => (false, None),
                Err(e) => {
                    log::warn!("sweep_registry: try_wait for {sweep_id} (pid {pid}) failed: {e}");
                    let dead = !pid_identity::tracked_pid_alive(pid, started_at);
                    if dead {
                        self.children.remove(sweep_id);
                    }
                    (dead, None)
                }
            }
        } else {
            (!pid_identity::tracked_pid_alive(pid, started_at), None)
        }
    }

    /// Reap the retained `Child` handle for `sweep_id`, blocking briefly until
    /// it exits. Called after `cancel` has SIGKILL'd (or observed the exit of)
    /// the child so the OS-level zombie is reclaimed under the daemon PID.
    /// No-op when no handle is retained (reconstructed / test-injected entry).
    pub(crate) fn reap_handle(&mut self, sweep_id: &str) -> Option<std::process::ExitStatus> {
        self.children.remove(sweep_id).and_then(|mut child| {
            // Bounded: we only reach here once the child has exited or has
            // just been SIGKILL'd, so `wait()` returns promptly.
            child.wait().ok()
        })
    }
}
