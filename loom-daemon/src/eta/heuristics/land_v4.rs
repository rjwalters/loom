//! `land-v4` (#10210): `land-v3`, with stopped service modelled explicitly
//! and no `beyond_history` refusal.
//!
//! # Why
//!
//! The worst `land` misses are **stopped** service, not slow service: a spent
//! forge quota, a cooling rate-limit breaker, an exhausted token pool, an
//! operator hold, a repo frozen behind the open-PR guard. `land-v1`..`v3`
//! either keep predicting a normal duration straight through a stall, or —
//! once the item outlives its stage history — refuse with `beyond_history`.
//! On one dispatch worker that refusal was ~41% of every live `land` estimate
//! (2026-10-04, the pending journal), and the refusals hide exactly the slow
//! tail from the accuracy numbers.
//!
//! # What changed, and only what changed
//!
//! Identical to [`super::LandV3`] — same journals, Kaplan–Meier grids,
//! calibration, always-merge path and Monte Carlo — except:
//!
//! 1. **Stall term** ([`crate::eta::stall`]): the binding stall's term
//!    (`resume_at − as_of` when the resume instant is known, the cause's
//!    documented default otherwise) is added to every simulated path, so the
//!    estimate is `term + normal`, and `stalled.applied` is `true`. The draw
//!    stream does not change, so with a known `resume_at = T` every quantile
//!    is exactly `(T − as_of)` above the unstalled one.
//! 2. **Operator holds stop refusing**: a PR whose every hold is an operator
//!    hold is estimated from the stage its review labels name underneath the
//!    hold, plus the `operator_hold` stall term (a fixed default until the
//!    hold-release model of #10218 replaces it).
//! 3. **No `beyond_history` refusal**: an item older than all but
//!    [`crate::eta::MIN_COND`] of its stage's samples is answered with the
//!    residual-life tail of its own age
//!    ([`crate::eta::explanation::CONDITIONING_RESIDUAL_LIFE`]: median
//!    remaining in-stage time = the age), flagged
//!    `result.tail_extrapolated` so scoring and the backtest keep it apart.
//!
//! Genuinely unknown stages still refuse exactly as before
//! (`unknown_stage`, `insufficient_samples`, `no_dispatch_plan`, …).
//!
//! # It does not get to skip the gate
//!
//! `land-v4` ships **registered, not current**, like every `land` candidate
//! before it: promotion is [`crate::eta::shadow`]'s two-gate rule.

use super::land_v3::{adjust, with_missing_inputs_noted};
use super::{estimate_path, PathRules};
use crate::eta::history::{SampleSource, StageSamples};
use crate::eta::{EstimateInput, Explanation, Heuristic, Kind};

/// The id. Immutable once shipped.
pub const LAND_V4: &str = "land-v4";

/// `land-v4`.
#[derive(Debug, Clone, Copy)]
pub struct LandV4;

impl Heuristic for LandV4 {
    fn id(&self) -> &'static str {
        LAND_V4
    }

    fn kind(&self) -> Kind {
        Kind::Land
    }

    fn estimate(&self, input: &EstimateInput, history: &StageSamples) -> Explanation {
        let input = with_missing_inputs_noted(input);
        estimate_path(
            PathRules {
                id: LAND_V4,
                kind: Kind::Land,
                sources: &[SampleSource::SweepOutcome, SampleSource::StageJournal],
                always_merge: true,
                censoring: true,
                adjust: Some(adjust),
                models_hold: false,
                half_life_sec: None,
                stall_term: true,
                residual_tail: true,
            },
            &input,
            history,
        )
    }
}
