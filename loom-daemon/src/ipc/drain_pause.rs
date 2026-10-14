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

    /// Start a host-maintenance pause (#11812, `restart --pause --then-exit`):
    /// H4 exactly as a roll runs it, ending in exit-and-stay-down.
    ///
    /// Decided under the descriptor lock, and **the dispatch-pause flag is
    /// never cleared on any branch**, so a host whose dispatch is already held
    /// is never open for a moment:
    ///
    /// - nothing active: a fresh [`DrainOrigin::PauseRoll`] drain with
    ///   `then_exit`;
    /// - an operator drain (in progress, or timed out and held), a #9588
    ///   startup hold or a #9598 fleet `paused` hold: **taken over in place**.
    ///   The drain becomes a then-exit pause roll and the generation is bumped,
    ///   so the wait-for-zero drain supervisor (if any) stands down without
    ///   exiting. Then-exit is one-way, as #4521's escalation is;
    /// - a pause roll already running H4: its terminal action is escalated to
    ///   stay-down (rule 3 above) and it carries on as the roll it is, with no
    ///   second H4. The auto-updater can no longer supersede it
    ///   ([`Self::abort_pause_roll`] refuses a then-exit);
    /// - an H5 resume hold: refused. H5 is finishing a previous pause and ends
    ///   by itself within its probation and resume budgets.
    ///
    /// Every accepted branch records the operator stop (#9588), so neither the
    /// watchdog nor a supervisor relaunch brings the daemon back mid-change.
    pub fn begin_host_maintenance(
        &self,
        budget: Duration,
        progress: PauseRollStatus,
    ) -> MaintenanceBegin {
        let mut inner = self.inner.lock().expect("Drain mutex poisoned");
        let now = chrono::Utc::now();
        let begun = if inner.active && inner.resume_hold {
            return MaintenanceBegin::Refused(
                "refusing --pause --then-exit: this daemon is still resuming the agents a                  previous pause stopped (H5, #10832). The hold ends by itself within the                  verify-probation and resume budgets; retry once `loom-daemon status` no longer                  shows it."
                    .to_string(),
            );
        } else if inner.active && inner.origin == DrainOrigin::PauseRoll {
            inner.then_exit = true;
            inner.roll_target = None;
            inner.note = Some(
                "host maintenance (#11812): the pause roll in progress was escalated to                  then-exit — it completes H4 and the daemon stays down"
                    .to_string(),
            );
            MaintenanceBegin::Escalated {
                generation: self.generation(),
                manifest_id: inner.pause.as_ref().and_then(|p| p.manifest_id.clone()),
            }
        } else {
            let took_over = inner.active.then(|| {
                if inner.fleet_hold {
                    TookOver::FleetHold
                } else if inner.startup_hold {
                    TookOver::StartupHold
                } else if inner.timed_out {
                    TookOver::TimedOutDrain {
                        then_exit: inner.then_exit,
                    }
                } else {
                    TookOver::Drain {
                        then_exit: inner.then_exit,
                    }
                }
            });
            let fresh = !inner.active;
            inner.active = true;
            inner.origin = DrainOrigin::PauseRoll;
            inner.then_exit = true;
            inner.force_after_timeout = false;
            inner.timed_out = false;
            inner.startup_hold = false;
            inner.fleet_hold = false;
            inner.deadline = Some(now + chrono::Duration::from_std(budget).unwrap_or_default());
            inner.roll_target = None;
            inner.pause = Some(progress);
            inner.note = Some(match took_over {
                Some(t) => format!(
                    "host maintenance (#11812): took over {} — H4 pauses every agent, then the                      daemon stays down",
                    t.as_str()
                ),
                None => "host maintenance (#11812): H4 pauses every agent, then the daemon stays                          down"
                    .to_string(),
            });
            if fresh {
                inner.started_at = Some(now);
            }
            // Set while holding the descriptor lock (it is already set when
            // something was active: this store is a no-op then).
            self.flag.store(true, std::sync::atomic::Ordering::Relaxed);
            let generation = self
                .generation
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                + 1;
            let begun = MaintenanceBegin::Started {
                generation,
                took_over,
            };
            if fresh {
                self.ledger_after(inner, now, true);
            } else {
                drop(inner);
            }
            self.record_operator_stop("restart --pause --then-exit (host maintenance)");
            return begun;
        };
        drop(inner);
        self.record_operator_stop("restart --pause --then-exit (host maintenance)");
        begun
    }

    /// A then-exit pause roll that failed **before it stopped anything** (the
    /// manifest could not be written, or H4 hung or died uncommitted) falls
    /// back to an operator then-exit drain, held as if timed out, instead of
    /// resuming dispatch (#11812). Host maintenance never reopens dispatch on
    /// its own: the drain supervisor the caller spawns for `generation` stops
    /// the daemon once nothing is in flight, and `--abort-drain` is the
    /// operator's way back. Returns `false`, changing nothing, for a relaunch
    /// roll, a committed pause, or a pause that is no longer this generation's.
    pub fn pause_fallback_to_operator_drain(&self, generation: u64, note: String) -> bool {
        let mut inner = self.inner.lock().expect("Drain mutex poisoned");
        if self.generation() != generation
            || !inner.active
            || !inner.then_exit
            || inner.origin != DrainOrigin::PauseRoll
            || inner.pause.as_ref().is_some_and(|p| p.stopped)
        {
            return false;
        }
        inner.origin = DrainOrigin::Operator;
        inner.pause = None;
        inner.roll_target = None;
        inner.timed_out = true;
        inner.deadline = None;
        inner.note = Some(format!(
            "{note} Fell back to a then-exit drain: dispatch stays PAUSED and the daemon stops              once nothing is in flight (#11812)."
        ));
        true
    }
}

/// What a host-maintenance pause took over (#11812).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TookOver {
    /// An operator `restart --drain` still waiting for in-flight work.
    Drain { then_exit: bool },
    /// An operator drain past its deadline, holding dispatch paused (#9588).
    TimedOutDrain { then_exit: bool },
    /// The hold a relaunch places while an operator stop is on record (#9588).
    StartupHold,
    /// The hold the fleet store's `paused` state places (#9598).
    FleetHold,
}

impl TookOver {
    /// Operator-facing wording.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Drain { then_exit: true } => "an operator then-exit drain (in progress)",
            Self::Drain { then_exit: false } => "an operator relaunch drain (in progress)",
            Self::TimedOutDrain { then_exit: true } => {
                "an operator then-exit drain (timed out, dispatch held)"
            }
            Self::TimedOutDrain { then_exit: false } => {
                "an operator relaunch drain (timed out, dispatch held)"
            }
            Self::StartupHold => "the operator-stop startup hold",
            Self::FleetHold => "the fleet-state `paused` hold",
        }
    }
}

/// Outcome of [`DrainState::begin_host_maintenance`] (#11812).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MaintenanceBegin {
    /// A then-exit pause roll of `generation` now owns dispatch; the caller
    /// spawns its H4 supervisor. `took_over` names what held dispatch before.
    Started {
        generation: u64,
        took_over: Option<TookOver>,
    },
    /// A pause roll was already running H4; it now stays down at the end.
    Escalated {
        generation: u64,
        manifest_id: Option<String>,
    },
    /// Nothing changed; the operator-facing reason.
    Refused(String),
}
