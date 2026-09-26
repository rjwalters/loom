//! Unsatisfiable-drain detection for the auto-update roll (Issue #8998).
//!
//! # The livelock this closes
//!
//! #6007 made a refused drain deadline *retain* the roll intent instead of
//! handing the admission window back to the work finder, and bounded that
//! retention with a total paused-dispatch budget
//! ([`crate::ipc::drain_roll::drain_pending_budget`]) so a wedged sweep could
//! not starve the host of work forever. Both halves are right. Neither is
//! enough, because the budget bounds **one drain**, and the thing that
//! livelocked was the *sequence* of drains:
//!
//! 1. `auto_update` arms a roll; dispatch pauses.
//! 2. The deadline expires with sweeps still in flight. The fail-safe correctly
//!    refuses to cancel them and re-arms a widened window.
//! 3. The budget is eventually spent, the roll is abandoned, dispatch resumes.
//! 4. A new release lands (the fleet cuts one every ~30 min) — or #8514
//!    supersedes the pending roll onto it, which *restarts the budget clock* —
//!    and step 1 happens again.
//!
//! Every step is individually correct and the aggregate never terminates. On
//! 2026-09-25 two fleet dispatchers spent **21 hours** in that cycle (72 and 65
//! consecutive `"a drain-and-restart roll is already armed … skipping this
//! tick"` ticks) and never once rolled. The blocker was a genuinely-working
//! 9h32m analog-simulation sweep, so "wait for in-flight to reach zero" was
//! structurally unachievable on that host — and because `draining: true`
//! suppresses **role spawns** as well as sweep dispatch, one host produced zero
//! role ticks for 4h20m.
//!
//! # What this module adds
//!
//! An **episode**: one continuous run of "a roll is armed (or was, and is about
//! to be re-armed) and the in-flight set is not emptying". It counts drain
//! deadline expiries *across* roll lifetimes — the boundary the livelock hid
//! behind, since [`crate::ipc::DrainDescriptor::refusals`] restarts at `0` on
//! every fresh drain — and once `threshold` deadlines have expired without the
//! in-flight count ever improving, declares the wait condition **unsatisfiable**.
//!
//! The declaration is sticky: it clears only on an observation of `in_flight ==
//! 0`, i.e. on proof that the condition the roll waits for is reachable after
//! all. Anything weaker (a decrease from 7 to 2 while a 9-hour sweep keeps
//! running) would re-arm the roll for another budget with the same outcome,
//! which is the cycling being stopped.
//!
//! # What it deliberately does not change
//!
//! - **The #6007 fail-safe.** No sweep is ever cancelled. Abandoning a roll
//!   resumes dispatch and leaves the pre-update binary running, exactly as the
//!   budget-exhaustion path already did.
//! - **Anyone else's drain.** Only a roll this daemon's auto-updater armed and
//!   labelled with an artifact target advances an episode or is abandoned — the
//!   same conservatism [`super::supersede`] applies. An operator `restart
//!   --drain` and a `fleet drain` teardown (`then_exit`) are untouched.
//! - **Directions 2 and 3 of #8998** (age-excluding a long sweep from the drain
//!   condition; dropping the full-drain requirement for artifact rolls). Both
//!   change safety-relevant semantics and are the work that would let such a
//!   host actually update; this module only converts a silent indefinite
//!   livelock into one loud, actionable, self-clearing state.

use super::supersede::ArmedRoll;
use super::AutoUpdateConfig;
use chrono::{DateTime, Utc};

/// Env override for the unsatisfiability threshold (Issue #8998).
pub const AUTO_UPDATE_ROLL_STALL_DEADLINES_ENV: &str = "LOOM_AUTO_UPDATE_ROLL_STALL_DEADLINES";

/// Default threshold: how many drain deadlines may expire — across roll
/// lifetimes — with the in-flight count never improving before the roll's wait
/// condition is declared unsatisfiable (Issue #8998).
///
/// `3` is chosen against #6007's own geometry rather than picked round. With the
/// default 1800s drain timeout the retry windows widen `1800 → 3600 → …` under a
/// 7200s total budget, so a *single* roll reaches roughly this many refusals
/// before it abandons itself anyway. A lower value would fire inside the first
/// roll's own fail-safe (pre-empting a wait that is still working); a much
/// higher one is what the fleet already had, since each new release reset the
/// count to zero.
pub const DEFAULT_ROLL_STALL_DEADLINES: u32 = 3;

/// Resolve the threshold with precedence **env > config > default** (Issue
/// #8998), matching every other `autonomous.autoUpdate.*` knob. A zero or
/// unparseable env value falls through rather than disabling the detector: `0`
/// would declare *every* armed roll unsatisfiable on its first observation.
#[must_use]
pub fn resolve_roll_stall_deadlines(config: &AutoUpdateConfig) -> u32 {
    std::env::var(AUTO_UPDATE_ROLL_STALL_DEADLINES_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|&n| n > 0)
        // Filtered on this side too, not only at `read_auto_update_config` time:
        // a directly-constructed config must not be able to smuggle a `0` past
        // the resolver either.
        .or(config.roll_stall_deadlines.filter(|&n| n > 0))
        .unwrap_or(DEFAULT_ROLL_STALL_DEADLINES)
}

/// The operator-facing finding: this host's roll cannot complete, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollStallReport {
    /// Drain deadlines that have expired during this episode, summed across
    /// every roll lifetime in it.
    pub deadlines: u32,
    /// The in-flight sweep count at the observation that produced this report.
    pub in_flight: usize,
    /// The lowest in-flight count seen anywhere in the episode — the number that
    /// makes "it is not emptying" concrete rather than asserted.
    pub floor: usize,
    /// How long the episode has run, in seconds.
    pub armed_secs: u64,
    /// The artifact identity the abandoned roll was targeting, when known.
    pub target: Option<String>,
}

impl RollStallReport {
    /// The WARN line and `status` note. Loud, and every sentence is an action or
    /// a fact an operator needs to choose between them.
    #[must_use]
    pub fn note(&self) -> String {
        let Self {
            deadlines,
            in_flight,
            floor,
            armed_secs,
            target,
        } = self;
        let target = target
            .as_deref()
            .map_or_else(String::new, |t| format!(" (target {t})"));
        format!(
            "ABANDONING the drain-and-restart roll{target}: its wait condition is UNSATISFIABLE. \
             {deadlines} drain deadline(s) have expired across {armed_secs}s of armed roll and the \
             in-flight sweep count has never improved on {floor} ({in_flight} in flight now) — \
             re-arming would pause dispatch for another budget and reach the same refusal, which \
             is the loop that cost two fleet hosts 21h of paused dispatch (#8998). The roll intent \
             is DISCARDED and NORMAL DISPATCH RESUMES, including role spawns. No sweep was \
             cancelled and the pre-update binary keeps running (the #6007 fail-safe is unchanged). \
             THIS HOST WILL NOT AUTO-UPDATE until in-flight reaches zero, at which point the roll \
             re-arms and completes on its own. To act now: find the long-running sweep with \
             `loom-daemon list`, then either let it finish, cancel it with `loom-daemon cancel \
             --sweep <id>`, or force the roll through with `loom-daemon restart --drain \
             --force-after-timeout` (which DOES cancel it)."
        )
    }
}

/// One episode of drain-deadline accounting that survives roll re-arms
/// (Issue #8998). Pure — no I/O, no clock of its own — so the whole
/// widen/re-arm/abandon sequence is a unit test rather than a 21-hour
/// reproduction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RollStallTracker {
    /// How many non-improving deadlines end the episode.
    threshold: u32,
    /// Deadline expiries inherited from rolls that have already ended
    /// (budget-abandoned, superseded, or replaced by a newer release).
    carried_deadlines: u32,
    /// The live roll's `refusals` as of the previous observation. A *decrease*
    /// is how a new roll replacing an old one is detected between two ticks.
    live_refusals: Option<u32>,
    /// The lowest in-flight count observed in this episode.
    floor: Option<usize>,
    /// The episode's deadline total when `floor` last improved — the rebase
    /// point, so a host that is genuinely draining keeps earning fresh patience.
    deadlines_at_floor: u32,
    /// When the episode's first observation was made.
    since: Option<DateTime<Utc>>,
    /// The most recent roll target seen, for the report.
    target: Option<String>,
    /// Sticky once set: cleared only by an `in_flight == 0` observation.
    unsatisfiable: bool,
}

impl Default for RollStallTracker {
    fn default() -> Self {
        Self {
            threshold: DEFAULT_ROLL_STALL_DEADLINES,
            carried_deadlines: 0,
            live_refusals: None,
            floor: None,
            deadlines_at_floor: 0,
            since: None,
            target: None,
            unsatisfiable: false,
        }
    }
}

impl RollStallTracker {
    /// Override the threshold (the resolved `rollStallDeadlines` knob).
    pub(super) fn set_threshold(&mut self, threshold: u32) {
        self.threshold = threshold.max(1);
    }

    /// Whether an episode is running — i.e. whether a tick with no armed roll
    /// still needs to read the in-flight count. `true` while a declaration is
    /// standing, which is what makes the suppression self-clearing.
    pub(super) fn is_active(&self) -> bool {
        self.since.is_some()
    }

    /// Fold one tick's observation in.
    ///
    /// `armed` is the live drain's identity (`None` when no drain is armed);
    /// `in_flight` is the cross-root non-terminal sweep count. Returns `Some`
    /// once the wait condition is unsatisfiable — on that tick and every
    /// subsequent one until the host is observed idle, so the state is
    /// re-reported rather than logged once and forgotten.
    pub(super) fn observe(
        &mut self,
        now: DateTime<Utc>,
        armed: Option<&ArmedRoll>,
        in_flight: usize,
    ) -> Option<RollStallReport> {
        // The condition the roll waits for is satisfied right now: whatever the
        // episode believed, it is over. This is the only way a declaration
        // clears, and the reason the suppression cannot outlive the blockage.
        if in_flight == 0 {
            *self = Self {
                threshold: self.threshold,
                ..Self::default()
            };
            return None;
        }
        if self.unsatisfiable {
            return Some(self.report(now, in_flight));
        }
        match armed {
            // Not ours to reason about: a `fleet drain` teardown, or an
            // untargeted drain (an operator `restart --drain`, or a source-path
            // roll this daemon cannot key an artifact identity on). Freeze the
            // episode rather than counting someone else's deadlines.
            Some(roll) if roll.then_exit || roll.target.is_none() => return None,
            Some(roll) => {
                // `refusals` is monotonic *within* a drain and restarts at 0 on
                // the next one, so a decrease means a roll ended and another
                // took its place: bank what the old one accumulated.
                if let Some(previous) = self.live_refusals {
                    if roll.refusals < previous {
                        self.carried_deadlines = self.carried_deadlines.saturating_add(previous);
                    }
                }
                self.live_refusals = Some(roll.refusals);
                self.target = roll.target.clone();
            }
            // The roll ended between ticks (budget-abandoned, superseded, or
            // aborted) and nothing is armed yet. The episode continues — this
            // gap is precisely where the 21h cycle reset itself.
            None => {
                if let Some(previous) = self.live_refusals.take() {
                    self.carried_deadlines = self.carried_deadlines.saturating_add(previous);
                }
                // Nothing has been armed in this episode yet, so there is no
                // episode: a busy host with no roll armed is not stalled.
                self.since?;
            }
        }
        if self.since.is_none() {
            self.since = Some(now);
        }
        let deadlines = self.deadlines();
        match self.floor {
            // The episode's very first reading establishes the floor without
            // consuming patience: `deadlines_at_floor` stays `0`, so the
            // threshold is measured against the episode's whole deadline count
            // rather than against whatever the count happened to be when this
            // tracker first looked.
            None => self.floor = Some(in_flight),
            Some(floor) if in_flight < floor => {
                self.floor = Some(in_flight);
                self.deadlines_at_floor = deadlines;
            }
            Some(_) => {}
        }
        if deadlines.saturating_sub(self.deadlines_at_floor) >= self.threshold {
            self.unsatisfiable = true;
            return Some(self.report(now, in_flight));
        }
        None
    }

    /// Deadlines expired in this episode: banked from ended rolls plus the live
    /// roll's own refusals.
    fn deadlines(&self) -> u32 {
        self.carried_deadlines
            .saturating_add(self.live_refusals.unwrap_or(0))
    }

    fn report(&self, now: DateTime<Utc>, in_flight: usize) -> RollStallReport {
        RollStallReport {
            deadlines: self.deadlines(),
            in_flight,
            floor: self.floor.unwrap_or(in_flight),
            armed_secs: self
                .since
                .map_or(0, |since| u64::try_from((now - since).num_seconds()).unwrap_or(0)),
            target: self.target.clone(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn roll(refusals: u32) -> ArmedRoll {
        ArmedRoll {
            target: Some("v0.19.390@aaaa".to_string()),
            pending: refusals > 0,
            then_exit: false,
            refusals,
        }
    }

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + secs, 0).unwrap()
    }

    // ---- the knob -----------------------------------------------------------

    #[test]
    fn the_threshold_falls_back_from_config_to_the_default() {
        let empty = AutoUpdateConfig::default();
        assert_eq!(resolve_roll_stall_deadlines(&empty), DEFAULT_ROLL_STALL_DEADLINES);
        let configured = AutoUpdateConfig {
            roll_stall_deadlines: Some(7),
            ..AutoUpdateConfig::default()
        };
        assert_eq!(resolve_roll_stall_deadlines(&configured), 7);
    }

    #[test]
    fn a_zero_threshold_never_disables_the_detector() {
        // `0` would declare every armed roll unsatisfiable on sight, so it is
        // dropped exactly like a zero `settleSecs` / `deferDeadlineSecs`.
        let configured = AutoUpdateConfig {
            roll_stall_deadlines: Some(0),
            ..AutoUpdateConfig::default()
        };
        assert_eq!(
            resolve_roll_stall_deadlines(&configured),
            DEFAULT_ROLL_STALL_DEADLINES,
            "a 0 is filtered at read time AND in the resolver"
        );
        let mut tracker = RollStallTracker::default();
        tracker.set_threshold(0);
        assert!(
            tracker.observe(at(0), Some(&roll(0)), 2).is_none(),
            "a 0 threshold is floored at 1, so the first observation cannot fire"
        );
    }

    // ---- the happy path: a busy host that is actually draining ---------------

    #[test]
    fn an_idle_observation_is_the_only_thing_that_clears_the_episode() {
        let mut tracker = RollStallTracker::default();
        for deadline in 1..=3 {
            tracker.observe(at(i64::from(deadline) * 900), Some(&roll(deadline)), 2);
        }
        assert!(tracker.observe(at(3600), Some(&roll(4)), 2).is_some(), "declared by now");
        // Zero in flight: the wait condition is reachable after all.
        assert!(tracker.observe(at(4500), Some(&roll(4)), 0).is_none());
        assert!(!tracker.is_active(), "the episode is gone, not merely quiet");
        assert!(
            tracker.observe(at(5400), Some(&roll(1)), 2).is_none(),
            "a fresh roll starts from a clean slate"
        );
    }

    #[test]
    fn a_first_attempt_roll_inside_its_own_deadline_is_never_declared() {
        let mut tracker = RollStallTracker::default();
        // `refusals == 0` for as many ticks as the drain's own deadline spans.
        for tick in 0..8 {
            assert_eq!(tracker.observe(at(tick * 300), Some(&roll(0)), 3), None);
        }
    }

    #[test]
    fn a_decreasing_in_flight_count_rebases_the_patience() {
        let mut tracker = RollStallTracker::default();
        // Three deadlines expire, but the host is genuinely draining 4 → 3 → 2 →
        // 1, so each improvement buys another `threshold` deadlines.
        for (deadline, in_flight) in [(0, 4), (1, 3), (2, 2), (3, 1)] {
            assert_eq!(
                tracker.observe(at(i64::from(deadline) * 900), Some(&roll(deadline)), in_flight),
                None,
                "deadline {deadline} with {in_flight} in flight must not declare"
            );
        }
        // It then stops draining: three more deadlines at the same floor.
        assert_eq!(tracker.observe(at(3600), Some(&roll(4)), 1), None);
        assert_eq!(tracker.observe(at(4500), Some(&roll(5)), 1), None);
        let report = tracker.observe(at(5400), Some(&roll(6)), 1).unwrap();
        assert_eq!(report.floor, 1);
        assert_eq!(report.deadlines, 6);
    }

    // ---- the defect: deadlines counted ACROSS roll lifetimes -----------------

    #[test]
    fn deadlines_accumulate_across_a_roll_that_is_abandoned_and_re_armed() {
        let mut tracker = RollStallTracker::default();
        // Roll A reaches two refusals, then spends its budget and is abandoned.
        assert_eq!(tracker.observe(at(0), Some(&roll(1)), 2), None);
        assert_eq!(tracker.observe(at(900), Some(&roll(2)), 2), None);
        // Nothing armed: dispatch resumed, the pre-#8998 reset point.
        assert_eq!(tracker.observe(at(1800), None, 2), None);
        // Roll B is armed for the next release and refuses once. Pre-#8998 that
        // was "retry 1" all over again; now it is deadline three.
        let report = tracker.observe(at(2700), Some(&roll(1)), 2).unwrap();
        assert_eq!(report.deadlines, 3, "2 banked from roll A + 1 from roll B");
        assert_eq!(report.in_flight, 2);
        assert_eq!(report.armed_secs, 2700);
        assert_eq!(report.target.as_deref(), Some("v0.19.390@aaaa"));
    }

    #[test]
    fn deadlines_accumulate_across_a_supersede_observed_without_an_idle_gap() {
        let mut tracker = RollStallTracker::default();
        assert_eq!(tracker.observe(at(0), Some(&roll(1)), 7), None);
        assert_eq!(tracker.observe(at(900), Some(&roll(2)), 7), None);
        // #8514 superseded the pending roll onto a newer release between ticks:
        // a brand-new drain, `refusals` back to 0 — and, before #8998, a
        // brand-new paused-dispatch budget too.
        let mut newer = roll(0);
        newer.target = Some("v0.19.391@bbbb".to_string());
        assert_eq!(tracker.observe(at(1800), Some(&newer), 7), None, "0 new deadlines yet");
        newer.refusals = 1;
        let report = tracker.observe(at(2700), Some(&newer), 7).unwrap();
        assert_eq!(report.deadlines, 3);
        assert_eq!(
            report.target.as_deref(),
            Some("v0.19.391@bbbb"),
            "the report names the roll that was actually armed"
        );
    }

    #[test]
    fn the_declaration_is_re_reported_every_tick_until_the_host_goes_idle() {
        let mut tracker = RollStallTracker::default();
        for deadline in 1..=3 {
            tracker.observe(at(i64::from(deadline) * 900), Some(&roll(deadline)), 2);
        }
        assert!(tracker.is_active());
        // No roll armed any more (it was abandoned), host still busy.
        let again = tracker.observe(at(4500), None, 2).unwrap();
        assert_eq!(again.in_flight, 2);
        let and_again = tracker.observe(at(5400), None, 5).unwrap();
        assert_eq!(and_again.in_flight, 5);
        assert_eq!(and_again.floor, 2, "the floor is the episode's best, not this tick's");
    }

    // ---- what it must never touch -------------------------------------------

    #[test]
    fn a_teardown_drain_never_advances_an_episode() {
        let mut tracker = RollStallTracker::default();
        let mut teardown = roll(9);
        teardown.then_exit = true;
        for tick in 0..10 {
            assert_eq!(tracker.observe(at(tick * 900), Some(&teardown), 4), None);
        }
        assert!(!tracker.is_active(), "`fleet drain`'s teardown is not an auto-update roll");
    }

    #[test]
    fn an_untargeted_operator_drain_never_advances_an_episode() {
        let mut tracker = RollStallTracker::default();
        let mut operator = roll(9);
        operator.target = None;
        for tick in 0..10 {
            assert_eq!(tracker.observe(at(tick * 900), Some(&operator), 4), None);
        }
        assert!(!tracker.is_active());
    }

    #[test]
    fn a_busy_host_with_no_roll_armed_is_not_an_episode() {
        let mut tracker = RollStallTracker::default();
        for tick in 0..10 {
            assert_eq!(tracker.observe(at(tick * 900), None, 12), None);
        }
        assert!(!tracker.is_active());
    }

    // ---- the note -----------------------------------------------------------

    #[test]
    fn the_note_names_the_numbers_and_all_three_operator_actions() {
        let note = RollStallReport {
            deadlines: 4,
            in_flight: 7,
            floor: 2,
            armed_secs: 75_600,
            target: Some("v0.19.390@aaaa".to_string()),
        }
        .note();
        for needle in [
            "UNSATISFIABLE",
            "4 drain deadline(s)",
            "75600s",
            "7 in flight now",
            "never improved on 2",
            "v0.19.390@aaaa",
            "NORMAL DISPATCH RESUMES",
            "No sweep was cancelled",
            "loom-daemon list",
            "loom-daemon cancel --sweep",
            "--force-after-timeout",
        ] {
            assert!(note.contains(needle), "missing {needle:?} in: {note}");
        }
    }

    #[test]
    fn an_untargeted_report_does_not_render_an_empty_target_clause() {
        let note = RollStallReport {
            deadlines: 3,
            in_flight: 1,
            floor: 1,
            armed_secs: 60,
            target: None,
        }
        .note();
        assert!(!note.contains("(target "), "{note}");
    }
}
