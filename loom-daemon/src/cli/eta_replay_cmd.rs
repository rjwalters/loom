//! Replay-calibration helper for the `loom-daemon eta` commands (#10207).

use loom_daemon::eta::backtest;
use loom_daemon::eta::history::StageSamples;
use loom_daemon::eta::{Provenance, Registry};

/// Give the recalibrating `land` heuristic (#10207) its calibration evidence
/// from the replay itself: the base heuristic's estimate at every `land` case,
/// landing at the case's own outcome. Leak-free — the recalibrating heuristic
/// refits at each case's `as_of` ([`backtest::calibration_from_replay`]) — and
/// inert for every other heuristic, which never reads `calibration`.
pub(super) fn with_replay_calibration(
    registry: &Registry,
    history: &mut StageSamples,
    cases: &[backtest::ReplayCase],
    loom: &Provenance,
) {
    if let Some(base) = registry.get(loom_daemon::eta::heuristics::CALIBRATION_BASE) {
        let replayed = backtest::calibration_from_replay(base, history, cases, loom);
        history.calibration.extend(replayed);
    }
}
