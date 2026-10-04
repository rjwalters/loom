//! The auto-update loop's resolved tuning, bundled (Issue #8998).
//!
//! `spawn_auto_update_task` already took three loose values the call site had to
//! keep in the right order, two of which (`settle`, `defer_deadline`) are both
//! `Duration` — so a transposition would compile and silently misconfigure the
//! loop. #8998 adds a fourth knob, which is the point at which a positional list
//! stops being the right shape.
//!
//! Bundling them also keeps the knob set extensible from *one* place: a fifth
//! knob is a field here plus a line in [`TickTuning::resolve`], not a new
//! parameter threaded through `daemon_service.rs` — which is exactly how #9010's
//! `rollStallCooldownSecs` landed, one commit later. That matters because
//! `daemon_service.rs` and `auto_update.rs` are both over
//! `.loom/docs/file-size-policy.md`'s ratchet threshold, and the sanctioned way
//! to add to a frozen file is a new sibling module like this one.
//!
//! Resolution itself is not reimplemented here: each field delegates to the
//! existing `resolve_*` function, so the **env > config > default** precedence
//! and every filter on it stay in exactly one place.

use super::AutoUpdateConfig;
use std::time::Duration;

/// Every knob the loop needs, already resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TickTuning {
    /// Cadence between staleness checks.
    pub interval: Duration,
    /// Settle window: how long to wait after first observing a stale target.
    pub settle: Duration,
    /// Gate 4's bound on deferring a rebuild for in-flight sweeps (#4929).
    pub defer_deadline: Duration,
    /// #8998's unsatisfiability threshold: how many drain deadlines may expire,
    /// summed across roll lifetimes, with the in-flight count never improving
    /// before the roll is abandoned instead of re-armed.
    pub roll_stall_deadlines: u32,
    /// #9010's bound on that suppression: how long a standing declaration may
    /// stand before it expires on time and releases one more bounded roll
    /// attempt, so a host that never *samples* `in_flight == 0` is stale for a
    /// bounded period rather than indefinitely.
    pub roll_stall_cooldown: Duration,
    /// #9132's schedule: window period, per-host offset, launchd live-reload
    /// opt-in. `period: None` (the default) leaves rolls arming on every build.
    pub roll_window: super::roll_window::RollWindowTuning,
}

impl TickTuning {
    /// Resolve every knob from `config` with **env > config > default**.
    #[must_use]
    pub fn resolve(config: &AutoUpdateConfig) -> Self {
        Self {
            interval: super::resolve_interval(config),
            settle: super::resolve_settle(config),
            defer_deadline: super::resolve_defer_deadline(config),
            roll_stall_deadlines: super::resolve_roll_stall_deadlines(config),
            roll_stall_cooldown: super::resolve_roll_stall_cooldown(config),
            roll_window: super::roll_window::RollWindowTuning::resolve(
                &config.roll_window,
                super::resolve_interval(config),
            ),
        }
    }

    /// The one-line rendering both the "enabled" and "starting loop" log lines
    /// use, so the two can never disagree about what the loop was configured
    /// with.
    #[must_use]
    pub fn describe(&self) -> String {
        format!(
            "interval={}s, settle={}s, deferDeadline={}s, rollStallDeadlines={}, \
             rollStallCooldown={}s, {}",
            self.interval.as_secs(),
            self.settle.as_secs(),
            self.defer_deadline.as_secs(),
            self.roll_stall_deadlines,
            self.roll_stall_cooldown.as_secs(),
            self.roll_window.describe()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auto_update::roll_window::{RollWindowConfig, RollWindowTuning};
    use crate::auto_update::{
        DEFAULT_AUTO_UPDATE_DEFER_DEADLINE_SECS, DEFAULT_AUTO_UPDATE_INTERVAL_SECS,
        DEFAULT_AUTO_UPDATE_SETTLE_SECS, DEFAULT_ROLL_STALL_COOLDOWN_SECS,
        DEFAULT_ROLL_STALL_DEADLINES,
    };
    use serial_test::serial;

    /// Every env override the resolvers read, cleared so a tier test measures the
    /// tier it means to.
    const ENV_VARS: [&str; 8] = [
        crate::auto_update::AUTO_UPDATE_INTERVAL_ENV,
        crate::auto_update::AUTO_UPDATE_SETTLE_ENV,
        crate::auto_update::AUTO_UPDATE_DEFER_DEADLINE_ENV,
        crate::auto_update::AUTO_UPDATE_ROLL_STALL_DEADLINES_ENV,
        crate::auto_update::AUTO_UPDATE_ROLL_STALL_COOLDOWN_SECS_ENV,
        crate::auto_update::roll_window::ROLL_WINDOW_SECS_ENV,
        crate::auto_update::roll_window::ROLL_WINDOW_OFFSET_SECS_ENV,
        crate::auto_update::roll_window::LAUNCHD_LIVE_RELOAD_ENV,
    ];

    #[test]
    #[serial(loom_auto_update_env)]
    fn an_absent_config_resolves_every_knob_to_its_default() {
        for var in ENV_VARS {
            std::env::remove_var(var);
        }
        let tuning = TickTuning::resolve(&AutoUpdateConfig::default());
        assert_eq!(tuning.interval, Duration::from_secs(DEFAULT_AUTO_UPDATE_INTERVAL_SECS));
        assert_eq!(tuning.settle, Duration::from_secs(DEFAULT_AUTO_UPDATE_SETTLE_SECS));
        assert_eq!(
            tuning.defer_deadline,
            Duration::from_secs(DEFAULT_AUTO_UPDATE_DEFER_DEADLINE_SECS)
        );
        assert_eq!(tuning.roll_stall_deadlines, DEFAULT_ROLL_STALL_DEADLINES);
        assert_eq!(
            tuning.roll_stall_cooldown,
            Duration::from_secs(DEFAULT_ROLL_STALL_COOLDOWN_SECS)
        );
        // Windowing is opt-in: the default leaves the arm-on-build behaviour intact.
        assert_eq!(tuning.roll_window.period, None);
        assert!(!tuning.roll_window.launchd_live_reload);
    }

    #[test]
    #[serial(loom_auto_update_env)]
    fn the_config_tier_reaches_every_knob() {
        for var in ENV_VARS {
            std::env::remove_var(var);
        }
        let tuning = TickTuning::resolve(&AutoUpdateConfig {
            enabled: Some(true),
            interval_secs: Some(120),
            settle_secs: Some(30),
            defer_deadline_secs: Some(7200),
            roll_stall_deadlines: Some(5),
            roll_stall_cooldown_secs: Some(10_800),
            roll_window: RollWindowConfig {
                period_secs: Some(21_600),
                offset_secs: Some(900),
                launchd_live_reload: Some(true),
            },
        });
        assert_eq!(
            tuning,
            TickTuning {
                interval: Duration::from_secs(120),
                settle: Duration::from_secs(30),
                defer_deadline: Duration::from_secs(7200),
                roll_stall_deadlines: 5,
                roll_stall_cooldown: Duration::from_secs(10_800),
                roll_window: RollWindowTuning {
                    period: Some(Duration::from_secs(21_600)),
                    offset: Duration::from_secs(900),
                    // max(2 x 120s interval, 600s floor), under the 6h period.
                    open_for: Duration::from_secs(600),
                    launchd_live_reload: true,
                },
            }
        );
        // The description is what both startup log lines render, so pin it.
        assert_eq!(
            tuning.describe(),
            "interval=120s, settle=30s, deferDeadline=7200s, rollStallDeadlines=5, \
             rollStallCooldown=10800s, \
             rollWindow=21600s, rollWindowOffset=900s, launchdLiveReload=true"
        );
    }
}
