//! `land-2026-10-06-even-lark` (#10489): `land-v2`'s path with each reported
//! quantile conformally calibrated against `land-v2`'s own landed **and
//! still-open** track record, with the shift **added in seconds**.
//!
//! It replaces `land-2026-10-06-calm-plover` (retired) and is that heuristic
//! with one change: the conformity score
//! is `actual − q_τ` ([`conformal::Scale::Seconds`]) instead of
//! `ln(actual / q_τ)`, so the adjusted quantile is `q_τ + c_τ` rather than
//! `q_τ · exp(c_τ)`, and the day-over-day rate limit is
//! [`conformal::MAX_DAILY_STEP_SEC`]. Cells, censoring (Kaplan–Meier with
//! still-open estimates as lower bounds), the trailing window, the replayed
//! rate limit and the point-in-time rule are calm-plover's. Why the scale
//! matters (calm-plover hits every rate but loses on pinball):
//! [`crate::eta::conformal`] and `defaults/docs/eta.md`.
//!
//! The simulation is `land-v2`'s exactly (same journals, same Kaplan–Meier
//! grids, same always-merge path; its own seed, keyed on this id). Generic
//! over its base through [`conformal::calibrate_on`]; this id fixes the base
//! at `land-v2` ([`super::CALIBRATION_BASE`]).
//!
//! Degrades to `land-v2`, never fabricates: with no cell holding
//! [`conformal::MIN_CELL_EVENTS`] landings the estimate is the base
//! simulation unchanged and carries no `calibration` record.
//!
//! Ships **registered, not current**: promotion is
//! [`crate::eta::shadow`]'s two-gate rule (#10233).

use super::{estimate_path, PathRules, LAND_V2};
use crate::eta::conformal::{self, Scale};
use crate::eta::history::{SampleSource, StageSamples};
use crate::eta::{EstimateInput, Explanation, Heuristic, Kind};

/// The id. Immutable once shipped.
pub const LAND_EVEN_LARK: &str = "land-2026-10-06-even-lark";

/// `land-2026-10-06-even-lark`.
#[derive(Debug, Clone, Copy)]
pub struct LandEvenLark;

impl Heuristic for LandEvenLark {
    fn id(&self) -> &'static str {
        LAND_EVEN_LARK
    }

    fn kind(&self) -> Kind {
        Kind::Land
    }

    fn estimate(&self, input: &EstimateInput, history: &StageSamples) -> Explanation {
        let base = estimate_path(
            PathRules {
                id: LAND_EVEN_LARK,
                kind: Kind::Land,
                sources: &[SampleSource::SweepOutcome, SampleSource::StageJournal],
                always_merge: true,
                censoring: true,
                adjust: None,
                models_hold: false,
                half_life_sec: None,
                stall_term: false,
                residual_tail: false,
            },
            input,
            history,
        );
        if base.result.is_none() {
            return base;
        }
        conformal::calibrate_on(Scale::Seconds, base, &history.calibration, LAND_V2)
    }
}
