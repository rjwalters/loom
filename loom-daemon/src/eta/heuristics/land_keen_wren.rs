//! `land-2026-10-06-keen-wren` (#10508): the priority-aware successor of
//! `land-2026-10-04-twin-otter-b`.
//!
//! # Why a new id
//!
//! Twin-otter's PR-stage model reads only the PR's own star. It never sees a
//! star that reaches the PR through its linked issue (#10372), the priority
//! level, the repo's fleet priority, or where the cross-repo dispatch order
//! puts it. Heuristic ids are immutable, so the priority-aware model ships
//! under this new id, and twin-otter and twin-otter-b keep their behaviour
//! and their shadow history.
//!
//! # The composition
//!
//! | current stage | answered by |
//! |---|---|
//! | `ready_wait`, `sweep.curator`, `sweep.builder` | `land-v2`'s path rules under this id ([`PRE_PR_METHOD`]) |
//! | `review_wait`, `doctor`, `merge_wait`, `merge_hold` | the twin-otter evaluation over the newest **`eta-fit/v2`** fit, fed the item's [`crate::eta::explanation::Features::priority`] |
//! | the resolver refused | the resolver's reason |
//!
//! The PR stages read the `eta-fit/v2` feature set
//! ([`crate::eta::fit::features_v2`]): `starred_any` (the PR **or** its
//! linked issue) in place of the PR-only `starred`, the priority level, the
//! repo's normalized fleet rank, and the fleet-wide dispatch position, each
//! with an unknown indicator. Serving builds those inputs through the
//! builder the fit calls ([`crate::eta::priority_inputs`]), so training and
//! serving cannot disagree about them. The fit must be tagged `eta-fit/v2`;
//! with no such fit (none written yet, or its cutoff not before `as_of`)
//! every PR-stage estimate refuses `no_model`.
//!
//! # `ready_wait` follows the real dispatch order
//!
//! A ready item's start comes from its position in the work finder's own
//! dispatch plan ([`crate::eta::DispatchInput`]). That plan is sorted by the
//! real comparator ([`crate::work_finder::ready_queue::candidate_keys`]):
//! priority level, star, star time, main-red fix, the workspace's
//! `fleet_priority`, age, number. So a starred issue, a level-2 issue, or an
//! issue in a higher-priority repo is ahead in the plan, has fewer slot
//! turnovers before it, and gets an earlier start. No second ordering is
//! defined here to drift from the planner (#10528).
//!
//! Ships **registered, not current**, tier `candidate` (#10525). Promotion
//! is [`crate::eta::shadow`]'s two-gate rule (#10233).

use super::{estimate_path, LandTwinOtter, PathRules};
use crate::eta::fit::features_v2::SCHEMA_V2;
use crate::eta::fit::CoefficientFile;
use crate::eta::history::{SampleSource, StageSamples};
use crate::eta::{CurrentState, EstimateInput, Explanation, Heuristic, Kind, Stage, Tier};
use std::sync::Arc;

/// The id. Immutable once shipped.
pub const LAND_KEEN_WREN: &str = "land-2026-10-06-keen-wren";

/// `combination.method` of a pre-PR answer: the path from the dispatch
/// plan position and `land-v2`'s stage rules.
pub const PRE_PR_METHOD: &str = "dispatch_plan_path_prefix";

/// `land-2026-10-06-keen-wren`.
#[derive(Debug, Clone)]
pub struct LandKeenWren {
    pr_stages: LandTwinOtter,
}

impl Default for LandKeenWren {
    fn default() -> Self {
        LandKeenWren::new(None)
    }
}

impl LandKeenWren {
    /// The heuristic over `fit_v2`, the `eta-fit/v2` coefficient file (used
    /// for the PR-level stages).
    #[must_use]
    pub fn new(fit_v2: Option<Arc<CoefficientFile>>) -> Self {
        LandKeenWren {
            pr_stages: LandTwinOtter::priority_aware(fit_v2, LAND_KEEN_WREN, SCHEMA_V2),
        }
    }
}

/// Stages before a PR exists.
fn is_pre_pr(stage: Stage) -> bool {
    matches!(stage, Stage::ReadyWait | Stage::SweepCurator | Stage::SweepBuilder)
}

impl Heuristic for LandKeenWren {
    fn id(&self) -> &'static str {
        LAND_KEEN_WREN
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
                        id: LAND_KEEN_WREN,
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
                    combination.method = PRE_PR_METHOD.to_string();
                }
                explanation
            }
            _ => self.pr_stages.estimate(input, history),
        }
    }
}
