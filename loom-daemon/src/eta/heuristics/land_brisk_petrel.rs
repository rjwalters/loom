//! `land-2026-10-06-brisk-petrel` (#10528):
//! `land-2026-10-04-twin-otter-b`'s estimate, scaled by the latent-regime
//! residual adjustment ([`regime::serve`]) while the drift check has tripped
//! for the item's stage.
//!
//! The base is twin-otter-b's estimate **exactly**: the same path or fitted
//! model, the same seed and the same quantiles. Only the id is rewritten.
//! The residuals are twin-otter-b's own logged track record
//! ([`crate::eta::calibration_log`], [`super::CALIBRATION_BASES`]), always
//! against its *unadjusted* prediction, so the factor is a function of the
//! recent window and never feeds back on itself.
//!
//! **Drift-gated, not always on.** The offline evidence
//! (loom-experiments#19) found an always-on 6 h residual tracker on
//! twin-otter-b recovers p50 within hours of a shift but over-widens
//! intervals in calm periods. So the factor is applied only while the CUSUM
//! ([`regime::drift`]) has tripped; otherwise — and below the sample floor,
//! with no logged rows (a fresh host), or when the recent mean is noise —
//! the estimate is twin-otter-b's unchanged and carries no
//! `regime_adjustment` record.
//!
//! Ships **registered, not current**, tier `candidate` (#10525). Promotion
//! is [`crate::eta::shadow`]'s two-gate rule (#10233).

use super::{LandTwinOtterB, LAND_TWIN_OTTER_B};
use crate::eta::fit::CoefficientFile;
use crate::eta::history::StageSamples;
use crate::eta::regime;
use crate::eta::{estimate_id, EstimateInput, Explanation, Heuristic, Kind, Tier};
use std::sync::Arc;

/// The id. Immutable once shipped.
pub const LAND_BRISK_PETREL: &str = "land-2026-10-06-brisk-petrel";

/// `land-2026-10-06-brisk-petrel`.
#[derive(Debug, Clone, Default)]
pub struct LandBriskPetrel {
    base: LandTwinOtterB,
}

impl LandBriskPetrel {
    /// The heuristic over `fit` (twin-otter-b's PR-level stages).
    #[must_use]
    pub fn new(fit: Option<Arc<CoefficientFile>>) -> Self {
        LandBriskPetrel {
            base: LandTwinOtterB::new(fit),
        }
    }
}

impl Heuristic for LandBriskPetrel {
    fn id(&self) -> &'static str {
        LAND_BRISK_PETREL
    }

    fn kind(&self) -> Kind {
        Kind::Land
    }

    /// A candidate (#10525): a drift-adapted twin-otter-b, eligible for
    /// promotion through the #10233 gate, counted in the shadow budget.
    fn tier(&self) -> Tier {
        Tier::Candidate
    }

    /// As its base: twin-otter-b models an operator hold (#10218).
    fn models_hold(&self) -> bool {
        self.base.models_hold()
    }

    fn estimate(&self, input: &EstimateInput, history: &StageSamples) -> Explanation {
        let mut explanation = self.base.estimate(input, history);
        explanation.heuristic = LAND_BRISK_PETREL.to_string();
        explanation.estimate_id =
            estimate_id(&input.subject, Kind::Land, LAND_BRISK_PETREL, input.as_of);
        if explanation.result.is_none() {
            return explanation;
        }
        regime::serve(explanation, &history.calibration, LAND_TWIN_OTTER_B)
    }
}
