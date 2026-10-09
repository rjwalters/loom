//! `--fit-dir` loading for `loom-daemon eta backtest` (#10524). The
//! replay-calibration helper it used to hold is the lib's
//! `eta::backtest::with_replay_calibration` (#10492), shared with the
//! nightly fold.

use std::path::Path;

use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use loom_daemon::eta::backtest;
use loom_daemon::eta::conformal_wrap::{self, Calibrator, IpcwWrap};
use loom_daemon::eta::history::StageSamples;
use loom_daemon::eta::walk_forward::{DatedFits, WalkForward};
use loom_daemon::eta::{Heuristic, Provenance};

/// `eta backtest --wrap NAME` (#10524, slice 5): `base` IPCW-wrapped
/// ([`IpcwWrap`]) as `<base>+<NAME>`, with `base`'s own replayed estimates
/// added to `history.calibration` as its evidence (leak-free, as
/// [`with_replay_calibration`]; skipped for a base that already has them).
/// Without `--wrap`, `base` unchanged.
///
/// # Errors
///
/// An unknown calibrator name, a base that is not `land`, or a base that
/// already calibrates itself ([`conformal_wrap::CALIBRATED`]).
pub(super) fn wrap_for_backtest<'a>(
    base: WalkForward<'a>,
    name: Option<&str>,
    history: &mut StageSamples,
    cases: &[backtest::ReplayCase],
    loom: &Provenance,
) -> Result<Box<dyn Heuristic + 'a>> {
    let Some(name) = name else {
        return Ok(Box::new(base));
    };
    let Some(calibrator) = Calibrator::parse(name) else {
        let known: Vec<_> = Calibrator::ALL.iter().map(|c| c.name()).collect();
        bail!("unknown --wrap {name:?} (known: {})", known.join(", "));
    };
    let base_id = base.id();
    if conformal_wrap::CALIBRATED.contains(&base_id) {
        bail!("--wrap: {base_id} already calibrates its own estimate; wrap its base instead");
    }
    if !loom_daemon::eta::heuristics::CALIBRATION_BASES.contains(&base_id) {
        let replayed = backtest::calibration_from_replay(&base, history, cases, loom);
        history.calibration.extend(replayed);
    }
    // One small leaked string per one-shot CLI run: `IpcwWrap` needs a `&'static` id.
    let id: &'static str = Box::leak(calibrator.wrapped_id(base_id).into_boxed_str());
    match IpcwWrap::new(id, base, calibrator) {
        Some(wrapped) => Ok(Box::new(wrapped)),
        None => bail!("--wrap: {base_id} does not predict `land`; only a land base is calibrated"),
    }
}

/// Parse `eta backtest --since` (RFC 3339) into a UTC instant.
pub(super) fn parse_since(raw: Option<&str>) -> Result<Option<DateTime<Utc>>> {
    raw.map(|raw| {
        DateTime::parse_from_rfc3339(raw)
            .map(|dt| dt.with_timezone(&Utc))
            .map_err(|e| anyhow::anyhow!("invalid --since {raw:?}: {e}"))
    })
    .transpose()
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
        bail!("--fit-dir {}: no readable eta-fit file", dir.display());
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
