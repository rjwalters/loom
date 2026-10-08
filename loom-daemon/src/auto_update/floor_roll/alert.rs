//! The unsatisfiable-floor alert: when to log it, and what a restart keeps
//! (Issue #10866, follow-up to #10712).
//!
//! An unsatisfiable floor stands until an operator fixes it, and the tick
//! that finds it runs every `autoUpdateIntervalSecs` (900s by default).
//! Logging the ERROR on every tick was about 96 identical lines a day per
//! host. The line is now logged when the stall starts, when it changes (a
//! different floor, running version or newest release), and otherwise once
//! per [`REMINDER`].
//!
//! Only the log line is rate-limited. The stall itself, its `status` note and
//! the tick record's `floor_stall` field are state, not alerts, and are
//! present on every tick the stall stands.
//!
//! # Across a restart
//!
//! The record of the last alert is saved in `auto_update_state.json` as
//! `floor_stall` ([`FloorStallState`]) and restored at spawn, so a restart
//! inside the reminder interval logs nothing new and the next reminder is due
//! on the original schedule. It is a separate field from the drain detector's
//! `stall`: the two can stand at once, and they are dropped for different
//! reasons. A floor stall is valid for its `(floor, running)` pair, whatever
//! build commit wrote it, and [`FloorState::set_basis`] checks exactly that on
//! the first tick.
//!
//! A child module so it can read [`FloorState`]'s private fields without
//! widening them.

use super::{FloorStallReport, FloorState};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// How often a standing, unchanged stall is logged again. Not configurable:
/// nothing asks for it to be.
pub const REMINDER: Duration = Duration::from_secs(3600);

/// The stall last alerted on, and when.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FloorAlert {
    /// The stall as it was last logged.
    pub(super) report: FloorStallReport,
    /// When this stall (this floor, running version and newest release) was
    /// first logged.
    pub(super) declared_at: DateTime<Utc>,
    /// When it was last logged.
    pub(super) last_alerted_at: DateTime<Utc>,
}

/// The `floor_stall` value in `auto_update_state.json`: [`FloorAlert`] as
/// plain fields. Times are UTC, like the drain detector's `declared_at`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FloorStallState {
    /// The floor that could not be satisfied.
    pub floor: String,
    /// The version that was running below it.
    pub running: String,
    /// The newest release's version, also below it.
    pub newest: String,
    /// When the stall was first alerted.
    pub declared_at: DateTime<Utc>,
    /// When it was last alerted.
    pub last_alerted_at: DateTime<Utc>,
}

impl FloorState {
    /// Whether to log the standing stall's ERROR line at `now`, recording the
    /// alert when the answer is yes. Call once per tick, after
    /// [`Self::observe`].
    ///
    /// True when there is a stall and any of:
    ///
    /// - nothing was alerted yet (the stall started);
    /// - its floor, running version or newest release differs from the one
    ///   last alerted (the stall changed, which also restarts `declared_at`);
    /// - `reminder` has passed since the last alert;
    /// - the last alert is in the future (the wall clock stepped back): alert
    ///   rather than stay silent for an unknown time.
    ///
    /// A stall kept across an unresolved tick is the same report, so it is
    /// neither a start nor a change. False, with the record cleared, when no
    /// stall stands.
    pub fn alert_due(&mut self, now: DateTime<Utc>, reminder: Duration) -> bool {
        let Some(report) = self.stall().cloned() else {
            self.alert = None;
            return false;
        };
        match &mut self.alert {
            Some(alert) if alert.report == report => {
                let due = (now - alert.last_alerted_at)
                    .to_std()
                    .map_or(true, |age| age >= reminder);
                if due {
                    alert.last_alerted_at = now;
                }
                due
            }
            _ => {
                self.alert = Some(FloorAlert {
                    report,
                    declared_at: now,
                    last_alerted_at: now,
                });
                true
            }
        }
    }

    /// When the standing stall was first alerted, for the log line.
    #[must_use]
    pub fn stall_declared_at(&self) -> Option<DateTime<Utc>> {
        self.alert.as_ref().map(|alert| alert.declared_at)
    }

    /// The alert record to persist, `None` when no stall has been alerted.
    #[must_use]
    pub fn stall_state(&self) -> Option<FloorStallState> {
        self.alert.as_ref().map(|alert| FloorStallState {
            floor: alert.report.floor.clone(),
            running: alert.report.running.clone(),
            newest: alert.report.newest.clone(),
            declared_at: alert.declared_at,
            last_alerted_at: alert.last_alerted_at,
        })
    }

    /// Take a persisted alert record. It is not trusted yet: the next
    /// [`Self::set_basis`] keeps it only if its floor and running version are
    /// the basis then in force, and drops it otherwise. Call before the first
    /// tick.
    pub fn restore_stall_state(&mut self, saved: Option<FloorStallState>) {
        self.alert = saved.map(|saved| FloorAlert {
            report: FloorStallReport {
                floor: saved.floor,
                running: saved.running,
                newest: saved.newest,
            },
            declared_at: saved.declared_at,
            last_alerted_at: saved.last_alerted_at,
        });
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::Release;
    use super::*;
    use chrono::TimeZone;

    const FLOOR: &str = "9.0.0";
    const RUNNING: &str = "0.19.800";

    fn rel(version: &str) -> Release {
        Release {
            tag: format!("v{version}"),
            version: version.to_string(),
        }
    }

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 8, 12, 0, 0).unwrap()
    }

    fn mins(n: i64) -> chrono::Duration {
        chrono::Duration::minutes(n)
    }

    /// A state with an unsatisfiable floor standing, nothing alerted yet.
    fn stalled() -> FloorState {
        let mut state = FloorState::default();
        state.set_basis(Some(FLOOR.to_string()), RUNNING);
        state.observe(Some(&rel("0.19.900")));
        assert!(state.stall().is_some());
        state
    }

    #[test]
    fn the_alert_logs_on_start_then_once_per_reminder() {
        let mut state = stalled();
        assert!(state.alert_due(t0(), REMINDER), "the stall started");
        // The 900s ticks inside the hour stay quiet, and do not push the
        // reminder back.
        for m in [15, 30, 45, 59] {
            state.observe(Some(&rel("0.19.900")));
            assert!(!state.alert_due(t0() + mins(m), REMINDER), "+{m}m");
        }
        state.observe(Some(&rel("0.19.900")));
        assert!(state.alert_due(t0() + mins(60), REMINDER), "60 min after the last alert");
        assert!(!state.alert_due(t0() + mins(75), REMINDER));
        assert!(!state.alert_due(t0() + mins(119), REMINDER));
        assert!(state.alert_due(t0() + mins(120), REMINDER));
        // The stall is the same one throughout.
        assert_eq!(state.stall_declared_at(), Some(t0()));
    }

    #[test]
    fn a_changed_stall_logs_at_once_and_restarts_declared_at() {
        // A newer release that still does not satisfy the floor.
        let mut state = stalled();
        assert!(state.alert_due(t0(), REMINDER));
        state.observe(Some(&rel("0.19.901")));
        assert!(state.alert_due(t0() + mins(15), REMINDER), "newest changed");
        assert_eq!(state.stall_declared_at(), Some(t0() + mins(15)));
        assert!(!state.alert_due(t0() + mins(30), REMINDER));

        // A different (still unsatisfiable) floor.
        state.set_basis(Some("8.0.0".to_string()), RUNNING);
        state.observe(Some(&rel("0.19.901")));
        assert!(state.alert_due(t0() + mins(45), REMINDER), "floor changed");
        assert_eq!(state.stall_state().unwrap().floor, "8.0.0");

        // A different running version.
        state.set_basis(Some("8.0.0".to_string()), "0.19.801");
        state.observe(Some(&rel("0.19.901")));
        assert!(state.alert_due(t0() + mins(46), REMINDER), "running changed");
        assert_eq!(state.stall_state().unwrap().running, "0.19.801");
    }

    #[test]
    fn a_last_alert_in_the_future_logs() {
        let mut state = stalled();
        assert!(state.alert_due(t0(), REMINDER));
        // The wall clock steps back two hours.
        let stepped = t0() - mins(120);
        assert!(state.alert_due(stepped, REMINDER));
        // ...and the schedule follows the new clock.
        assert!(!state.alert_due(stepped + mins(15), REMINDER));
        assert!(state.alert_due(stepped + mins(60), REMINDER));
    }

    #[test]
    fn a_kept_stall_is_neither_a_start_nor_a_change() {
        let mut state = stalled();
        assert!(state.alert_due(t0(), REMINDER));
        state.observe(None);
        assert!(state.stall().is_some());
        assert!(!state.alert_due(t0() + mins(15), REMINDER));
        // The reminder still fires on an unresolved tick.
        state.observe(None);
        assert!(state.alert_due(t0() + mins(60), REMINDER));
    }

    #[test]
    fn a_stall_that_clears_and_returns_is_a_new_start() {
        let mut state = stalled();
        assert!(state.alert_due(t0(), REMINDER));
        // A release satisfies the floor: no stall, no alert, no record.
        state.observe(Some(&rel(FLOOR)));
        assert!(!state.alert_due(t0() + mins(15), REMINDER));
        assert_eq!(state.stall_state(), None);
        // The release is withdrawn: the stall is back, inside the old interval.
        state.observe(Some(&rel("0.19.900")));
        assert!(state.alert_due(t0() + mins(30), REMINDER));
        assert_eq!(state.stall_declared_at(), Some(t0() + mins(30)));
    }

    #[test]
    fn no_stall_never_alerts() {
        let mut state = FloorState::default();
        assert!(!state.alert_due(t0(), REMINDER));
        state.set_basis(Some("0.19.850".to_string()), RUNNING);
        state.observe(None);
        assert!(!state.alert_due(t0(), REMINDER), "unresolved is noted, not alerted");
        state.observe(Some(&rel("0.19.900")));
        assert!(!state.alert_due(t0(), REMINDER), "below a satisfiable floor");
        assert_eq!(state.stall_state(), None);
    }

    #[test]
    fn a_restored_record_is_kept_only_for_the_basis_it_was_saved_under() {
        let mut before = stalled();
        assert!(before.alert_due(t0(), REMINDER));
        let saved = before.stall_state();
        assert!(saved.is_some());

        // Same floor and running version: the stall stands before anything is
        // observed, and the record round-trips unchanged.
        let mut same = FloorState::default();
        same.restore_stall_state(saved.clone());
        same.set_basis(Some(FLOOR.to_string()), RUNNING);
        assert_eq!(same.stall(), before.stall());
        assert_eq!(same.stall_state(), saved);

        // A different floor, a different running version, or no floor: dropped.
        for (floor, running) in [
            (Some("8.0.0"), RUNNING),
            (Some(FLOOR), "0.19.801"),
            (None, RUNNING),
        ] {
            let mut other = FloorState::default();
            other.restore_stall_state(saved.clone());
            other.set_basis(floor.map(str::to_string), running);
            assert!(other.stall().is_none(), "{floor:?} {running}");
            assert_eq!(other.stall_state(), None, "{floor:?} {running}");
        }
    }
}
