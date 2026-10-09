//! The auto-update loop's resolved tuning, bundled (Issue #8998).
//!
//! `spawn_auto_update_task` already took three loose values the call site had to
//! keep in the right order, two of which (`settle`, `defer_deadline`) are both
//! `Duration` — so a transposition would compile and silently misconfigure the
//! loop. #8998 adds a fourth knob, which is the point at which a positional list
//! stops being the right shape.
//!
//! Bundling them also keeps the knob set extensible from *one* place: another
//! knob is a field here plus a line in [`TickTuning::resolve`], not a new
//! parameter threaded through `daemon_service.rs`. (#8998's and #9010's stall
//! knobs lived here until #10831 removed the wait-for-zero roll they tuned;
//! the pause-and-roll budgets are resolved per roll by
//! [`super::pause_roll::PauseRollTuning`].) That matters because
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
}

impl TickTuning {
    /// Resolve every knob from `config` with **env > config > default**.
    #[must_use]
    pub fn resolve(config: &AutoUpdateConfig) -> Self {
        Self {
            interval: super::resolve_interval(config),
            settle: super::resolve_settle(config),
            defer_deadline: super::resolve_defer_deadline(config),
        }
    }

    /// The one-line rendering both the "enabled" and "starting loop" log lines
    /// use, so the two can never disagree about what the loop was configured
    /// with.
    #[must_use]
    pub fn describe(&self) -> String {
        format!(
            "interval={}s, settle={}s, deferDeadline={}s",
            self.interval.as_secs(),
            self.settle.as_secs(),
            self.defer_deadline.as_secs()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auto_update::removed_settings::REMOVED;
    use crate::auto_update::{
        DEFAULT_AUTO_UPDATE_DEFER_DEADLINE_SECS, DEFAULT_AUTO_UPDATE_INTERVAL_SECS,
        DEFAULT_AUTO_UPDATE_SETTLE_SECS,
    };
    use serial_test::serial;

    /// Every env override the resolvers read, cleared so a tier test measures the
    /// tier it means to.
    const ENV_VARS: [&str; 3] = [
        crate::auto_update::AUTO_UPDATE_INTERVAL_ENV,
        crate::auto_update::AUTO_UPDATE_SETTLE_ENV,
        crate::auto_update::AUTO_UPDATE_DEFER_DEADLINE_ENV,
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
            removed_keys: Vec::new(),
        });
        assert_eq!(
            tuning,
            TickTuning {
                interval: Duration::from_secs(120),
                settle: Duration::from_secs(30),
                defer_deadline: Duration::from_secs(7200),
            }
        );
        // The description is what both startup log lines render, so pin it.
        assert_eq!(tuning.describe(), "interval=120s, settle=30s, deferDeadline=7200s");
    }

    /// #10885: the removed roll-window settings change nothing. A config that
    /// still carries the keys, in a process that still sets the env vars,
    /// resolves to the tuning it would without them.
    #[test]
    #[serial(loom_auto_update_env)]
    fn the_removed_window_settings_do_not_change_the_tuning() {
        for var in ENV_VARS {
            std::env::remove_var(var);
        }
        let plain = AutoUpdateConfig {
            enabled: Some(true),
            interval_secs: Some(120),
            settle_secs: Some(30),
            defer_deadline_secs: Some(7200),
            removed_keys: Vec::new(),
        };
        let expected = TickTuning::resolve(&plain);

        for removed in REMOVED {
            std::env::set_var(removed.env, "21600");
        }
        let old = AutoUpdateConfig {
            removed_keys: REMOVED.iter().map(|r| r.key).collect(),
            ..plain
        };
        let resolved = TickTuning::resolve(&old);
        for removed in REMOVED {
            std::env::remove_var(removed.env);
        }
        assert_eq!(resolved, expected);
        assert!(!resolved.describe().contains("ollWindow"), "{}", resolved.describe());
    }
}
