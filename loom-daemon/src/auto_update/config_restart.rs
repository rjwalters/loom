//! The self-update loop's `config_restart` trigger (Issue #10720).
//!
//! Fleet-sync keeps a process-wide demand: the restart-required config paths
//! changed since this daemon booted ([`crate::fleet_sync::config_restart`]).
//! Each tick, after the version-roll decision, [`after_tick`] acts on it:
//!
//! - **Not armable** (`fleet.autoApply` off, a `Mode::Check` host): reported
//!   in the tick note and record; nothing is armed. `loom-daemon status`
//!   already shows the pending restart from fleet-sync's marker.
//! - **A version roll armed this tick, or a roll or drain already in
//!   progress**: folded into it. That restart starts a new process, whose boot
//!   snapshot equals the disk, so the demand is gone with it.
//! - **Otherwise**: one [`RollTrigger::trigger_pause_roll`] with
//!   [`RollTarget::config_restart`] — the running version's own tag, source
//!   `config_restart`. There is no second restart path: the pause, the
//!   5-minute resumability rule, budgets, supervisor checks, operator-drain
//!   precedence and `EXIT_RESTART` are #10831's.
//!
//! No roll window, jitter, debounce or minimum interval: the demand acts on
//! the next tick (#10885), and writes before it coalesce into the one
//! boot-relative demand. Nothing is fetched: the on-disk binary is the running
//! one (a newer staged binary is a version roll, which the path above owns).
//!
//! # Not a completed roll
//!
//! The binary is the same before and after, so this never stamps `last_roll`,
//! never writes the artifact-roll record, and never resets backoff or the
//! convergence guard. The state is persisted before the arm (the restart may
//! follow at once), and since the same binary reads it back, the settle clocks
//! and the floor-stall episode survive the restart (#10877's same-binary rule).
//!
//! **Paths are never values**: the note and the record carry a count, and the
//! log line names the paths only.

use super::super::tick_telemetry::TickSummary;
use super::super::{AutoUpdateState, AutoUpdateStatus, RollTarget, RollTrigger};
use crate::fleet_sync::ConfigRestartDemand;
use crate::telemetry::kinds::auto_update_tick::TickDecisionKind;

/// What a tick does with a standing demand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::auto_update) enum Action {
    /// `fleet.autoApply` is off: report only.
    Report,
    /// A restart is already coming; the demand rides on it. The text names it.
    Fold(String),
    /// Arm the pause-and-roll onto the running binary.
    Arm,
}

/// The decision, pure of I/O. `roll_armed` is whether this tick's version
/// decision armed a roll.
pub(in crate::auto_update) fn decide<T: RollTrigger>(
    demand: &ConfigRestartDemand,
    roll_armed: bool,
    trigger: &T,
) -> Action {
    if !demand.armable {
        return Action::Report;
    }
    if roll_armed {
        return Action::Fold("the version roll this tick armed".to_string());
    }
    if trigger.roll_in_progress() {
        let why = match trigger.armed_roll() {
            Some(roll) if roll.then_exit => "the operator stop (drain then exit) in progress",
            Some(roll) if roll.committed => "the roll or operator drain already in progress",
            _ => "the roll already in progress",
        };
        return Action::Fold(why.to_string());
    }
    Action::Arm
}

/// The running artifact's published sha256, when the last artifact this
/// daemon installed is the running version.
fn running_artifact_sha256(state: &AutoUpdateState) -> Option<String> {
    state
        .last_artifact_roll
        .as_ref()
        .filter(|r| r.version == env!("CARGO_PKG_VERSION") && !r.asset_sha256.is_empty())
        .map(|r| r.asset_sha256.clone())
}

/// Act on `demand` after a tick's version decision (see the module doc), and
/// fold the outcome into `summary` and the published status note.
pub(in crate::auto_update) fn after_tick<T: RollTrigger>(
    state: &AutoUpdateState,
    status: &AutoUpdateStatus,
    trigger: &T,
    demand: Option<&ConfigRestartDemand>,
    mut summary: TickSummary,
) -> TickSummary {
    let Some(demand) = demand else {
        return summary;
    };
    let n = demand.paths.len();
    summary.config_restart_paths = Some(n);
    let addendum = match decide(demand, summary.roll_armed, trigger) {
        Action::Report => format!(
            "[config restart pending: {n} restart-required path(s) changed since this daemon \
             started; fleet.autoApply is off, so nothing is armed]"
        ),
        Action::Fold(why) => {
            log::info!(
                "auto_update: {n} restart-required config path(s) changed since boot ride on {why}"
            );
            format!("[config restart: {n} path(s) folded into {why}; its restart applies them]")
        }
        Action::Arm => {
            // Written before the arm: the restart may follow at once.
            state.persist_state();
            let target = RollTarget::config_restart(
                env!("CARGO_PKG_VERSION"),
                running_artifact_sha256(state),
            );
            if trigger.trigger_pause_roll(&target) {
                summary.decision = TickDecisionKind::ConfigRestart;
                summary.roll_armed = true;
                log::warn!(
                    "auto_update: restart-required config changed since this daemon started ({}) \
                     — pause-and-roll armed onto the same binary ({})",
                    demand.paths.join(", "),
                    env!("CARGO_PKG_VERSION")
                );
                format!(
                    "[config restart armed: {n} restart-required path(s); pause-and-roll onto the \
                     same binary]"
                )
            } else {
                log::warn!(
                    "auto_update: {n} restart-required config path(s) changed since boot, but the \
                     pause-and-roll could not start; retried next tick"
                );
                format!(
                    "[config restart pending: {n} path(s); the pause-and-roll could not start, \
                     retried next tick]"
                )
            }
        }
    };
    summary.note = format!("{} {addendum}", summary.note);
    let mut snap = status.snapshot();
    snap.note = Some(summary.note.clone());
    status.publish(snap);
    summary
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "tests/config_restart.rs"]
mod tests;
