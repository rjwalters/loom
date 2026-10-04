//! `land-2026-10-04-twin-otter-b` (#10244): twin-otter, composed with
//! `land-v2` for the stages the fit does not cover.
//!
//! # Why a second id
//!
//! [`super::LandTwinOtter`] refuses `unknown_stage` for `ready_wait`,
//! `sweep.curator` and `sweep.builder`: its model is PR-level. Promotion
//! (#10233) gates on answer rate, so a twin-otter that refuses every pre-PR
//! item could never replace `land-v2`. Heuristic ids are immutable once
//! shipped, so the composition is a **new id** and the original keeps its
//! shadow history clean.
//!
//! # The composition
//!
//! | current stage | answered by |
//! |---|---|
//! | `ready_wait`, `sweep.curator`, `sweep.builder` | `land-v2`'s path rules (Kaplan-Meier grids, always-merge) |
//! | `review_wait`, `doctor`, `merge_wait`, `merge_hold` | [`super::LandTwinOtter`], unchanged |
//! | the resolver refused | the resolver's reason |
//!
//! The pre-PR prefix uses exactly `land-v2`'s [`PathRules`] apart from the
//! id, so its answered-ness equals `land-v2`'s by construction (and a test
//! pins it). The explanation records the source in `combination.method`
//! ([`PRE_PR_METHOD`]); it recomputes through the ordinary path machinery.
//! The Monte Carlo seed derives from the estimate id, so its draws differ
//! from `land-v2`'s while its refusals do not.
//!
//! Ships as shadow, registered after `land-2026-10-04-twin-otter`.

use super::{estimate_path, LandTwinOtter, PathRules};
use crate::eta::history::{SampleSource, StageSamples};
use crate::eta::{CurrentState, EstimateInput, Explanation, Heuristic, Kind, Stage};
use std::sync::Arc;

use crate::eta::fit::CoefficientFile;

/// The id. Immutable once shipped.
pub const LAND_TWIN_OTTER_B: &str = "land-2026-10-04-twin-otter-b";

/// `combination.method` of a pre-PR answer: the path came from `land-v2`.
pub const PRE_PR_METHOD: &str = "land_v2_path_prefix";

/// `land-2026-10-04-twin-otter-b`.
#[derive(Debug, Clone, Default)]
pub struct LandTwinOtterB {
    twin_otter: LandTwinOtter,
}

impl LandTwinOtterB {
    /// The heuristic over `fit` (used for the PR-level stages).
    #[must_use]
    pub fn new(fit: Option<Arc<CoefficientFile>>) -> Self {
        LandTwinOtterB {
            twin_otter: LandTwinOtter::new(fit),
        }
    }
}

/// Stages before a PR exists.
fn is_pre_pr(stage: Stage) -> bool {
    matches!(stage, Stage::ReadyWait | Stage::SweepCurator | Stage::SweepBuilder)
}

impl Heuristic for LandTwinOtterB {
    fn id(&self) -> &'static str {
        LAND_TWIN_OTTER_B
    }

    fn kind(&self) -> Kind {
        Kind::Land
    }

    fn estimate(&self, input: &EstimateInput, history: &StageSamples) -> Explanation {
        match &input.current {
            CurrentState::At(current) if is_pre_pr(current.stage) => {
                let mut explanation = estimate_path(
                    PathRules {
                        id: LAND_TWIN_OTTER_B,
                        kind: Kind::Land,
                        sources: &[SampleSource::SweepOutcome, SampleSource::StageJournal],
                        always_merge: true,
                        censoring: true,
                        adjust: None,
                    },
                    input,
                    history,
                );
                if let Some(combination) = explanation.combination.as_mut() {
                    combination.method = PRE_PR_METHOD.to_string();
                }
                explanation
            }
            _ => {
                // PR-level stage (or a resolver refusal): twin-otter's own
                // explanation, re-identified as this heuristic's.
                let mut explanation = self.twin_otter.estimate(input, history);
                explanation.heuristic = LAND_TWIN_OTTER_B.to_string();
                explanation.estimate_id = crate::eta::estimate_id(
                    &input.subject,
                    Kind::Land,
                    LAND_TWIN_OTTER_B,
                    input.as_of,
                );
                explanation
            }
        }
    }
}
