//! `land-2026-10-06-swift-tern` (#10524, slice 3):
//! `land-2026-10-06-quick-tern` made **drift-aware** with the #10528 drift
//! check ([`conformal_ipcw::calibrate_drift_aware`]).
//!
//! The base is twin-otter-b's estimate **exactly**, as for quick-tern, and
//! the calibration is quick-tern's IPCW split-conformal. One thing is added.
//! When the stage's CUSUM drift check trips against the shift quick-tern
//! would serve, the recency ladder restarts from a quarter of quick-tern's
//! 6 h half-life (still floored by effective N), so the old regime is
//! forgotten faster. The interval inflation the check would ask for is
//! recorded, not applied: on the fixtures it over-covers (see
//! [`conformal_ipcw::calibrate_drift_aware`]).
//!
//! Otherwise its answer is quick-tern's. Quick-tern itself is unchanged: ids
//! are immutable, so the behaviour change is this new id beside it.
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
pub const LAND_SWIFT_TERN: &str = "land-2026-10-06-swift-tern";

/// `land-2026-10-06-swift-tern`.
#[derive(Debug, Clone, Default)]
pub struct LandSwiftTern {
    base: LandTwinOtterB,
}

impl LandSwiftTern {
    /// The heuristic over `fit` (twin-otter-b's PR-level stages).
    #[must_use]
    pub fn new(fit: Option<Arc<CoefficientFile>>) -> Self {
        LandSwiftTern {
            base: LandTwinOtterB::new(fit),
        }
    }
}

impl Heuristic for LandSwiftTern {
    fn id(&self) -> &'static str {
        LAND_SWIFT_TERN
    }

    fn kind(&self) -> Kind {
        Kind::Land
    }

    /// A candidate (#10525), as quick-tern.
    fn tier(&self) -> Tier {
        Tier::Candidate
    }

    /// As its base: twin-otter-b models an operator hold (#10218).
    fn models_hold(&self) -> bool {
        self.base.models_hold()
    }

    fn estimate(&self, input: &EstimateInput, history: &StageSamples) -> Explanation {
        let mut explanation = self.base.estimate(input, history);
        explanation.heuristic = LAND_SWIFT_TERN.to_string();
        explanation.estimate_id =
            estimate_id(&input.subject, Kind::Land, LAND_SWIFT_TERN, input.as_of);
        if explanation.result.is_none() {
            return explanation;
        }
        conformal_ipcw::calibrate_drift_aware(explanation, &history.calibration, LAND_TWIN_OTTER_B)
    }
}
