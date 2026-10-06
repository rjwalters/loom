//! `land-2026-10-06-bold-lark` (#10524, slice 4):
//! `land-2026-10-06-keen-wren`'s priority-aware estimate (#10508), with each
//! reported quantile calibrated by IPCW split-conformal over a short recent
//! window ([`conformal_ipcw`]).
//!
//! This is the wrapper's second base, after twin-otter-b (quick-tern). The
//! base is keen-wren's estimate **exactly**: same path or `eta-fit/v2`
//! model, same seed, same quantiles; only the id is rewritten. The base
//! quantiles the wrapper adjusts are the ones logged as keen-wren's track
//! record ([`super::CALIBRATION_BASES`]), so a calibration score compares
//! like with like, and the calibrator filters on that base id so it never
//! mixes rows with another base's.
//!
//! Degrades to keen-wren, never fabricates: with too few effective landings
//! at every half-life, or no logged keen-wren rows yet, the estimate is
//! keen-wren's unchanged and carries no `calibration` record.
//!
//! Ships **registered, not current**, tier `candidate` (#10525). Promotion
//! is [`crate::eta::shadow`]'s two-gate rule (#10233).

use super::{LandKeenWren, LAND_KEEN_WREN};
use crate::eta::conformal_ipcw;
use crate::eta::fit::CoefficientFile;
use crate::eta::history::StageSamples;
use crate::eta::{estimate_id, EstimateInput, Explanation, Heuristic, Kind, Tier};
use std::sync::Arc;

/// The id. Immutable once shipped.
pub const LAND_BOLD_LARK: &str = "land-2026-10-06-bold-lark";

/// `land-2026-10-06-bold-lark`.
#[derive(Debug, Clone, Default)]
pub struct LandBoldLark {
    base: LandKeenWren,
}

impl LandBoldLark {
    /// The heuristic over `fit_v2`, keen-wren's `eta-fit/v2` file.
    #[must_use]
    pub fn new(fit_v2: Option<Arc<CoefficientFile>>) -> Self {
        LandBoldLark {
            base: LandKeenWren::new(fit_v2),
        }
    }
}

impl Heuristic for LandBoldLark {
    fn id(&self) -> &'static str {
        LAND_BOLD_LARK
    }

    fn kind(&self) -> Kind {
        Kind::Land
    }

    fn tier(&self) -> Tier {
        Tier::Candidate
    }

    /// As its base.
    fn models_hold(&self) -> bool {
        self.base.models_hold()
    }

    fn estimate(&self, input: &EstimateInput, history: &StageSamples) -> Explanation {
        let mut explanation = self.base.estimate(input, history);
        explanation.heuristic = LAND_BOLD_LARK.to_string();
        explanation.estimate_id =
            estimate_id(&input.subject, Kind::Land, LAND_BOLD_LARK, input.as_of);
        if explanation.result.is_none() {
            return explanation;
        }
        conformal_ipcw::calibrate(explanation, &history.calibration, LAND_KEEN_WREN)
    }
}
