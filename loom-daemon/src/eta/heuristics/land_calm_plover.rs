//! `land-2026-10-06-calm-plover` (#10489): `land-v2`'s path with each
//! reported quantile conformally calibrated against `land-v2`'s own landed
//! **and still-open** track record.
//!
//! The simulation is `land-v2`'s exactly (same journals, same Kaplan–Meier
//! grids, same always-merge path; its own seed, keyed on this id). Then
//! [`conformal::calibrate`] moves p25/p50/p75/p90 so each aims at its own
//! hit rate over a trailing window, per (stage, age bucket), with the
//! day-over-day change of the shift rate-limited. Method, point-in-time
//! discipline and why it differs from `amber-heron`: [`crate::eta::conformal`]
//! and `defaults/docs/eta.md`.
//!
//! The wrapper is generic over its base through [`conformal::calibrate`]
//! (any explanation + the base's observations); this heuristic fixes the base
//! at `land-v2`, which is what the track record is logged for
//! ([`crate::eta::calibration_log`]).
//!
//! Degrades to `land-v2`, never fabricates: with no cell holding
//! [`conformal::MIN_CELL_EVENTS`] landings — a fresh host, or a log written
//! before observations carried the base quantiles — the estimate is the base
//! simulation unchanged and carries no `calibration` record.
//!
//! Ships **registered, not current**: promotion is
//! [`crate::eta::shadow`]'s two-gate rule (#10233).

use super::{estimate_path, PathRules, LAND_V2};
use crate::eta::conformal;
use crate::eta::history::{SampleSource, StageSamples};
use crate::eta::{EstimateInput, Explanation, Heuristic, Kind};

/// The id. Immutable once shipped.
pub const LAND_CALM_PLOVER: &str = "land-2026-10-06-calm-plover";

/// `land-2026-10-06-calm-plover`.
#[derive(Debug, Clone, Copy)]
pub struct LandCalmPlover;

impl Heuristic for LandCalmPlover {
    fn id(&self) -> &'static str {
        LAND_CALM_PLOVER
    }

    fn kind(&self) -> Kind {
        Kind::Land
    }

    fn estimate(&self, input: &EstimateInput, history: &StageSamples) -> Explanation {
        let base = estimate_path(
            PathRules {
                id: LAND_CALM_PLOVER,
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
        conformal::calibrate(base, &history.calibration, LAND_V2)
    }
}
