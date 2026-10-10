//! The roll trigger: how the auto-update loop starts — and, since #8514,
//! supersedes — a roll.
//!
//! Since #10831 there is exactly one way to roll:
//! [`RollTrigger::trigger_pause_roll`]. Every roll source (`floor`,
//! `repo_ahead`, `config_restart`, `autoupdate`) passes its
//! [`RollTarget`] to it, and the production trigger starts the pause-and-roll
//! H4 pause ([`super::pause_roll::start_pause_roll`]). The wait-for-zero drain
//! trigger this replaced (`DrainTrigger` / `IpcDrainTrigger::trigger`, which
//! armed a drain that re-armed and then abandoned itself on a busy host) is
//! gone, along with its `abandon_roll`.
//!
//! The supersede *decision* is pure and lives in [`super::supersede`]; this
//! module only performs it.

use super::pause_roll::RollTarget;
use super::supersede::ArmedRoll;
use crate::event_bus::EventBus;
use crate::ipc::{DrainOrigin, DrainState};
use crate::workspace_pool::WorkspacePool;
use std::path::PathBuf;
use std::sync::Arc;

/// Starts a roll — separated from [`super::AutoUpdateProbe`] because in
/// production it needs a tokio runtime handle to spawn the pause supervisor.
pub trait RollTrigger: Send {
    /// Start the pause-and-roll for `target` (H3 → H4). Returns `true` when a
    /// restart is now coming: the pause started, or a drain already in
    /// progress will restart the daemon. `false` when the roll could not start
    /// (no supervisor, no state directory); nothing was paused.
    fn trigger_pause_roll(&self, target: &RollTarget) -> bool;

    /// Whether a roll or drain is **already** in progress. A tick that fires
    /// then has nothing useful to do: the fresh binary is provisioned and the
    /// restart is coming.
    ///
    /// Defaults to `false` so a caller with no drain state (tests, alternative
    /// triggers) behaves exactly as before.
    fn roll_in_progress(&self) -> bool {
        false
    }

    /// Describe the armed roll (Issue #8514): its artifact target, whether it
    /// has already stopped agents and whether it is a teardown. `None` ⇒
    /// "cannot describe it", which [`super::supersede::decide_armed_roll`]
    /// treats as never-supersede.
    fn armed_roll(&self) -> Option<ArmedRoll> {
        None
    }

    /// Discard the armed roll so this tick can replace it with one for `to`
    /// (Issue #8514).
    ///
    /// Returns `true` when a roll was actually discarded. Only a pause roll
    /// that has not stopped any agent yet can be: once the H4 pause commits it
    /// is too late to supersede (design §10).
    fn supersede_roll(&self, from: &str, to: &str) -> bool {
        let _ = (from, to);
        false
    }

    /// Why a roll cannot start on this host because nothing would relaunch
    /// the daemon (Issue #11042), or `None` when a supervisor is present.
    ///
    /// The tick reads this to stop re-fetching a target it already installed
    /// and could not roll to ([`super::not_rolling`]). Defaults to `None`
    /// (supervised) so test and alternative triggers behave as before.
    fn unsupervised(&self) -> Option<String> {
        None
    }
}

/// The production [`RollTrigger`]: starts the H4 pause inside a captured
/// runtime handle, so the supervisor it spawns resolves a runtime even when
/// invoked from the tick's blocking thread.
pub struct IpcRollTrigger {
    drain: Arc<DrainState>,
    workspace_pool: Arc<WorkspacePool>,
    fallback_root: PathBuf,
    event_bus: Arc<EventBus>,
    handle: tokio::runtime::Handle,
}

impl IpcRollTrigger {
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

impl RollTrigger for IpcRollTrigger {
    fn trigger_pause_roll(&self, target: &RollTarget) -> bool {
        // Enter the runtime so `start_pause_roll`'s `tokio::spawn` of the
        // pause supervisor resolves a runtime from a blocking thread.
        let _guard = self.handle.enter();
        super::pause_roll::start_pause_roll(
            &self.drain,
            &self.workspace_pool,
            &self.fallback_root,
            &self.event_bus,
            target,
        )
    }

    /// A roll or a supervised drain: something that ends in a restart.
    ///
    /// Not the shared "dispatch is held" flag (#10979). A fleet-state `paused`
    /// hold sets that flag too, with no supervisor behind it and no restart
    /// coming, so keying on the flag made a paused host skip every update tick
    /// forever while reporting a roll as armed. Such a hold is not a roll: the
    /// tick decides normally, and the roll it starts replaces the hold with a
    /// supervised drain ([`DrainState::begin_as`]), which restarts at once when
    /// nothing is in flight. The restarted daemon reads the store and holds
    /// again.
    fn roll_in_progress(&self) -> bool {
        let snap = self.drain.snapshot();
        snap.active && !snap.fleet_hold
    }

    fn armed_roll(&self) -> Option<ArmedRoll> {
        let snap = self.drain.snapshot();
        if !snap.active || snap.fleet_hold {
            return None;
        }
        Some(ArmedRoll {
            target: snap.roll_target,
            // An operator drain is never the auto-updater's to supersede; a
            // pause roll is, until it has stopped an agent.
            committed: snap.origin != DrainOrigin::PauseRoll
                || snap.pause.as_ref().is_some_and(|p| p.stopped),
            then_exit: snap.then_exit,
        })
    }

    /// The same check `start_pause_roll` refuses on (H7 `unsupervised`).
    fn unsupervised(&self) -> Option<String> {
        crate::ipc::detect_supervisor()
            .is_none()
            .then(|| "no supervisor is detected (LOOM_DAEMON_SUPERVISOR unset)".to_string())
    }

    fn supersede_roll(&self, from: &str, to: &str) -> bool {
        // `abort_pause_roll` refuses an operator drain, a then-exit, and a
        // pause that has already stopped an agent. It clears the pause flag and
        // bumps the generation, so the live pause supervisor stands down
        // (withdraws its pause requests, deletes its manifest) without exiting.
        let superseded = self.drain.abort_pause_roll();
        if superseded {
            self.drain
                .set_note(super::supersede::supersede_note(from, to));
            let _ = self.event_bus.publish_generic(
                "daemon.drain.superseded",
                serde_json::json!({ "from": from, "to": to }),
            );
        }
        superseded
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::ipc::{DrainBegin, PauseOwnership, PauseRollStatus};
    use std::time::Duration;

    fn trigger(drain: &Arc<DrainState>) -> IpcRollTrigger {
        let bus = Arc::new(EventBus::new());
        IpcRollTrigger::new(
            drain.clone(),
            Arc::new(WorkspacePool::new(bus.clone(), tokio::runtime::Handle::current())),
            PathBuf::from("/nonexistent/roll-trigger-test"),
            bus,
            tokio::runtime::Handle::current(),
        )
    }

    /// #10979: a fleet-state `paused` hold holds dispatch but is not a roll.
    /// With nothing in flight the host must still update: the tick is not
    /// skipped as "already armed", and the roll it starts replaces the hold
    /// with a supervised drain that restarts as soon as in-flight is zero.
    #[tokio::test]
    async fn a_fleet_state_pause_is_not_a_roll_in_progress() {
        let drain = Arc::new(DrainState::new());
        assert!(drain.hold_for_fleet_state("fleet state: paused".to_string()));
        assert!(drain.is_draining(), "the hold does pause dispatch");
        let t = trigger(&drain);
        assert!(!t.roll_in_progress(), "a hold with no restart coming is not a roll");
        assert_eq!(t.armed_roll(), None, "and no roll is armed to report or supersede");

        // What `trigger_pause_roll` does next on this state.
        let begun = drain.begin_pause_roll(Duration::from_secs(120), PauseRollStatus::default());
        let DrainBegin::Started { generation, .. } = begun else {
            panic!("the roll must start on a held host, got {begun:?}");
        };
        let snap = drain.snapshot();
        assert!(snap.active && !snap.fleet_hold && !snap.startup_hold, "{snap:?}");
        assert!(drain.is_draining(), "dispatch stays paused throughout");
        // It is an operator-origin drain (a failed roll must never release the
        // hold), so H4 stands down to the drain supervisor: nothing in flight
        // means an immediate restart.
        assert_eq!(drain.pause_ownership(generation), PauseOwnership::Promoted);
        assert!(t.roll_in_progress(), "now a restart really is coming");
    }

    /// The counterpart: a supervised drain and a pause roll still are.
    #[tokio::test]
    async fn a_supervised_drain_and_a_pause_roll_are_rolls_in_progress() {
        let drain = Arc::new(DrainState::new());
        let t = trigger(&drain);
        assert!(!t.roll_in_progress());
        let _ = drain.begin_pause_roll(Duration::from_secs(120), PauseRollStatus::default());
        assert!(t.roll_in_progress());
        assert!(t.armed_roll().is_some_and(|roll| !roll.committed));
        assert!(drain.abort_pause_roll());
        let _ = drain.begin(Duration::from_secs(60), false, false);
        assert!(t.roll_in_progress());
        assert!(t.armed_roll().is_some_and(|roll| roll.committed));
    }
}
