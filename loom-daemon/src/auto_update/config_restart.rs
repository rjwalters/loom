//! Arm the pause-and-roll from restart-required config changes (#12425, child 3
//! of #12419).
//!
//! [`crate::fleet_sync::config_restart_demand`] says which restart-required keys
//! changed since this process booted. This module is the consumer: on a host
//! that writes its config, a standing demand with no version roll arming becomes
//! [`TickDecision::ConfigRestart`], and the tick starts the one roll path
//! (`trigger_pause_roll`) onto the binary already running.
//!
//! What it is not: a version roll. It fetches nothing, waits for no window,
//! settle, debounce or interval, records no artifact roll and no roll attempt,
//! and leaves `last_roll`, the backoff and the convergence guard as they were.
//! The restart's fresh snapshot has no demand, so nothing re-arms after it.

use super::pause_roll::RollTarget;
use super::{AutoUpdateState, TickDecision};
use crate::fleet_sync::config_restart::ConfigRestartDemand;

impl AutoUpdateState {
    /// #12425: the demand this tick may arm a restart for. Read once per tick
    /// beside the floor and repo-ahead inputs.
    pub(super) fn set_config_restart(&mut self, demand: Option<ConfigRestartDemand>) {
        self.config_restart = demand;
    }
}

/// Replace a tick that would do nothing with the config restart, when one is
/// due. A decision that fetches or rebuilds is a version roll arming: its
/// restart clears the demand, so it keeps the tick. A host with no fleet store
/// arms nothing.
#[must_use]
pub(super) fn arm(state: &AutoUpdateState, decision: TickDecision) -> TickDecision {
    let Some(demand) = state.config_restart.as_ref() else {
        return decision;
    };
    if !state.floor.fleet_host()
        || !matches!(decision, TickDecision::Skip(_) | TickDecision::SkipWarn(_))
    {
        return decision;
    }
    TickDecision::ConfigRestart {
        paths: demand.paths.clone(),
    }
}

/// The roll target: the running version, no artifact.
#[must_use]
pub(super) fn target() -> RollTarget {
    RollTarget::config_restart(env!("CARGO_PKG_VERSION"))
}

/// The tick's note: the paths that need the restart.
#[must_use]
pub(super) fn note(paths: &[String], armed: bool) -> String {
    let what = if armed {
        "restarting this daemon onto the same binary"
    } else {
        "the pause-and-roll refused to start (see the daemon log); retrying next tick"
    };
    format!("restart-required config changed since boot ({}): {what}", paths.join(", "))
}
