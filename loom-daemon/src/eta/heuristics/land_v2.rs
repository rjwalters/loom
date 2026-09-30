//! `land-v2` (#9328): `land-v1`'s path, with right-censoring handled instead
//! of ignored.
//!
//! # What changed, and only what changed
//!
//! Identical to [`super::LandV1`] in every respect — same kind, same journals,
//! same path, same always-merge terminal, same Monte Carlo — except that each
//! stage's duration grid is the **Kaplan–Meier** product-limit estimate over
//! the stage's observed durations *and* its right-censored lower bounds
//! ([`crate::eta::grid::km_grid_of`]), rather than the nearest-rank grid over
//! the observed durations alone.
//!
//! # Why that is not a cosmetic difference
//!
//! `land-v1` reads only stages that *completed*. The stages that have not
//! completed are not a random sample of the population: they are the slow
//! ones, by definition. A PR that has sat in `merge_wait` under a merge-risk
//! hold for three days contributes nothing to `land-v1`'s `merge_wait` grid,
//! while the one merged in twenty minutes contributes fully — so the grid
//! reads short exactly where it matters most, and `land` runs early.
//!
//! Kaplan–Meier is the standard answer: a censored sample contributes no
//! event, but stays *at risk* up to its lower bound, so it raises the
//! estimated survival past every later event without claiming to know when it
//! would have ended.
//!
//! # It does not get to skip the gate
//!
//! `land-v2` ships **registered, not current**. Promotion runs the same
//! two-gate rule as any other candidate ([`crate::eta::shadow`]): it must beat
//! `land-v1` on the phase-2 backtest first, then on ≥50 live paired
//! observations with coverage inside `[40%, 60%]`. Until #9579 gives the
//! `land` kind any backtest cases at all, the backtest gate has nothing to
//! rule on and the switch correctly refuses to flip `current.land` — that is
//! the mechanism working, not a defect here.

use super::{estimate_path, PathRules};
use crate::eta::history::{SampleSource, StageSamples};
use crate::eta::{EstimateInput, Explanation, Heuristic, Kind};

/// The id. Immutable once shipped.
pub const LAND_V2: &str = "land-v2";

/// `land-v2`.
#[derive(Debug, Clone, Copy)]
pub struct LandV2;

impl Heuristic for LandV2 {
    fn id(&self) -> &'static str {
        LAND_V2
    }

    fn kind(&self) -> Kind {
        Kind::Land
    }

    fn estimate(&self, input: &EstimateInput, history: &StageSamples) -> Explanation {
        estimate_path(
            PathRules {
                id: LAND_V2,
                kind: Kind::Land,
                sources: &[SampleSource::SweepOutcome, SampleSource::StageJournal],
                always_merge: true,
                censoring: true,
            },
            input,
            history,
        )
    }
}
