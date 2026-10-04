//! `land-2026-10-04-amber-heron` (#10207): `land-v2`'s path, with its
//! interval recalibrated from `land-v2`'s own track record.
//!
//! # What changed, and only what changed
//!
//! The simulation is `land-v2`'s exactly — same journals, same Kaplan–Meier
//! grids, same always-merge path, same Monte Carlo (its own seed, keyed on
//! this id like every heuristic's). Then [`recalibrate`] moves the interval:
//! the current stage's distribution of `ln(actual_remaining / p50)` over
//! `land-v2`'s past `land` estimates — landed ones as events, still-open ones
//! as right-censored lower bounds — is fitted **as of this estimate's own
//! `as_of`** ([`fit_table`]), and
//! `p_τ = p50 × exp(Q(τ) − Q(0.5))` replaces p25 and p75. The median is
//! kept ([`Mode::SpreadOnly`]): `land-v2`'s median is roughly calibrated, its
//! interval is not (#9970, #10193).
//!
//! # Pure, and leak-free
//!
//! The observations arrive as data in [`StageSamples::calibration`]; this
//! heuristic reads nothing else. Fitting at the estimate's own `as_of` means
//! a backtest replay can never see an outcome that was not yet known then.
//!
//! # Degrades to `land-v2`, never fabricates
//!
//! A stage with fewer than [`crate::eta::recalibrate::MIN_STAGE_EVENTS`]
//! landings uses the pooled distribution; with too few pooled landings (an
//! empty table — a fresh host, or `eta view` with no outcome log) the
//! estimate is the base simulation unchanged and carries no
//! `recalibration` record.
//!
//! # Naming, and the gate
//!
//! Named by the operator's convention for a shipped heuristic — datestamp
//! plus two words, not `land-v4`. Ships **registered, not current**:
//! promotion is [`crate::eta::shadow`]'s two-gate rule.

use super::{estimate_path, PathRules, LAND_V2};
use crate::eta::history::{SampleSource, StageSamples};
use crate::eta::recalibrate::{fit_table, recalibrate, Mode, Weighting};
use crate::eta::{EstimateInput, Explanation, Heuristic, Kind};

/// The id. Immutable once shipped.
pub const LAND_AMBER_HERON: &str = "land-2026-10-04-amber-heron";

/// The heuristic whose track record the interval is recalibrated from.
pub const CALIBRATION_BASE: &str = LAND_V2;

/// `land-2026-10-04-amber-heron`.
#[derive(Debug, Clone, Copy)]
pub struct LandAmberHeron;

impl Heuristic for LandAmberHeron {
    fn id(&self) -> &'static str {
        LAND_AMBER_HERON
    }

    fn kind(&self) -> Kind {
        Kind::Land
    }

    fn estimate(&self, input: &EstimateInput, history: &StageSamples) -> Explanation {
        let base = estimate_path(
            PathRules {
                id: LAND_AMBER_HERON,
                kind: Kind::Land,
                sources: &[SampleSource::SweepOutcome, SampleSource::StageJournal],
                always_merge: true,
                censoring: true,
                adjust: None,
            },
            input,
            history,
        );
        if base.result.is_none() {
            return base;
        }
        let table =
            fit_table(&history.calibration, CALIBRATION_BASE, input.as_of, Weighting::default());
        recalibrate(base, &table, CALIBRATION_BASE, Mode::SpreadOnly)
    }
}
