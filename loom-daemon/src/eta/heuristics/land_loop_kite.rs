//! `land-2026-10-06-loop-kite` (#10521): the friction-aware successor of
//! `land-2026-10-06-keen-wren`.
//!
//! # Why a new id
//!
//! The error analysis of twin-otter-b's largest misses (#10521) found four
//! point-in-time predictors the PR-stage model never sees (open-PR file
//! overlap, the review-loop history, the PR's own last CI run, the repo's
//! Judge rejection rate over 7 days) and a mis-specified stage age: `age_h`
//! restarts at every loop, so a PR on its fourth approval looks minutes old.
//! Heuristic ids are immutable, so the model that reads them ships under this
//! new id, and twin-otter, twin-otter-b and keen-wren keep their behaviour
//! and their shadow history.
//!
//! # The composition
//!
//! | current stage | answered by |
//! |---|---|
//! | `ready_wait`, `sweep.curator`, `sweep.builder` | keen-wren's dispatch-plan path under this id ([`super::KEEN_WREN_PRE_PR_METHOD`]) |
//! | `review_wait`, `doctor`, `merge_wait`, `merge_hold` | the twin-otter evaluation over the newest **`eta-fit/v3`** fit, fed the item's [`crate::eta::explanation::Features::priority`] and [`crate::eta::explanation::Features::loops`] |
//! | the resolver refused | the resolver's reason |
//!
//! The PR stages read the `eta-fit/v3` feature set
//! ([`crate::eta::fit::features_v3`]): keen-wren's 26 columns, then the
//! friction block of [`crate::eta::loop_features`] (the cumulative stage age
//! as `ln(1 + h)`, review requests, approvals lost to stale-main re-reviews,
//! the repo Judge rejection rate, file overlap and own CI, each that can be
//! unknown with its `*_known` indicator). Serving builds the friction block
//! through the builder the fit calls (`Tracker::loop_features_of`, over the
//! same fleet-snapshot timeline at `as_of − LAG`), so training and serving
//! cannot disagree about it. The fit must be tagged `eta-fit/v3`; with no
//! such fit (none written yet, or its cutoff not before `as_of`) every
//! PR-stage estimate refuses `no_model`.
//!
//! Ships **registered, not current**, tier `candidate` (#10525). Promotion
//! is [`crate::eta::shadow`]'s two-gate rule (#10233).

use super::{estimate_path, LandTwinOtter, PathRules, KEEN_WREN_PRE_PR_METHOD};
use crate::eta::fit::features_v3::SCHEMA_V3;
use crate::eta::fit::CoefficientFile;
use crate::eta::history::{SampleSource, StageSamples};
use crate::eta::{CurrentState, EstimateInput, Explanation, Heuristic, Kind, Stage, Tier};
use std::sync::Arc;

/// The id. Immutable once shipped.
pub const LAND_LOOP_KITE: &str = "land-2026-10-06-loop-kite";

/// `land-2026-10-06-loop-kite`.
#[derive(Debug, Clone)]
pub struct LandLoopKite {
    pr_stages: LandTwinOtter,
}

impl Default for LandLoopKite {
    fn default() -> Self {
        LandLoopKite::new(None)
    }
}

impl LandLoopKite {
    /// The heuristic over `fit_v3`, the `eta-fit/v3` coefficient file (used
    /// for the PR-level stages).
    #[must_use]
    pub fn new(fit_v3: Option<Arc<CoefficientFile>>) -> Self {
        LandLoopKite {
            pr_stages: LandTwinOtter::friction_aware(fit_v3, LAND_LOOP_KITE, SCHEMA_V3),
        }
    }
}

/// Stages before a PR exists.
fn is_pre_pr(stage: Stage) -> bool {
    matches!(stage, Stage::ReadyWait | Stage::SweepCurator | Stage::SweepBuilder)
}

impl Heuristic for LandLoopKite {
    fn id(&self) -> &'static str {
        LAND_LOOP_KITE
    }

    fn kind(&self) -> Kind {
        Kind::Land
    }

    /// A candidate (#10525): eligible for promotion through the #10233
    /// gate, counted in the shadow budget.
    fn tier(&self) -> Tier {
        Tier::Candidate
    }

    /// As twin-otter-b: the fit has its own `merge_hold` stage (#10218).
    fn models_hold(&self) -> bool {
        true
    }

    fn estimate(&self, input: &EstimateInput, history: &StageSamples) -> Explanation {
        match &input.current {
            CurrentState::At(current) if is_pre_pr(current.stage) => {
                let mut explanation = estimate_path(
                    PathRules {
                        id: LAND_LOOP_KITE,
                        kind: Kind::Land,
                        sources: &[SampleSource::SweepOutcome, SampleSource::StageJournal],
                        always_merge: true,
                        censoring: true,
                        adjust: None,
                        // Unreachable here: `merge_hold` is a PR stage.
                        models_hold: false,
                        half_life_sec: None,
                        stall_term: false,
                        residual_tail: false,
                    },
                    input,
                    history,
                );
                if let Some(combination) = explanation.combination.as_mut() {
                    combination.method = KEEN_WREN_PRE_PR_METHOD.to_string();
                }
                explanation
            }
            _ => self.pr_stages.estimate(input, history),
        }
    }
}
