//! The pause roll's transitions on [`DrainState`] (#10831, design
//! `docs/design/daemon-roll-pause-resume.md` §7).
//!
//! A [`DrainOrigin::PauseRoll`] drain is supervised by
//! `crate::auto_update::pause_roll`, not by the wait-for-zero poll. That
//! supervisor reports which H4 step it is on and, before it stops any agent's
//! process tree, **commits** the pause. The commit is the boundary every
//! operator-interplay rule keys on:
//!
//! 1. Before the commit, an operator relaunch request promotes the drain to
//!    [`DrainOrigin::Operator`] (see [`DrainState::begin_as`]) and an
//!    `--abort-drain` is honoured. The pause supervisor sees that at its next
//!    step boundary ([`PauseOwnership::Promoted`] / [`PauseOwnership::Gone`])
//!    and stands down: withdraws its pause requests, deletes the manifest,
//!    stops nothing.
//! 2. After the commit, a relaunch request is acked with no promotion and an
//!    abort is refused ([`super::AbortOutcome::Refused`]): the roll completes.
//! 3. A then-exit request always escalates the terminal action; after the
//!    commit the pause completes and the daemon exits without relaunch.
//!
//! The commit is taken under the descriptor lock ([`DrainState::pause_commit_stop`]),
//! so an abort and a stop can never both win.

use super::{DrainBegin, DrainOrigin, DrainState, PauseRollStatus};
use std::time::Duration;

/// Whether the pause supervisor still owns the drain it started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseOwnership {
    /// Still this supervisor's pause roll.
    Ours,
    /// An operator request promoted it to an operator drain before the pause
    /// committed. The supervisor stands down and then supervises that drain.
    Promoted,
    /// Aborted, superseded, or otherwise replaced. The supervisor stands down
    /// and returns.
    Gone,
}

// Allow expect_used: same crash-on-poison policy as the rest of `DrainState`.
#[allow(clippy::expect_used)]
impl DrainState {
    /// Start a pause roll (H3 → H4): set the dispatch-pause flag with
    /// [`DrainOrigin::PauseRoll`], a deadline of `budget`, and `progress` as
    /// the initial pause status. An already-active drain is acked exactly as
    /// [`Self::begin_as`] acks it (an operator drain is never demoted).
    pub fn begin_pause_roll(&self, budget: Duration, progress: PauseRollStatus) -> DrainBegin {
        let begin = self.begin_as(budget, false, false, DrainOrigin::PauseRoll);
        if let DrainBegin::Started { generation, .. } = &begin {
            let mut inner = self.inner.lock().expect("Drain mutex poisoned");
            if self.generation() == *generation && inner.origin == DrainOrigin::PauseRoll {
                inner.pause = Some(progress);
            }
        }
        begin
    }

    /// Who owns the drain the pause supervisor of `generation` started.
    #[must_use]
    pub fn pause_ownership(&self, generation: u64) -> PauseOwnership {
        let inner = self.inner.lock().expect("Drain mutex poisoned");
        if self.generation() != generation || !inner.active {
            return PauseOwnership::Gone;
        }
        match inner.origin {
            DrainOrigin::PauseRoll => PauseOwnership::Ours,
            DrainOrigin::Operator => PauseOwnership::Promoted,
        }
    }

    /// Record that the pause is entering H4 step `step`, if it is still ours.
    /// Returns the ownership observed, so every step boundary is also an
    /// ownership check.
    pub fn pause_enter_step(&self, generation: u64, step: u8) -> PauseOwnership {
        self.pause_update(generation, |p| p.step = step)
    }

    /// Apply `update` to the pause status if the pause is still ours.
    pub fn pause_update(
        &self,
        generation: u64,
        update: impl FnOnce(&mut PauseRollStatus),
    ) -> PauseOwnership {
        let mut inner = self.inner.lock().expect("Drain mutex poisoned");
        if self.generation() != generation || !inner.active {
            return PauseOwnership::Gone;
        }
        if inner.origin != DrainOrigin::PauseRoll {
            return PauseOwnership::Promoted;
        }
        if let Some(p) = inner.pause.as_mut() {
            update(p);
        }
        PauseOwnership::Ours
    }

    /// Commit the pause before stopping an agent's process tree. Returns
    /// `false` (stop nothing) unless the pause is still ours. Once committed,
    /// an abort is refused and a relaunch request no longer promotes the drain.
    pub fn pause_commit_stop(&self, generation: u64) -> bool {
        self.pause_update(generation, |p| p.stopped = true) == PauseOwnership::Ours
    }

    /// End the pause roll of `generation` **if it has not committed**: clear
    /// the dispatch-pause flag, bump the generation (so its supervisor stands
    /// down) and record `note`. Returns `false`, changing nothing, when the
    /// pause has already stopped an agent, was promoted, or is gone. Decided
    /// under the descriptor lock, like [`Self::pause_commit_stop`], so "end it"
    /// and "stop an agent" can never both win (#10974: the H4 deadline and a
    /// failed H4 task use this instead of an unconditional clear).
    pub fn pause_abort_uncommitted(&self, generation: u64, note: String) -> bool {
        let mut inner = self.inner.lock().expect("Drain mutex poisoned");
        if self.generation() != generation
            || !inner.active
            || inner.origin != DrainOrigin::PauseRoll
            || inner.pause.as_ref().is_some_and(|p| p.stopped)
        {
            return false;
        }
        self.flag.store(false, std::sync::atomic::Ordering::Relaxed);
        self.generation
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        inner.active = false;
        inner.deadline = None;
        inner.roll_target = None;
        inner.pause = None;
        inner.note = Some(note);
        self.ledger_after(inner, chrono::Utc::now(), false);
        true
    }

    /// Whether a pause roll is armed, committed or in progress: an active
    /// [`DrainOrigin::PauseRoll`] drain, whether or not the H4 pause has
    /// stopped an agent yet. The restart it ends in is coming.
    ///
    /// `false` for an operator drain, a fleet-state `paused` hold (always
    /// [`DrainOrigin::Operator`]) and a pause roll an operator request
    /// promoted: those pause dispatch, but no roll is coming. This is what
    /// replaced #6007's retained-roll flag (`roll_pending`) for the workspace
    /// resync's host gate (#10718, merged with #10831 in #10974).
    #[must_use]
    pub fn pause_roll_in_progress(&self) -> bool {
        let inner = self.inner.lock().expect("Drain mutex poisoned");
        inner.active && inner.origin == DrainOrigin::PauseRoll
    }

    /// The active drain's terminal action: `true` ⇒ exit and stay down.
    #[must_use]
    pub fn then_exit(&self) -> bool {
        self.inner.lock().expect("Drain mutex poisoned").then_exit
    }
}
