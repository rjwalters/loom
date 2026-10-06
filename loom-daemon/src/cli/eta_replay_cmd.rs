//! Replay-calibration helper for the `loom-daemon eta` commands (#10207;
//! removed with `amber-heron` in #10484, restored for `calm-plover`, #10489;
//! every calibration base since `quick-tern`, #10524).

use loom_daemon::eta::backtest;
use loom_daemon::eta::history::StageSamples;
use loom_daemon::eta::walk_forward::DatedFits;
use loom_daemon::eta::Provenance;

/// Give the calibrating `land` heuristics (`land-2026-10-06-calm-plover`,
/// #10489, over `land-v2`; `land-2026-10-06-quick-tern`, #10524, over
/// `land-2026-10-04-twin-otter-b`) their calibration evidence
/// from the replay itself: each base heuristic's estimate at every `land` case,
/// landing at the case's own outcome. Leak-free — the calibrating heuristic
/// refits at each case's `as_of` ([`backtest::calibration_from_replay`]) — and
/// inert for every other heuristic, which never reads `calibration`.
///
/// A fitted base (twin-otter-b) is replayed walk-forward over `fits`
/// ([`DatedFits`]), so its logged quantiles are the ones the wrapper adjusts.
pub(super) fn with_replay_calibration(
    fits: &DatedFits,
    history: &mut StageSamples,
    cases: &[backtest::ReplayCase],
    loom: &Provenance,
) {
    // Every base is replayed over the same pre-calibration history: a
    // base never reads `calibration`, so the order does not matter.
    let mut replayed = Vec::new();
    for id in loom_daemon::eta::heuristics::CALIBRATION_BASES {
        if let Some(base) = fits.heuristic(id) {
            replayed.extend(backtest::calibration_from_replay(&base, history, cases, loom));
        }
    }
    history.calibration.extend(replayed);
}
