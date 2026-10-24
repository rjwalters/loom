//! Why a host below the fleet floor is not rolling, and the unsupervised-host
//! re-download guard (Issue #11042, follow-up to #10954).
//!
//! **The re-download loop.** A roll is started only after the release is
//! fetched and installed, and the pause-and-roll refuses to start on a host
//! with no launchd/systemd supervisor (H7 `unsupervised`): nothing would
//! relaunch the daemon. Before #11042 nothing remembered that refusal. The
//! next tick compared the release with the *running* version again, found it
//! newer again, and fetched it again — once per `intervalSecs` (about 96
//! downloads a day at 900 s). #10954 made that reachable on every fleet host
//! below its floor, autoUpdate on or off.
//!
//! The fix is a per-target backoff ([`UnsupervisedStage`]): once a fetch (or
//! rebuild) of a target has succeeded on an unsupervised host and the roll was
//! refused, later ticks for the **same** target do not fetch it again. The
//! release was installed by that first fetch, so a manual restart runs it, as
//! the refusal says. A new target (a newer release, a moved floor) is fetched
//! once, the same way. Supervision is read from the environment
//! (`LOOM_DAEMON_SUPERVISOR`), which is fixed for the life of the process, so
//! the hold never has to be lifted while the process runs.
//!
//! Nothing else changes: the floor stays a lower bound, dispatch continues,
//! and a fetch still verifies the signature and checksum it always did.
//!
//! **The typed reason.** [`classify`] names, as a [`FloorNotRolling`], why a
//! host below the floor did not roll on a tick. It is carried on the
//! `auto_update.tick` record and in `status` (`auto_update.floor_not_rolling`).

use super::floor_roll::FloorVerdict;
use super::{AutoUpdateState, RebuildOutcome, RollTrigger, TickDecision};
pub use crate::telemetry::kinds::auto_update_tick::FloorNotRolling;
use crate::telemetry::kinds::auto_update_tick::TickDecisionKind;

/// The target an unsupervised host has already installed and could not roll
/// to (see the module doc).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UnsupervisedStage {
    staged: Option<String>,
}

impl UnsupervisedStage {
    /// Whether `tracked` is the target already installed and refused.
    #[must_use]
    pub fn holds(&self, tracked: Option<&str>) -> bool {
        tracked.is_some() && self.staged.as_deref() == tracked
    }

    /// Record the outcome of a fetch or rebuild of `tracked`: a success whose
    /// roll was refused on an unsupervised host stages it.
    pub fn record(
        &mut self,
        tracked: Option<&str>,
        outcome: &RebuildOutcome,
        roll_accepted: bool,
        unsupervised: bool,
    ) {
        if matches!(outcome, RebuildOutcome::Success) && !roll_accepted && unsupervised {
            self.staged = tracked.map(str::to_string);
        }
    }
}

/// Turn a fetch or rebuild decision into a skip when this host is
/// unsupervised and the target was already installed and refused. Every other
/// decision passes through unchanged.
pub(super) fn gate<T: RollTrigger>(
    state: &AutoUpdateState,
    trigger: &T,
    decision: TickDecision,
) -> TickDecision {
    if !matches!(decision, TickDecision::FetchArtifact { .. } | TickDecision::Rebuild { .. }) {
        return decision;
    }
    let tracked = state.tracked_target.as_deref();
    match trigger.unsupervised() {
        Some(why) if state.unsupervised.holds(tracked) => TickDecision::Skip(format!(
            "{} is already installed, but {why}, so the roll cannot start — not fetching it \
             again; restart the daemon manually to run it",
            tracked.unwrap_or("the target")
        )),
        _ => decision,
    }
}

/// What one tick saw, for [`classify`].
#[derive(Debug, Clone, Copy)]
pub(super) struct TickFacts<'a> {
    pub verdict: &'a FloorVerdict,
    pub decision: TickDecisionKind,
    pub outcome: Option<&'a str>,
    pub roll_armed: bool,
    pub terminal: bool,
    pub backing_off: bool,
    pub unsupervised: bool,
}

/// Why a host below the fleet floor did not roll on this tick, or `None` when
/// it is not below the floor or a roll is under way.
#[must_use]
pub(super) fn classify(facts: &TickFacts<'_>) -> Option<FloorNotRolling> {
    match facts.verdict {
        FloorVerdict::Unsatisfiable(_) => return Some(FloorNotRolling::Unsatisfiable),
        FloorVerdict::Unresolved { .. } => return Some(FloorNotRolling::NoRelease),
        FloorVerdict::Below { .. } => {}
        _ => return None,
    }
    if facts.roll_armed || facts.decision == TickDecisionKind::DrainWait {
        return None;
    }
    Some(match facts.outcome {
        Some("terminal") => FloorNotRolling::Terminal,
        Some("retryable") => FloorNotRolling::Backoff,
        _ if facts.terminal => FloorNotRolling::Terminal,
        _ if facts.backing_off => FloorNotRolling::Backoff,
        _ if facts.unsupervised => FloorNotRolling::Unsupervised,
        _ => FloorNotRolling::RollRefused,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auto_update::floor_roll::Release;

    fn below() -> FloorVerdict {
        FloorVerdict::Below {
            floor: "0.19.850".to_string(),
            target: Release {
                tag: "v0.19.900".to_string(),
                version: "0.19.900".to_string(),
            },
        }
    }

    fn facts(verdict: &FloorVerdict) -> TickFacts<'_> {
        TickFacts {
            verdict,
            decision: TickDecisionKind::Fetch,
            outcome: Some("success"),
            roll_armed: false,
            terminal: false,
            backing_off: false,
            unsupervised: false,
        }
    }

    #[test]
    fn a_stage_holds_only_its_own_target_and_only_after_an_unsupervised_refusal() {
        let mut stage = UnsupervisedStage::default();
        assert!(!stage.holds(Some("artifact:0.19.900:aa")));
        // Supervised, or accepted, or failed: nothing is staged.
        stage.record(Some("artifact:0.19.900:aa"), &RebuildOutcome::Success, false, false);
        stage.record(Some("artifact:0.19.900:aa"), &RebuildOutcome::Success, true, true);
        let failed = RebuildOutcome::Retryable("net".to_string());
        stage.record(Some("artifact:0.19.900:aa"), &failed, false, true);
        assert!(!stage.holds(Some("artifact:0.19.900:aa")));
        stage.record(Some("artifact:0.19.900:aa"), &RebuildOutcome::Success, false, true);
        assert!(stage.holds(Some("artifact:0.19.900:aa")));
        assert!(!stage.holds(Some("artifact:0.19.901:bb")), "a new target is fetched once");
        assert!(!stage.holds(None));
    }

    #[test]
    fn only_a_host_below_the_floor_and_not_rolling_gets_a_reason() {
        for verdict in [
            FloorVerdict::NoStore,
            FloorVerdict::Satisfied {
                floor: "0.19.700".to_string(),
            },
        ] {
            assert_eq!(classify(&facts(&verdict)), None, "{verdict:?}");
        }
        let below = below();
        let armed = TickFacts {
            roll_armed: true,
            ..facts(&below)
        };
        assert_eq!(classify(&armed), None);
        let waiting = TickFacts {
            decision: TickDecisionKind::DrainWait,
            outcome: None,
            ..facts(&below)
        };
        assert_eq!(classify(&waiting), None);
        let unresolved = FloorVerdict::Unresolved {
            floor: "0.19.850".to_string(),
            unparsed: None,
        };
        assert_eq!(classify(&facts(&unresolved)), Some(FloorNotRolling::NoRelease));
    }

    #[test]
    fn a_failure_outranks_supervision_and_a_refusal_is_named() {
        let below = below();
        let unsupervised = TickFacts {
            unsupervised: true,
            ..facts(&below)
        };
        assert_eq!(classify(&unsupervised), Some(FloorNotRolling::Unsupervised));
        let failed = TickFacts {
            outcome: Some("retryable"),
            ..unsupervised
        };
        assert_eq!(classify(&failed), Some(FloorNotRolling::Backoff));
        let held = TickFacts {
            decision: TickDecisionKind::Defer,
            outcome: None,
            terminal: true,
            ..unsupervised
        };
        assert_eq!(classify(&held), Some(FloorNotRolling::Terminal));
        assert_eq!(classify(&facts(&below)), Some(FloorNotRolling::RollRefused));
    }
}
