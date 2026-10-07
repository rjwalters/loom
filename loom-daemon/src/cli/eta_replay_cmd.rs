//! `--fit-dir` loading for `loom-daemon eta backtest` (#10524). The
//! replay-calibration helper it used to hold is the lib's
//! `eta::backtest::with_replay_calibration` (#10492), shared with the
//! nightly fold.

use std::path::Path;

use anyhow::{bail, Result};
use loom_daemon::eta::walk_forward::DatedFits;

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
