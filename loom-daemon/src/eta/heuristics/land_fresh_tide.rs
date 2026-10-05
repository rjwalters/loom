//! `land-2026-10-04-fresh-tide` (#10209): `land-v2`'s path, with each stage's
//! samples weighted by recency instead of flat across the window.
//!
//! # What changed, and only what changed
//!
//! Identical to [`super::LandV2`] — same journals, same window, same
//! repo-then-host floor, same Kaplan–Meier treatment of censored samples, same
//! always-merge path, same Monte Carlo (its own seed, keyed on this id like
//! every heuristic's) — except that every observed and censored sample of a
//! stage is weighted `exp(−age / half_life)` and the stage grid is the
//! **weighted** product-limit (or, with nothing censored, weighted
//! nearest-rank) grid ([`crate::eta::grid::weighted_grid_of`]).
//!
//! # Why
//!
//! The planner, role cadence, token pool and CI all change in place, so a
//! 60-day flat marginal mostly describes a system that no longer exists:
//! lookup models trained on ~1.3 days of recent history beat `land-v2` on
//! pinball loss by 21–26% in experiment v0 (#10193). A hard short window
//! did *not* (experiment v2: long-tailed stages need long follow-up), so this
//! keeps the whole window and lets age discount it, with the effective-N
//! fallback ([`crate::eta::recency`]) widening the half-life whenever the
//! discount would leave too little evidence.
//!
//! Each stage records the half-life actually used and the effective N in
//! `distribution.half_life_sec` / `distribution.effective_n`.
//!
//! # The half-life is a parameter
//!
//! The registered instance uses [`DEFAULT_HALF_LIFE_SEC`] (two days).
//! [`LandFreshTide::with_half_life`] builds the same heuristic at another
//! half-life, so a backtest can compare 1, 2 and 7 days without registering
//! three ids; every such estimate still names its half-life per stage.
//!
//! # Naming, and the gate
//!
//! Named by the operator's convention for a shipped heuristic — datestamp
//! plus two words. Ships **registered, not current**: promotion is
//! [`crate::eta::shadow`]'s two-gate rule, and it may legitimately lose.

use super::{estimate_path, PathRules};
use crate::eta::history::{SampleSource, StageSamples};
use crate::eta::{EstimateInput, Explanation, Heuristic, Kind};

pub use crate::eta::recency::DEFAULT_HALF_LIFE_SEC;

/// The id. Immutable once shipped.
pub const LAND_FRESH_TIDE: &str = "land-2026-10-04-fresh-tide";

/// `land-2026-10-04-fresh-tide`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LandFreshTide {
    half_life_sec: i64,
}

impl Default for LandFreshTide {
    fn default() -> Self {
        LandFreshTide {
            half_life_sec: DEFAULT_HALF_LIFE_SEC,
        }
    }
}

impl LandFreshTide {
    /// The heuristic at base half-life `half_life_sec` (a non-positive one
    /// weighs flat). Only [`Self::default`] is registered.
    #[must_use]
    pub fn with_half_life(half_life_sec: i64) -> Self {
        LandFreshTide { half_life_sec }
    }

    /// The base half-life, before any effective-N widening.
    #[must_use]
    pub fn half_life_sec(&self) -> i64 {
        self.half_life_sec
    }
}

impl Heuristic for LandFreshTide {
    fn id(&self) -> &'static str {
        LAND_FRESH_TIDE
    }

    fn kind(&self) -> Kind {
        Kind::Land
    }

    fn estimate(&self, input: &EstimateInput, history: &StageSamples) -> Explanation {
        estimate_path(
            PathRules {
                id: LAND_FRESH_TIDE,
                kind: Kind::Land,
                sources: &[SampleSource::SweepOutcome, SampleSource::StageJournal],
                always_merge: true,
                censoring: true,
                adjust: None,
                models_hold: false,
                half_life_sec: Some(self.half_life_sec),
                stall_term: false,
                residual_tail: false,
            },
            input,
            history,
        )
    }
}
