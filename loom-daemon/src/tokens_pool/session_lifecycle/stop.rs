//! `SessionLifecycle::stop`: the operator stop and its two races with the
//! session reconcile pass (issues #10453, #10661). A child module of
//! `session_lifecycle` so it shares the lifecycle's private state; it lives
//! in its own file for the file-size ratchet (#7711).
//!
//! # The two races
//!
//! A reconcile pass checks the hold before it touches Docker and again just
//! before a `docker start`/`run`. A pass that passed that last check just
//! before `stop` wrote the hold can still land one start:
//!
//! * **Container present** — its `docker start` lands between `stop`'s
//!   `docker stop` and `docker rm`, so `rm` finds the container running.
//!   `stop` re-inspects, re-applies the in-flight-exec refusal, and stops and
//!   removes it once more.
//! * **Container missing** (#10661) — `stop` saw no container, so it had
//!   nothing to stop, but the pass's `docker run` can create one right after
//!   the hold is written: the account is then running **and** held, and the
//!   pass will never touch it again. So when no container was seen, `stop`
//!   inspects once more *after* writing the hold, and stops and removes
//!   whatever is there now, under the same refusal.
//!
//! These retries are `stop`'s half of a **two-sided** guarantee; neither
//! side alone closes the race. A start already in flight when the hold is
//! written (`docker run` can take minutes on an image pull) can finish after
//! both of `stop`'s inspects, so no fixed number of retries here would be
//! enough. The other half is the reconciler's: it re-checks the hold after
//! its own `docker start`/`run` returns and undoes the start if it is now
//! held (`session_lifecycle/start.rs`). With `stop` writing the hold at T1
//! and the reconciler re-checking at T2: if T2 is after T1, the reconciler
//! sees the hold and removes its container; if T2 is before T1, its start
//! had returned before the hold existed, so `stop`'s inspect after writing
//! it sees the container and removes it.
//!
//! The busy refusal applies on every path unless `--force`. The dispatch lock
//! (`session_dispatch_lock`) is the caller's: `accounts session stop` without
//! `--force` takes it exclusively before calling `stop` and keeps it until
//! `stop` returns, so it covers both retries too, and a dispatch that is only
//! starting cannot slip into a recreated container in between.

use anyhow::Result;

use super::{
    container_name, find_codex_account, session_hold, ContainerRunner, ContainerState,
    SessionLifecycle, SessionStatus, STOP_GRACE,
};

impl<R: ContainerRunner> SessionLifecycle<R> {
    /// Tear down the container cleanly. Refuses (unless `force`) when an
    /// in-flight `docker exec` is detected, per `session_lifecycle`'s
    /// restart-safety doc comment. Idempotent: a session that is already
    /// stopped/absent is success, not an error.
    ///
    /// A stop is deliberate, so it stays down: an operator hold
    /// ([`session_hold::HOLD_FILE`]) is written *before* `docker stop`, and
    /// the session reconciler leaves a held account alone until an operator
    /// `start` lifts it (issue #10453). See this module's docs for the races
    /// with a reconcile pass that this closes.
    pub fn stop(&self, name: &str, force: bool) -> Result<SessionStatus> {
        let account = find_codex_account(&self.workspace, name)?;
        let name = account.id.name.as_str();
        let container = container_name(name);
        let mut state = self.runner.inspect(&container)?;
        if let Some(state) = &state {
            self.refuse_unless_stoppable(name, &container, state, force)?;
        }
        session_hold::write_hold(&account.credential_reference, session_hold::now_unix_ms())?;
        if state.is_none() {
            // A pass whose last hold check ran before the hold above was
            // written may have just created the container (#10661).
            state = self.runner.inspect(&container)?;
            if let Some(state) = &state {
                self.refuse_unless_stoppable(name, &container, state, force)?;
            }
        }
        if state.is_some() {
            self.stop_and_remove_racing_a_start(name, &container, force)?;
        }
        self.status(name)
    }

    /// `docker stop` + `rm`, retried once when a reconcile `docker start`
    /// landed between the two (the `rm` then finds the container running).
    fn stop_and_remove_racing_a_start(
        &self,
        name: &str,
        container: &str,
        force: bool,
    ) -> Result<()> {
        let Err(error) = self.runner.stop_and_remove(container, STOP_GRACE) else {
            return Ok(());
        };
        match self.runner.inspect(container)? {
            None => Ok(()),
            Some(again) if again.running || again.restarting => {
                self.refuse_if_busy(name, container, &again, force)?;
                self.runner.stop_and_remove(container, STOP_GRACE)
            }
            Some(_) => Err(error),
        }
    }

    fn refuse_unless_stoppable(
        &self,
        name: &str,
        container: &str,
        state: &ContainerState,
        force: bool,
    ) -> Result<()> {
        Self::require_host_mode(state)?;
        self.refuse_if_busy(name, container, state, force)
    }

    fn refuse_if_busy(
        &self,
        name: &str,
        container: &str,
        state: &ContainerState,
        force: bool,
    ) -> Result<()> {
        if state.running && !force && self.runner.has_active_exec(container)? {
            anyhow::bail!(
                "session {name:?} has an in-flight `docker exec`; refusing to stop without \
                 --force (a hard stop here would SIGKILL active work, violating the #5119 \
                 restart-safety contract). Retry once the exec finishes, or pass --force to \
                 override."
            );
        }
        Ok(())
    }
}
