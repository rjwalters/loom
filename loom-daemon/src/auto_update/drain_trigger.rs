//! The roll trigger: how the auto-update loop arms — and, since #8514,
//! supersedes — a drain-and-restart.
//!
//! Moved verbatim out of `auto_update.rs` (over
//! `.loom/docs/file-size-policy.md`'s threshold, and frozen) and then extended
//! with the three methods #8514 needs: labelling a roll with the artifact it
//! was armed for, reading that label back, and discarding a pending roll that a
//! newer release has overtaken. The supersede *decision* is pure and lives in
//! [`super::supersede`]; this module only performs it.

use super::supersede::ArmedRoll;
use crate::event_bus::EventBus;
use crate::ipc::DrainState;
use crate::workspace_pool::WorkspacePool;
use std::path::PathBuf;
use std::sync::Arc;

/// Triggers the roll through #4090's drain path — separated from
/// [`super::AutoUpdateProbe`] because in production it needs a tokio runtime
/// handle to spawn the drain supervisor. Returns `true` when the drain was
/// accepted.
pub trait DrainTrigger: Send {
    fn trigger(&self) -> bool;

    /// Arm a roll and label it with the artifact identity it is rolling to
    /// (Issue #8514) — the key a later tick compares a freshly resolved release
    /// against to decide whether the armed roll has been overtaken.
    ///
    /// Defaults to plain [`Self::trigger`] (label dropped) so a trigger with no
    /// drain state — tests, alternative triggers — behaves exactly as before.
    fn trigger_for(&self, target: Option<&str>) -> bool {
        let _ = target;
        self.trigger()
    }

    /// Whether a drain-and-restart is **already** armed (Issue #6007).
    ///
    /// Since a refused roll deadline now *retains* its intent (dispatch stays
    /// paused and the restart re-arms itself when in-flight reaches zero), an
    /// auto-update tick that fires while that is pending has nothing useful to
    /// do: the fresh binary is already provisioned and the restart is already
    /// coming. Rebuilding again would burn CPU competing with the very in-flight
    /// sweeps the roll is waiting on — the #4929 nicing exists precisely because
    /// that competition is harmful.
    ///
    /// Defaults to `false` so a caller with no drain state (tests, alternative
    /// triggers) behaves exactly as before.
    fn roll_in_progress(&self) -> bool {
        false
    }

    /// Describe the armed roll (Issue #8514): its artifact target, whether it
    /// is already *pending* (has survived a deadline refusal) and whether it is
    /// a teardown. `None` ⇒ "cannot describe it", which
    /// [`super::supersede::decide_armed_roll`] treats as never-supersede.
    fn armed_roll(&self) -> Option<ArmedRoll> {
        None
    }

    /// Discard the armed roll so this tick can replace it with one for `to`
    /// (Issue #8514).
    ///
    /// Returns `true` when a roll was actually discarded. Implementations must
    /// go through the same primitive an operator `--abort-drain` uses, so the
    /// drain flag is cleared, the generation bumped (the live supervisor stands
    /// down without exiting the process) and no supervisor is ever left
    /// orphaned holding dispatch paused.
    fn supersede_roll(&self, from: &str, to: &str) -> bool {
        let _ = (from, to);
        false
    }

    /// Abandon the armed roll because its drain condition is **unsatisfiable**
    /// (Issue #8998): discard the roll intent, resume normal dispatch, and
    /// record `reason` as the drain note.
    ///
    /// Distinct from [`Self::supersede_roll`] in intent, not in mechanism — both
    /// go through the operator `--abort-drain` primitive. A supersede discards a
    /// roll because a *better* one is available and arms that one in its place;
    /// this discards a roll because no roll can complete on this host right now
    /// and arms nothing. Neither cancels a sweep.
    ///
    /// Returns `true` when a roll was actually abandoned. Defaults to `false`
    /// so a trigger with no drain state (tests, alternative triggers) behaves
    /// exactly as before.
    fn abandon_roll(&self, reason: &str) -> bool {
        let _ = reason;
        false
    }
}

/// The production [`DrainTrigger`]: calls [`crate::ipc::handle_drain_request`]
/// (the #4090 primitive) inside a captured runtime handle so the supervisor it
/// spawns resolves a runtime even when invoked from a blocking thread.
pub struct IpcDrainTrigger {
    drain: Arc<DrainState>,
    workspace_pool: Arc<WorkspacePool>,
    fallback_root: PathBuf,
    event_bus: Arc<EventBus>,
    handle: tokio::runtime::Handle,
}

impl IpcDrainTrigger {
    #[must_use]
    pub fn new(
        drain: Arc<DrainState>,
        workspace_pool: Arc<WorkspacePool>,
        fallback_root: PathBuf,
        event_bus: Arc<EventBus>,
        handle: tokio::runtime::Handle,
    ) -> Self {
        Self {
            drain,
            workspace_pool,
            fallback_root,
            event_bus,
            handle,
        }
    }
}

impl DrainTrigger for IpcDrainTrigger {
    fn trigger(&self) -> bool {
        // Enter the runtime so `handle_drain_request`'s internal `tokio::spawn`
        // of the drain supervisor resolves a runtime from a blocking thread.
        // `timeout_secs=None` uses the default drain deadline;
        // `force_after_timeout=false` is the fail-safe — if in-flight sweeps do
        // not drain by the deadline the roll is refused and dispatch resumes,
        // never killing a sweep.
        let _guard = self.handle.enter();
        // `then_exit=false`: this is the #4090 roll trigger — the daemon must
        // restart (relaunch into the freshly-rebuilt binary), never stop for
        // good. `then_exit: true` is `fleet drain`'s (#4343) teardown-only path.
        let resp = crate::ipc::handle_drain_request(
            &self.drain,
            &self.workspace_pool,
            &self.fallback_root,
            &self.event_bus,
            None,
            false,
            false,
        );
        // Issue #4521: the reply's `then_exit` reports the ACTIVE drain's
        // terminal action, not this request's. `true` here means an operator
        // teardown drain (`--drain --then-exit`) was already in flight, so this
        // roll piggybacks on a drain that will STOP the daemon rather than
        // relaunch it into the freshly-built binary. That is intentional
        // (then-exit is never downgraded — the host is being torn down), but it
        // must not be silent: the new binary will not be picked up until the
        // daemon is started again.
        if let crate::types::Response::DaemonDrain {
            accepted: true,
            then_exit: true,
            ..
        } = &resp
        {
            log::warn!(
                "auto-update roll joined an in-progress then-exit (teardown) drain: the daemon \
                 will STOP when drained and will NOT relaunch into the rebuilt binary. Start it \
                 again to pick up the update."
            );
        }
        matches!(resp, crate::types::Response::DaemonDrain { accepted: true, .. })
    }

    fn trigger_for(&self, target: Option<&str>) -> bool {
        let accepted = self.trigger();
        if accepted {
            // Label the drain this trigger just armed (or joined) with the
            // artifact it is rolling to (#8514). `set_roll_target` is a no-op on
            // a teardown drain, so joining an operator teardown never mislabels
            // it as a supersedable auto-update roll.
            self.drain.set_roll_target(target.map(str::to_string));
        }
        accepted
    }

    fn roll_in_progress(&self) -> bool {
        // `is_draining()` covers both an in-progress first-attempt drain and a
        // retained (pending) roll — in either case a restart is already armed.
        self.drain.is_draining()
    }

    fn armed_roll(&self) -> Option<ArmedRoll> {
        let snap = self.drain.snapshot();
        if !snap.active {
            return None;
        }
        Some(ArmedRoll {
            target: snap.roll_target,
            pending: snap.roll_pending,
            then_exit: snap.then_exit,
            refusals: snap.refusals,
        })
    }

    fn supersede_roll(&self, from: &str, to: &str) -> bool {
        // Deliberately the operator `--abort-drain` primitive: it clears the
        // pause flag, bumps the generation so the live supervisor stands down
        // (without exiting the process) and resets every piece of #6007's
        // pending-roll bookkeeping — `refusals`, `roll_pending`, `deadline`,
        // `roll_target`. Re-deriving that reset by hand is exactly the class of
        // mistake that could resurrect the pre-#6007 drain/work-finder
        // livelock, so this path does not.
        let superseded = self.drain.abort();
        if superseded {
            // `abort()`'s own note says "aborted by operator", which is not what
            // happened — overwrite it so `status` explains the real reason the
            // pause ended.
            self.drain
                .set_note(super::supersede::supersede_note(from, to));
            let _ = self.event_bus.publish_generic(
                "daemon.drain.superseded",
                serde_json::json!({ "from": from, "to": to }),
            );
        }
        superseded
    }

    fn abandon_roll(&self, reason: &str) -> bool {
        // Same primitive as `supersede_roll` above, for the same reason: it
        // clears the pause flag, bumps the generation so the live supervisor
        // stands down without exiting the process, and resets every piece of
        // #6007's pending-roll bookkeeping. Re-deriving that reset by hand is
        // how the pre-#6007 drain/work-finder livelock would come back.
        let abandoned = self.drain.abort();
        if abandoned {
            // `abort()`'s own note says "aborted by operator", which is not what
            // happened — the daemon gave up on its own roll. Overwrite it so
            // `status --json`'s `drain_note` names the real reason dispatch
            // resumed, alongside `auto_update_note`.
            self.drain.set_note(reason.to_string());
            let _ = self.event_bus.publish_generic(
                "daemon.drain.roll_unsatisfiable",
                serde_json::json!({ "reason": reason }),
            );
        }
        abandoned
    }
}
