//! When the worker stops asking the captain, and when it starts again
//! (#12450): fail-open with hysteresis.
//!
//! - **Normal.** Every claim asks the captain. A call that fails (no answer,
//!   a timeout, a non-2xx, a malformed body) falls open for that call only:
//!   the step runs today's forge path. [`ENTER_AFTER_FAILURES`] failures in
//!   a row enter fallback.
//! - **Fallback.** Claims do not ask the captain at all, so a dead captain
//!   costs no timeout per spawn, and the step tick draws with the reduced
//!   budget ([`draw_budget`]: one round, and no free `claim_lost`), so a
//!   captain blip does not restart the fleet-wide race at full rate. The
//!   heartbeat keeps probing. Fallback
//!   ends once every probe has succeeded for [`LEAVE_AFTER`]; one failed
//!   probe restarts that clock.
//!
//! A claim refusal is an answer, so it counts as a success. A `grace`
//! refusal (the captain just started) counts as neither: the caller does not
//! observe it at all (spec #12452 §9.1), so a routine captain roll never
//! pushes the fleet into fallback.
//!
//! Pure: the caller passes the clock in.

use std::time::{Duration, Instant};

/// Consecutive failed calls that enter fallback.
pub const ENTER_AFTER_FAILURES: u32 = 3;
/// How long every probe must succeed before fallback ends.
pub const LEAVE_AFTER: Duration = Duration::from_secs(5 * 60);

/// Whether the worker asks the captain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Normal,
    Fallback,
}

impl Mode {
    /// The attribute value (`loom.fleet_dispatch.mode`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Fallback => "fallback",
        }
    }
}

/// A mode change, for the log line and the span.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    Entered,
    Left,
}

/// The hysteresis state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Hysteresis {
    mode: Mode,
    /// Failed calls in a row (normal mode).
    failures: u32,
    /// When the current run of successful probes began (fallback mode).
    ok_since: Option<Instant>,
}

impl Hysteresis {
    /// The current mode.
    #[must_use]
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Record one call's outcome at `now`; the mode change it caused, if any.
    pub fn observe(&mut self, ok: bool, now: Instant) -> Option<Transition> {
        match (self.mode, ok) {
            (Mode::Normal, true) => {
                self.failures = 0;
                None
            }
            (Mode::Normal, false) => {
                self.failures = self.failures.saturating_add(1);
                (self.failures >= ENTER_AFTER_FAILURES).then(|| {
                    *self = Self {
                        mode: Mode::Fallback,
                        failures: 0,
                        ok_since: None,
                    };
                    Transition::Entered
                })
            }
            (Mode::Fallback, false) => {
                self.ok_since = None;
                None
            }
            (Mode::Fallback, true) => {
                let since = *self.ok_since.get_or_insert(now);
                (now.saturating_duration_since(since) >= LEAVE_AFTER).then(|| {
                    *self = Self::default();
                    Transition::Left
                })
            }
        }
    }
}

/// The slots a step tick may fill: all of `free_slots` normally; in
/// fallback one round, `spawn_concurrency` slots at most (spec #12452 §9.3).
/// Pure.
#[must_use]
pub fn draw_budget(mode: Mode, free_slots: usize, spawn_concurrency: usize) -> usize {
    match mode {
        Mode::Normal => free_slots,
        Mode::Fallback => free_slots.min(spawn_concurrency.max(1)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    #[test]
    fn two_failures_stay_normal_and_the_third_enters_fallback() {
        let t0 = Instant::now();
        let mut h = Hysteresis::default();
        assert_eq!(h.observe(false, t0), None);
        assert_eq!(h.observe(false, t0), None);
        assert_eq!(h.mode(), Mode::Normal);
        assert_eq!(h.observe(false, t0), Some(Transition::Entered));
        assert_eq!(h.mode(), Mode::Fallback);
    }

    #[test]
    fn a_success_resets_the_failure_run() {
        let t0 = Instant::now();
        let mut h = Hysteresis::default();
        h.observe(false, t0);
        h.observe(false, t0);
        assert_eq!(h.observe(true, t0), None);
        h.observe(false, t0);
        h.observe(false, t0);
        assert_eq!(h.mode(), Mode::Normal, "the run restarted after the success");
        assert_eq!(h.observe(false, t0), Some(Transition::Entered));
    }

    #[test]
    fn fallback_leaves_only_after_five_minutes_of_successes() {
        let t0 = Instant::now();
        let mut h = Hysteresis::default();
        for _ in 0..3 {
            h.observe(false, t0);
        }
        assert_eq!(h.observe(true, t0 + secs(10)), None, "the clock starts");
        assert_eq!(h.observe(true, t0 + secs(10) + secs(299)), None);
        assert_eq!(h.mode(), Mode::Fallback);
        assert_eq!(h.observe(true, t0 + secs(10) + secs(300)), Some(Transition::Left));
        assert_eq!(h.mode(), Mode::Normal);
        assert_eq!(h, Hysteresis::default(), "leaving resets every counter");
    }

    #[test]
    fn a_failed_probe_in_fallback_restarts_the_leave_clock() {
        let t0 = Instant::now();
        let mut h = Hysteresis::default();
        for _ in 0..3 {
            h.observe(false, t0);
        }
        h.observe(true, t0);
        h.observe(true, t0 + secs(240));
        assert_eq!(h.observe(false, t0 + secs(250)), None);
        assert_eq!(h.observe(true, t0 + secs(260)), None, "a new run begins");
        assert_eq!(h.observe(true, t0 + secs(300)), None, "five minutes from t0 is not enough");
        assert_eq!(h.observe(true, t0 + secs(560)), Some(Transition::Left));
    }

    #[test]
    fn failures_in_fallback_never_re_enter() {
        let t0 = Instant::now();
        let mut h = Hysteresis::default();
        for _ in 0..3 {
            h.observe(false, t0);
        }
        for _ in 0..10 {
            assert_eq!(h.observe(false, t0), None);
        }
        assert_eq!(h.mode(), Mode::Fallback);
    }

    #[test]
    fn the_draw_budget_shrinks_only_in_fallback() {
        assert_eq!(draw_budget(Mode::Normal, 12, 3), 12);
        assert_eq!(draw_budget(Mode::Fallback, 12, 3), 3, "one round");
        assert_eq!(draw_budget(Mode::Fallback, 12, 0), 1, "a round is at least one spawn");
        assert_eq!(draw_budget(Mode::Fallback, 1, 3), 1);
        assert_eq!(draw_budget(Mode::Fallback, 0, 3), 0);
    }
}
