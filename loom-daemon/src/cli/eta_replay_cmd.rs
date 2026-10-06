//! Replay-calibration helper for the `loom-daemon eta` commands (#10207;
//! removed with `amber-heron` in #10484, restored for `calm-plover`, #10489;
//! every calibration base since `quick-tern`, #10524).

use std::path::Path;

use anyhow::{bail, Result};
use loom_daemon::eta::backtest;
use loom_daemon::eta::history::StageSamples;
use loom_daemon::eta::walk_forward::DatedFits;
use loom_daemon::eta::Provenance;

/// Give the calibrating `land` heuristics (`land-2026-10-06-calm-plover`,
/// #10489, over `land-v2`; `land-2026-10-06-quick-tern`, #10524, over
/// `land-2026-10-04-twin-otter-b`; `land-2026-10-06-brisk-petrel`, #10528,
/// whose regime residuals are the same `-b` rows) their calibration evidence
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

/// Load the `--fit-dir` coefficient files for `eta backtest` (#10524);
/// without a directory every fitted heuristic refuses `no_model`.
pub(super) fn load_fits(fit_dir: Option<&Path>) -> Result<DatedFits> {
    let Some(dir) = fit_dir else {
        return Ok(DatedFits::new(Vec::new()));
    };
    let fits = DatedFits::load_dir(dir)
        .map_err(|e| anyhow::anyhow!("--fit-dir {}: {e}", dir.display()))?;
    if fits.is_empty() {
        bail!("--fit-dir {}: no readable eta-fit/v1 file", dir.display());
    }
    eprintln!(
        "[eta backtest] walk-forward over {} coefficient file(s), cutoffs {}",
        fits.len(),
        fits.cutoffs()
            .iter()
            .map(|c| c.to_rfc3339())
            .collect::<Vec<_>>()
            .join(", ")
    );
    Ok(fits)
}
