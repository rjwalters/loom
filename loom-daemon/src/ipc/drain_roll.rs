//! The pending-roll policy and its live status projection (Issue #6007,
//! extended by #8514).
//!
//! Two things live here:
//!
//! 1. **The widen-then-give-up policy** (`drain_pending_budget`,
//!    `drain_refusal_decision`, `drain_refusal_path` and their constants),
//!    moved out of `ipc.rs` unchanged. It is pure — no I/O, no state — so the
//!    whole matrix is unit-testable without driving a real supervisor to a real
//!    deadline.
//! 2. **The live projection** ([`DrainRollStatus`], [`roll_status`]) #8514 adds
//!    so `loom-daemon status` can answer "roll pending since T, dispatch paused
//!    for D, N in flight" at any moment, rather than only at the instant a
//!    refusal logs its one-shot note.
//!
//! It is a sibling module rather than more of `ipc.rs` because that file is
//! over `.loom/docs/file-size-policy.md`'s threshold and frozen, and because
//! the projection belongs next to the policy whose state it renders.

use super::DrainDescriptor;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Multiplier applied to the requested drain timeout to size the **total**
/// paused-dispatch budget a *retained* ("pending") roll may spend across all of
/// its automatic re-arms (Issue #6007).
///
/// Sizing the budget from the operator's own `--timeout` — rather than from a
/// flat constant — keeps a deliberately short drain short: `--timeout 60` buys a
/// 240s budget, not four hours.
pub const DRAIN_PENDING_BUDGET_MULTIPLIER: u64 = 4;

/// Absolute cap on the pending-roll budget, however large a `--timeout` was
/// requested (Issue #6007). A host must never stop taking work for longer than
/// this on account of a version roll.
pub const MAX_DRAIN_PENDING_BUDGET_SECS: u64 = 4 * 3600;

/// Cap on any single re-armed retry window (Issue #6007) — the windows widen
/// geometrically, and this stops the widening.
pub const MAX_DRAIN_RETRY_WINDOW_SECS: u64 = 2 * 3600;

/// A retry window shorter than this is not worth re-arming: the remaining budget
/// is spent, so the roll is abandoned instead (Issue #6007).
pub const MIN_DRAIN_RETRY_WINDOW_SECS: u64 = 60;

/// What a pending-roll deadline refusal decided to do (Issue #6007). Pure
/// counterpart of [`drain_refusal_decision`], so the widen-then-give-up policy
/// is unit-testable without driving a real supervisor to a real deadline.
#[derive(Debug, PartialEq, Eq)]
pub enum RefusalDecision {
    /// Retain the roll: keep dispatch paused and re-arm the deadline `window`
    /// from now.
    Defer { window: Duration },
    /// The paused-dispatch budget is spent — discard the roll intent and resume
    /// dispatch (the pre-#6007 terminal behavior).
    Abandon,
}

/// Which fail-safe path a [`super::DrainTick::TimedOutRefuse`] tick takes
/// (Issue #6007).
#[derive(Debug, PartialEq, Eq)]
pub enum RefusalPath {
    /// **Relaunch (roll) drains**: retain the intent — keep dispatch paused and
    /// re-arm the deadline ([`super::DrainState::refuse_roll_deadline`]).
    RetainRoll,
    /// **Then-exit (teardown) drains**: resume dispatch immediately and discard
    /// the intent — the pre-#6007 behavior, kept byte-for-byte because
    /// `fleet drain` orchestrates teardowns over SSH and detects a remote refusal
    /// by observing `drain.draining == false` on a still-reachable daemon.
    ResumeDispatch,
}

/// Pick the fail-safe path for a refused deadline (Issue #6007). Extracted as a
/// pure function so the roll-vs-teardown split is a test assertion rather than a
/// branch only reachable by driving a real supervisor to a real deadline.
#[must_use]
pub fn drain_refusal_path(then_exit: bool) -> RefusalPath {
    if then_exit {
        RefusalPath::ResumeDispatch
    } else {
        RefusalPath::RetainRoll
    }
}

/// Total paused-dispatch budget a retained ("pending") roll may spend, derived
/// from the operator's requested drain timeout (Issue #6007).
#[must_use]
pub fn drain_pending_budget(base: Duration) -> Duration {
    let scaled = base
        .as_secs()
        .saturating_mul(DRAIN_PENDING_BUDGET_MULTIPLIER);
    Duration::from_secs(scaled.min(MAX_DRAIN_PENDING_BUDGET_SECS))
}

/// Decide what a deadline refusal on a **relaunch (roll)** drain should do
/// (Issue #6007): re-arm a widened window, or give up because the total
/// paused-dispatch budget is spent.
///
/// The windows widen geometrically from the operator's own `--timeout`
/// (`base * 2^attempt`), each capped at [`MAX_DRAIN_RETRY_WINDOW_SECS`] and at
/// whatever budget remains — this is the operator's manual
/// "re-run with a larger `--timeout`" workaround, automated. When less than
/// [`MIN_DRAIN_RETRY_WINDOW_SECS`] of budget remains there is nothing useful
/// left to wait for, so the roll is abandoned and dispatch resumes rather than
/// starving the host of work indefinitely.
#[must_use]
pub fn drain_refusal_decision(
    base: Duration,
    refusals_so_far: u32,
    elapsed: Duration,
) -> RefusalDecision {
    let budget = drain_pending_budget(base);
    let remaining = budget.saturating_sub(elapsed).as_secs();
    if remaining < MIN_DRAIN_RETRY_WINDOW_SECS {
        return RefusalDecision::Abandon;
    }
    // `min(16)` only guards the shift; the widened value is capped immediately
    // below anyway.
    let widened = base
        .as_secs()
        .saturating_mul(1u64 << refusals_so_far.saturating_add(1).min(16));
    let window = widened
        .min(MAX_DRAIN_RETRY_WINDOW_SECS)
        .min(remaining)
        .max(MIN_DRAIN_RETRY_WINDOW_SECS);
    RefusalDecision::Defer {
        window: Duration::from_secs(window),
    }
}

/// The live, always-queryable view of an in-progress drain-and-restart roll
/// (Issue #8514).
///
/// Before #8514 the only evidence a host was sitting paused behind a roll was
/// [`DrainDescriptor::note`] — written once, at the instant of a refusal, and
/// overwritten by the next transition. An operator asking "is this host idle
/// because of a roll, and for how long?" had to catch that note live or read
/// the daemon log. These fields answer it from a single `loom-daemon status
/// --json` at any moment.
///
/// `None` on the wire (or from a pre-#8514 daemon) means "no drain is active";
/// every field describes the **current** drain only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrainRollStatus {
    /// `true` once the roll has survived at least one deadline refusal and is
    /// being *retained* (dispatch stays paused, the restart re-arms itself at
    /// quiescence). `false` for a first-attempt drain still inside its original
    /// deadline.
    pub roll_pending: bool,
    /// When the active drain began — the anchor for [`Self::paused_secs`].
    pub started_at: Option<DateTime<Utc>>,
    /// How long dispatch has been paused for this drain, in seconds. This is
    /// the number the original issue asked for ("dispatch paused for D").
    pub paused_secs: u64,
    /// The total paused-dispatch budget this roll may spend before it is
    /// abandoned and dispatch resumes ([`drain_pending_budget`]). `0` for a
    /// drain with no recorded base timeout.
    pub budget_secs: u64,
    /// How many deadline refusals this drain has already survived.
    pub refusals: u32,
    /// In-flight sweep count at the moment this status was built — what the
    /// roll is waiting to reach zero.
    pub in_flight: usize,
    /// The artifact identity this roll was triggered for (#8514's supersede
    /// key), or `None` for a drain not triggered by the auto-updater (an
    /// operator `restart --drain`, a `fleet drain` teardown).
    pub target: Option<String>,
    /// `true` when this drain's terminal action is "exit and stay down"
    /// (a `fleet drain` teardown) rather than "exit for a supervised
    /// relaunch" — a teardown is never superseded by a newer artifact.
    pub then_exit: bool,
}

/// Project the live drain state into [`DrainRollStatus`] (Issue #8514).
///
/// `None` when no drain is active: the historical `drain_note` already explains
/// a drain that ended, and reporting stale elapsed-pause numbers for a finished
/// drain would be worse than reporting nothing.
#[must_use]
pub fn roll_status(
    snap: &DrainDescriptor,
    in_flight: usize,
    now: DateTime<Utc>,
) -> Option<DrainRollStatus> {
    if !snap.active {
        return None;
    }
    Some(DrainRollStatus {
        roll_pending: snap.roll_pending,
        started_at: snap.started_at,
        paused_secs: paused_secs(snap.started_at, now),
        budget_secs: drain_pending_budget(snap.base_timeout).as_secs(),
        refusals: snap.refusals,
        in_flight,
        target: snap.roll_target.clone(),
        then_exit: snap.then_exit,
    })
}

/// Seconds of paused dispatch so far. Saturating: a `started_at` in the future
/// (a clock step) reads as `0` rather than panicking or wrapping.
#[must_use]
pub fn paused_secs(started_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> u64 {
    started_at.map_or(0, |started| {
        let secs = (now - started).num_seconds();
        u64::try_from(secs).unwrap_or(0)
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn descriptor() -> DrainDescriptor {
        DrainDescriptor {
            active: true,
            base_timeout: Duration::from_secs(1800),
            started_at: Some(Utc::now()),
            ..DrainDescriptor::default()
        }
    }

    // ---- the moved policy keeps behaving exactly as #6007 specified --------

    #[test]
    fn budget_is_the_multiplier_until_the_absolute_cap() {
        assert_eq!(drain_pending_budget(Duration::from_secs(60)), Duration::from_secs(240));
        assert_eq!(drain_pending_budget(Duration::from_secs(1800)), Duration::from_secs(7200));
        // Far past the cap: clamped, never scaled.
        assert_eq!(
            drain_pending_budget(Duration::from_secs(10 * 3600)),
            Duration::from_secs(MAX_DRAIN_PENDING_BUDGET_SECS)
        );
    }

    #[test]
    fn refusal_widens_then_abandons_when_the_budget_is_spent() {
        let base = Duration::from_secs(1800);
        assert_eq!(
            drain_refusal_decision(base, 0, Duration::from_secs(1800)),
            RefusalDecision::Defer {
                window: Duration::from_secs(3600)
            }
        );
        // Budget is 7200s; 7180s elapsed leaves less than the minimum window.
        assert_eq!(
            drain_refusal_decision(base, 3, Duration::from_secs(7180)),
            RefusalDecision::Abandon
        );
    }

    #[test]
    fn only_a_teardown_drain_resumes_dispatch_on_refusal() {
        assert_eq!(drain_refusal_path(false), RefusalPath::RetainRoll);
        assert_eq!(drain_refusal_path(true), RefusalPath::ResumeDispatch);
    }

    // ---- #8514's live projection ------------------------------------------

    #[test]
    fn no_active_drain_projects_to_none() {
        let snap = DrainDescriptor::default();
        assert!(roll_status(&snap, 3, Utc::now()).is_none());
    }

    #[test]
    fn an_active_roll_projects_live_paused_seconds_and_budget() {
        let now = Utc::now();
        let mut snap = descriptor();
        snap.started_at = Some(now - chrono::Duration::seconds(900));
        snap.roll_pending = true;
        snap.refusals = 2;
        snap.roll_target = Some("v0.19.24".to_string());

        let status = roll_status(&snap, 3, now).unwrap();
        assert!(status.roll_pending);
        assert_eq!(status.paused_secs, 900);
        assert_eq!(status.budget_secs, 7200);
        assert_eq!(status.refusals, 2);
        assert_eq!(status.in_flight, 3);
        assert_eq!(status.target.as_deref(), Some("v0.19.24"));
        assert!(!status.then_exit);
    }

    #[test]
    fn paused_seconds_grow_between_polls() {
        let start = Utc::now();
        let mut snap = descriptor();
        snap.started_at = Some(start);
        let first = roll_status(&snap, 1, start + chrono::Duration::seconds(30))
            .unwrap()
            .paused_secs;
        let second = roll_status(&snap, 1, start + chrono::Duration::seconds(90))
            .unwrap()
            .paused_secs;
        assert_eq!((first, second), (30, 90));
    }

    #[test]
    fn a_backwards_clock_step_reads_as_zero_not_a_wrapped_duration() {
        let now = Utc::now();
        assert_eq!(paused_secs(Some(now + chrono::Duration::seconds(60)), now), 0);
        assert_eq!(paused_secs(None, now), 0);
    }

    // ---- #8514's roll target, on the real DrainState -----------------------

    use super::super::DrainState;

    #[test]
    fn a_relaunch_roll_can_be_labelled_with_the_artifact_it_rolls_to() {
        let drain = DrainState::new();
        drain.begin(Duration::from_secs(1800), false, false);
        assert_eq!(drain.snapshot().roll_target, None, "unlabelled until told");
        drain.set_roll_target(Some("v0.19.30@bbbb".to_string()));
        assert_eq!(drain.snapshot().roll_target.as_deref(), Some("v0.19.30@bbbb"));
    }

    #[test]
    fn a_teardown_drain_is_never_labelled_as_a_supersedable_roll() {
        let drain = DrainState::new();
        // `then_exit` — `fleet drain`'s teardown.
        drain.begin(Duration::from_secs(1800), false, true);
        drain.set_roll_target(Some("v0.19.30@bbbb".to_string()));
        assert_eq!(drain.snapshot().roll_target, None);
    }

    #[test]
    fn labelling_when_no_drain_is_active_is_a_no_op() {
        let drain = DrainState::new();
        drain.set_roll_target(Some("v0.19.30@bbbb".to_string()));
        assert_eq!(drain.snapshot().roll_target, None);
    }

    #[test]
    fn aborting_clears_the_target_with_the_rest_of_the_pending_bookkeeping() {
        let drain = DrainState::new();
        drain.begin(Duration::from_secs(1800), false, false);
        drain.set_roll_target(Some("v0.19.30@bbbb".to_string()));
        assert!(drain.abort());
        let snap = drain.snapshot();
        assert_eq!(snap.roll_target, None);
        assert!(!snap.roll_pending);
        assert!(!snap.active);
        assert!(!drain.is_draining(), "dispatch resumes");
    }

    #[test]
    fn a_fresh_drain_never_inherits_the_previous_rolls_target() {
        let drain = DrainState::new();
        drain.begin(Duration::from_secs(1800), false, false);
        drain.set_roll_target(Some("v0.19.24@aaaa".to_string()));
        drain.abort();
        drain.begin(Duration::from_secs(1800), false, false);
        assert_eq!(drain.snapshot().roll_target, None);
    }

    #[test]
    fn an_abandoned_roll_clears_its_target_too() {
        let drain = DrainState::new();
        // A 60s base gives a 240s budget, so a refusal 5 minutes in abandons.
        drain.begin(Duration::from_secs(60), false, false);
        drain.set_roll_target(Some("v0.19.24@aaaa".to_string()));
        let refusal = drain.refuse_roll_deadline(Utc::now() + chrono::Duration::seconds(300));
        assert!(matches!(refusal, super::super::RollRefusal::Abandoned { .. }), "{refusal:?}");
        let snap = drain.snapshot();
        assert_eq!(snap.roll_target, None);
        assert!(!drain.is_draining(), "dispatch resumes on abandon");
    }

    #[test]
    fn a_deferred_roll_keeps_its_target_and_projects_as_pending() {
        let drain = DrainState::new();
        let started = Utc::now();
        drain.begin(Duration::from_secs(1800), false, false);
        drain.set_roll_target(Some("v0.19.24@aaaa".to_string()));
        let refusal = drain.refuse_roll_deadline(started + chrono::Duration::seconds(1800));
        assert!(
            matches!(refusal, super::super::RollRefusal::Deferred { attempt: 1, .. }),
            "{refusal:?}"
        );
        let snap = drain.snapshot();
        assert!(drain.is_draining(), "dispatch stays paused — the #6007 retention");
        let status = roll_status(&snap, 3, started + chrono::Duration::seconds(1800)).unwrap();
        assert!(status.roll_pending);
        assert_eq!(status.refusals, 1);
        assert_eq!(status.target.as_deref(), Some("v0.19.24@aaaa"));
        assert!(status.paused_secs >= 1799, "{}", status.paused_secs);
    }
}
