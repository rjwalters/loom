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
    /// removed from `self.children`; `exit_code` is `None` only when liveness
    /// came from the fallback probe. A leader killed by signal N reports the
    /// shell convention `128 + N` (#11076: SIGKILL → 137, SIGTERM → 143), so a
    /// signal death is distinguishable from "no exit status observed".
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
                    let code = exit_code_of(status);
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

/// Issue #11076: the exit code of a reaped child, mapping a signal death to
/// the shell convention `128 + signal` instead of `ExitStatus::code()`'s
/// `None` — which the reaper otherwise reads as "no exit status observed".
pub(crate) fn exit_code_of(status: std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .or_else(|| status.signal().map(|sig| 128 + sig))
}

#[cfg(test)]
mod tests {
    use super::exit_code_of;
    use std::process::Command;

    /// #11076: a SIGKILLed / SIGTERMed child must surface as 137 / 143 — the
    /// codes `prless_retry::external_kill_exemption` keys on — not `None`.
    #[test]
    fn a_signal_killed_child_reports_128_plus_the_signal() {
        for (sig, want) in [("KILL", 137), ("TERM", 143)] {
            let status = Command::new("sh")
                .args(["-c", &format!("kill -{sig} $$")])
                .status()
                .expect("spawn sh");
            assert_eq!(status.code(), None, "the raw status carries no code");
            assert_eq!(exit_code_of(status), Some(want), "SIG{sig}");
        }
        let status = Command::new("sh")
            .args(["-c", "exit 3"])
            .status()
            .expect("spawn sh");
        assert_eq!(exit_code_of(status), Some(3), "an ordinary exit is unchanged");
    }
}
