//! Short re-check cadence for a standing unsatisfiable floor (Issue #12072).
//!
//! A floor raised while a release's assets are still uploading is
//! unsatisfiable on this platform until the binary and its `.sha256` land.
//! The floor-change wake fires once, on the change, so without this the host
//! would ask again only on the next `autoUpdate` interval and the roll would
//! trail the asset by up to that long.
//!
//! While [`FloorState::stall`] stands the loop therefore re-checks every
//! [`STALL_RECHECK`], for up to [`STALL_WINDOW`] from the stall's declaration;
//! after that the normal interval resumes (a stall that long is not an upload
//! race). The ERROR line is paced separately by [`super::floor_roll::alert`],
//! so the faster cadence adds no log noise. Internal constants, no config key.

use super::floor_roll::FloorState;
use chrono::{DateTime, Utc};
use std::time::Duration;

/// How soon to ask again while the floor is unsatisfiable.
pub const STALL_RECHECK: Duration = Duration::from_secs(120);

/// How long the short cadence lasts, from the stall's declaration.
pub const STALL_WINDOW: Duration = Duration::from_secs(3600);

/// The delay before the next tick: [`STALL_RECHECK`] (never longer than
/// `interval`) while a stall stands and is younger than [`STALL_WINDOW`],
/// otherwise `interval`.
#[must_use]
pub fn next_tick_delay(stall_age: Option<Duration>, interval: Duration) -> Duration {
    match stall_age {
        Some(age) if age < STALL_WINDOW => interval.min(STALL_RECHECK),
        _ => interval,
    }
}

/// [`next_tick_delay`] for the floor as of `now`. A stall not yet alerted has
/// no declaration time and counts as age zero.
#[must_use]
pub fn delay_after_tick(floor: &FloorState, now: DateTime<Utc>, interval: Duration) -> Duration {
    let age = floor.stall().map(|_| {
        floor
            .stall_declared_at()
            .and_then(|at| (now - at).to_std().ok())
            .unwrap_or_default()
    });
    next_tick_delay(age, interval)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: Duration = Duration::from_secs(3600);
    const INTERVAL: Duration = Duration::from_secs(900);

    #[test]
    fn a_fresh_stall_rechecks_in_two_minutes() {
        assert_eq!(next_tick_delay(Some(Duration::ZERO), INTERVAL), STALL_RECHECK);
        assert_eq!(next_tick_delay(Some(HOUR - Duration::from_secs(1)), INTERVAL), STALL_RECHECK);
    }

    #[test]
    fn a_stall_older_than_an_hour_uses_the_interval() {
        assert_eq!(next_tick_delay(Some(HOUR), INTERVAL), INTERVAL);
        assert_eq!(next_tick_delay(Some(HOUR * 5), INTERVAL), INTERVAL);
    }

    #[test]
    fn no_stall_uses_the_interval() {
        assert_eq!(next_tick_delay(None, INTERVAL), INTERVAL);
    }

    #[test]
    fn a_short_interval_is_never_lengthened() {
        let short = Duration::from_secs(30);
        assert_eq!(next_tick_delay(Some(Duration::ZERO), short), short);
    }

    fn stalled(declared: DateTime<Utc>) -> FloorState {
        use crate::fleet_sync::FloorKnowledge;
        let mut state = FloorState::default();
        state.set_basis(FloorKnowledge::Set("0.19.950".to_string()), "0.19.800");
        let newest = super::super::floor_roll::Release {
            tag: "v0.19.900".to_string(),
            version: "0.19.900".to_string(),
        };
        state.observe(Some(&newest));
        assert!(state.alert_due(declared, super::super::floor_roll::alert::REMINDER));
        state
    }

    #[test]
    fn delay_after_tick_reads_the_floor_state() {
        let t0 = Utc::now();
        assert_eq!(delay_after_tick(&FloorState::default(), t0, INTERVAL), INTERVAL);
        let state = stalled(t0);
        let later = |secs| t0 + chrono::Duration::seconds(secs);
        assert_eq!(delay_after_tick(&state, later(0), INTERVAL), STALL_RECHECK);
        assert_eq!(delay_after_tick(&state, later(3599), INTERVAL), STALL_RECHECK);
        assert_eq!(delay_after_tick(&state, later(3600), INTERVAL), INTERVAL);
    }

    #[test]
    fn rechecks_inside_the_window_do_not_re_alert() {
        let t0 = Utc::now();
        let mut state = stalled(t0);
        // A re-check every STALL_RECHECK for the whole window: one alert only.
        for n in 1..30 {
            let now = t0 + chrono::Duration::seconds(120 * n);
            assert!(!state.alert_due(now, super::super::floor_roll::alert::REMINDER), "{n}");
        }
    }

    #[test]
    fn a_cleared_stall_returns_to_the_interval() {
        let t0 = Utc::now();
        let mut state = stalled(t0);
        let landed = super::super::floor_roll::Release {
            tag: "v0.19.950".to_string(),
            version: "0.19.950".to_string(),
        };
        state.observe(Some(&landed));
        assert_eq!(delay_after_tick(&state, t0, INTERVAL), INTERVAL);
    }
}
