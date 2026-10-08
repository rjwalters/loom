//! The H5 resume hold on [`DrainState`] (#10832, design
//! `docs/design/daemon-roll-pause-resume.md` §7 "H5 Verifying").
//!
//! A daemon that starts and finds a live pause manifest must not dispatch new
//! work until it has passed health probation and resumed (or requeued) every
//! paused agent. This is the same dispatch-pause flag a drain sets, held with
//! [`DrainOrigin::PauseRoll`] and no supervisor: H5 places it when it starts
//! and releases it when the manifest is finished.
//!
//! - It is **not** a drain: nothing waits for in-flight work and nothing exits.
//! - A real drain request **replaces** it (as it replaces a startup hold). H5
//!   then sees the hold gone ([`DrainState::is_roll_resume_held`]) and stops
//!   relaunching; the manifest stays for the next start.
//! - `--abort-drain` is **refused** while it is held: releasing dispatch in the
//!   middle of H5 would let the work finder run beside a half-resumed host,
//!   and the hold ends by itself within the probation and resume budgets.
//! - It never displaces a hold or drain already in force (an operator stop, a
//!   fleet `paused` state, an operator drain). Dispatch is paused either way,
//!   and H5 waits for that hold to end before it resumes anything.

use super::{DrainOrigin, DrainState};
use chrono::Utc;
use std::sync::atomic::Ordering;

/// Why an `--abort-drain` is refused during H5.
pub(super) const ABORT_REFUSED: &str =
    "refusing --abort-drain: dispatch is held while this daemon verifies its health and resumes \
     the agents a version roll paused (#10832, design §7 H5). The hold ends by itself once every \
     paused agent is resumed or requeued, within the verify-probation and resume budgets. To \
     stop the host instead, use `loom-daemon restart --drain --then-exit`.";

/// Who is holding dispatch for H5 ([`DrainState::roll_resume_hold`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeHold {
    /// H5's own hold (placed by this call if nothing held dispatch).
    Ours,
    /// The fleet store says this host is `paused` (#9598). Dispatch is held
    /// and stays held after H5: H5 proceeds under it and releases nothing.
    /// Finishing the paused agents is in-flight work, which a fleet pause
    /// lets finish.
    Fleet,
    /// An operator stop or a real drain holds dispatch. H5 resumes nothing
    /// while it stands.
    Blocked,
}

// Allow expect_used: same crash-on-poison policy as the rest of `DrainState`.
#[allow(clippy::expect_used)]
impl DrainState {
    /// Hold dispatch for H5. Returns `true` when this call placed the hold (or
    /// it was already in place). `false` when another hold or drain is active:
    /// dispatch is already paused, and that hold's own release path owns it.
    pub fn hold_for_roll_resume(&self, note: String) -> bool {
        let mut inner = self.inner.lock().expect("Drain mutex poisoned");
        if inner.active {
            return inner.resume_hold;
        }
        let at = Utc::now();
        inner.active = true;
        inner.resume_hold = true;
        inner.origin = DrainOrigin::PauseRoll;
        inner.started_at = Some(at);
        inner.deadline = None;
        inner.then_exit = false;
        inner.force_after_timeout = false;
        inner.timed_out = false;
        inner.roll_target = None;
        inner.pause = None;
        inner.note = Some(note);
        self.flag.store(true, Ordering::Relaxed);
        self.ledger_after(inner, at, true);
        true
    }

    /// Make sure dispatch is held for H5 and say by whom. Places H5's own
    /// hold when nothing holds dispatch; never displaces another hold.
    pub fn roll_resume_hold(&self, note: String) -> ResumeHold {
        if self.hold_for_roll_resume(note) {
            return ResumeHold::Ours;
        }
        if self.is_fleet_held() {
            ResumeHold::Fleet
        } else {
            ResumeHold::Blocked
        }
    }

    /// Release the H5 hold and resume dispatch. Returns `true` when the hold
    /// was in place; `false` when something else replaced it meanwhile (that
    /// drain keeps dispatch paused and is left alone).
    pub fn release_roll_resume_hold(&self, note: String) -> bool {
        let mut inner = self.inner.lock().expect("Drain mutex poisoned");
        if !inner.resume_hold {
            return false;
        }
        self.flag.store(false, Ordering::Relaxed);
        self.generation.fetch_add(1, Ordering::Relaxed);
        inner.active = false;
        inner.resume_hold = false;
        inner.deadline = None;
        inner.note = Some(note);
        self.ledger_after(inner, Utc::now(), false);
        true
    }

    /// Whether the H5 hold is in place.
    #[must_use]
    pub fn is_roll_resume_held(&self) -> bool {
        self.inner.lock().expect("Drain mutex poisoned").resume_hold
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::{AbortOutcome, DrainBegin};
    use super::*;
    use std::time::Duration;

    #[test]
    fn the_hold_pauses_dispatch_until_it_is_released() {
        let drain = DrainState::new();
        assert!(drain.hold_for_roll_resume("resuming".to_string()));
        assert!(drain.is_draining() && drain.is_roll_resume_held());
        let snap = drain.snapshot();
        assert_eq!(snap.origin, DrainOrigin::PauseRoll);
        assert!(!snap.startup_hold, "not an operator stop");
        // Idempotent.
        assert!(drain.hold_for_roll_resume("again".to_string()));
        assert!(drain.release_roll_resume_hold("done".to_string()));
        assert!(!drain.is_draining() && !drain.is_roll_resume_held());
        assert_eq!(drain.snapshot().note.as_deref(), Some("done"));
        assert!(!drain.release_roll_resume_hold("twice".to_string()));
    }

    #[test]
    fn an_abort_is_refused_while_the_hold_is_in_place() {
        let drain = DrainState::new();
        drain.hold_for_roll_resume("resuming".to_string());
        let AbortOutcome::Refused(why) = drain.abort_checked() else {
            panic!("the abort must be refused");
        };
        assert!(why.contains("H5"), "{why}");
        assert!(drain.is_draining(), "dispatch stays held");
    }

    /// The auto-updater's supersede path ends a roll it armed. The resume
    /// hold is not one, and must survive it.
    #[test]
    fn the_auto_updater_cannot_end_the_hold_as_if_it_were_an_armed_roll() {
        let drain = DrainState::new();
        drain.hold_for_roll_resume("resuming".to_string());
        assert!(!drain.abort_pause_roll());
        assert!(drain.is_draining() && drain.is_roll_resume_held());
    }

    #[test]
    fn a_real_drain_replaces_the_hold_and_the_release_then_leaves_it_alone() {
        let drain = DrainState::new();
        drain.hold_for_roll_resume("resuming".to_string());
        let begin = drain.begin(Duration::from_secs(60), false, false);
        assert!(matches!(begin, DrainBegin::Started { .. }), "replaced, not acked");
        assert!(!drain.is_roll_resume_held());
        assert!(!drain.release_roll_resume_hold("done".to_string()));
        assert!(drain.is_draining(), "the operator drain still pauses dispatch");
        assert_eq!(drain.snapshot().origin, DrainOrigin::Operator);
    }

    /// #10979: a host the fleet store says is `paused` must not dispatch
    /// after H5. A fleet pause in force at startup is H5's hold; one that
    /// arrives while H5 holds dispatch takes the hold over; either way the
    /// release at the end of H5 leaves dispatch paused.
    #[test]
    fn a_fleet_pause_outlives_the_resume_hold() {
        // In force before H5 starts.
        let drain = DrainState::new();
        drain.hold_for_fleet_state("fleet says paused".to_string());
        assert_eq!(drain.roll_resume_hold("resuming".to_string()), ResumeHold::Fleet);
        assert!(!drain.release_roll_resume_hold("done".to_string()));
        assert!(drain.is_draining() && drain.is_fleet_held());

        // Arriving while H5 holds dispatch.
        let drain = DrainState::new();
        assert_eq!(drain.roll_resume_hold("resuming".to_string()), ResumeHold::Ours);
        assert!(drain.hold_for_fleet_state("fleet says paused".to_string()));
        assert!(drain.is_fleet_held() && !drain.is_roll_resume_held());
        assert_eq!(drain.roll_resume_hold("resuming".to_string()), ResumeHold::Fleet);
        assert!(!drain.release_roll_resume_hold("done".to_string()));
        assert!(drain.is_draining(), "dispatch stays paused after H5");
        // The store's own release still works, and H5 then re-holds.
        assert!(drain.release_fleet_hold());
        assert_eq!(drain.roll_resume_hold("resuming".to_string()), ResumeHold::Ours);

        // An operator stop is not a fleet pause: H5 waits.
        let drain = DrainState::new();
        let _ = drain.begin(Duration::from_secs(60), false, true);
        assert_eq!(drain.roll_resume_hold("resuming".to_string()), ResumeHold::Blocked);
    }

    #[test]
    fn it_never_displaces_a_hold_already_in_force() {
        let drain = DrainState::new();
        assert!(drain.hold_for_fleet_state("fleet says paused".to_string()));
        assert!(!drain.hold_for_roll_resume("resuming".to_string()));
        assert!(drain.is_fleet_held() && !drain.is_roll_resume_held());
        assert_eq!(drain.snapshot().note.as_deref(), Some("fleet says paused"));
    }
}
