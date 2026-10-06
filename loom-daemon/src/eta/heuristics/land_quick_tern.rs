//! `land-2026-10-06-quick-tern` (#10524):
//! `land-2026-10-04-twin-otter-b`'s estimate, with each reported quantile
//! calibrated by IPCW split-conformal over a short recent window
//! ([`conformal_ipcw`]).
//!
//! The base is twin-otter-b's estimate **exactly**: the same path or fitted
//! model, the same seed and the same quantiles. Only the id is rewritten.
//! The base quantiles the wrapper adjusts are therefore the ones logged as
//! twin-otter-b's track record ([`crate::eta::calibration_log`],
//! [`super::CALIBRATION_BASES`]), so a calibration score compares like with like.
//!
//! It differs from `land-2026-10-06-calm-plover` (#10489) in three ways:
//! - the base is twin-otter-b, not `land-v2`;
//! - censoring is corrected by inverse-probability weighting on the time
//!   axis, not by Kaplan–Meier on the score axis;
//! - the window is hours (6 h half-life), not 14 days rate-limited to
//!   `ln 1.2` a day, so it follows a regime shift within hours (#10528).
//!
//! Degrades to twin-otter-b, never fabricates. With too few effective
//! landings at every half-life, or no logged twin-otter-b rows yet (a fresh
//! host), the estimate is twin-otter-b's unchanged and carries no
//! `calibration` record.
//!
//! Ships **registered, not current**, tier `candidate` (#10525). Promotion
//! is [`crate::eta::shadow`]'s two-gate rule (#10233).

use super::{LandTwinOtterB, LAND_TWIN_OTTER_B};
use crate::eta::conformal_ipcw;
use crate::eta::fit::CoefficientFile;
use crate::eta::history::StageSamples;
use crate::eta::{estimate_id, EstimateInput, Explanation, Heuristic, Kind, Tier};
use std::sync::Arc;

/// The id. Immutable once shipped.
pub const LAND_QUICK_TERN: &str = "land-2026-10-06-quick-tern";

/// `land-2026-10-06-quick-tern`.
#[derive(Debug, Clone, Default)]
pub struct LandQuickTern {
    base: LandTwinOtterB,
}

impl LandQuickTern {
    /// The heuristic over `fit` (twin-otter-b's PR-level stages).
    #[must_use]
    pub fn new(fit: Option<Arc<CoefficientFile>>) -> Self {
        LandQuickTern {
            base: LandTwinOtterB::new(fit),
        }
    }
}

impl Heuristic for LandQuickTern {
    fn id(&self) -> &'static str {
        LAND_QUICK_TERN
    }

    fn kind(&self) -> Kind {
        Kind::Land
    }

    /// A candidate (#10525): a calibrated twin-otter-b, eligible for
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
        explanation.heuristic = LAND_QUICK_TERN.to_string();
        explanation.estimate_id =
            estimate_id(&input.subject, Kind::Land, LAND_QUICK_TERN, input.as_of);
        if explanation.result.is_none() {
            return explanation;
        }
        conformal_ipcw::calibrate(explanation, &history.calibration, LAND_TWIN_OTTER_B)
    }
}
