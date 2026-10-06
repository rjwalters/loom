//! `land-2026-10-06-held-heron` (#10523): twin-otter-b, except that a PR
//! **held or sequenced at `as_of`** is answered by the competing-risks
//! simulator ([`crate::eta::hazard_sim`]).
//!
//! # The switching rule
//!
//! | state at `as_of` | answered by |
//! |---|---|
//! | `merge_hold` (an approved PR under an operator or Champion hold) | the simulator |
//! | `merge_wait` with `loom:sequenced` on the PR | the simulator |
//! | anything else, refusals included | `land-2026-10-04-twin-otter-b`, unchanged apart from the id |
//!
//! A hold outranks sequencing, as in the offline state machine. When the
//! simulator has too little evidence ([`hazard_sim::fit`] returns `None`),
//! the item is answered by twin-otter-b too: it degrades, never fabricates.
//!
//! # Why
//!
//! Offline (loom-experiments#19, 52 point-in-time folds on verified
//! history) the simulator alone lost to twin-otter-b by +1.75 h of pinball,
//! but won on sequenced (−11.1 h on the grid set) and held (−2.0 h) PRs;
//! this composition won by −0.53 h [−0.90, −0.21] over all folds. The split
//! was chosen after looking at subsets, so the id ships as a shadow and
//! needs shadow evidence before it can become a default.
//!
//! # The explanation
//!
//! A simulator answer records `combination.method` [`METHOD`] and the
//! whole chain in `held_heron` ([`hazard_sim::HeldHeronRecord`]), from which
//! [`crate::eta::simulate::run_explanation`] recomputes the result. A
//! twin-otter-b answer is twin-otter-b's explanation (and recomputes as
//! one). There is no conformal layer: a calibrated variant would be a new id.
//!
//! Ships **registered, not current**, tier `candidate`. Promotion is
//! [`crate::eta::shadow`]'s two-gate rule (#10233).

use super::{blank, visit_entry, LandTwinOtterB};
use crate::eta::explanation::{Combination, CurrentStageRecord, EstimateResult};
use crate::eta::fit::CoefficientFile;
use crate::eta::hazard_sim::{self, SideState};
use crate::eta::history::StageSamples;
use crate::eta::labels::{pr_flags, FLAG_SEQUENCED};
use crate::eta::{
    estimate_id, CurrentStage, CurrentState, EstimateInput, Explanation, Heuristic, Kind, Stage,
    Tier,
};
use chrono::Duration;
use std::sync::Arc;

/// The id. Immutable once shipped.
pub const LAND_HELD_HERON: &str = "land-2026-10-06-held-heron";

/// `combination.method` of a simulator answer.
pub const METHOD: &str = "held_heron_competing_risks";

/// `combination.draw_order` of a simulator answer: nothing is drawn.
pub const DRAW_ORDER: &str = "none: a deterministic forward solution of the competing-risks \
     chain in held_heron, in held_heron.step_sec steps out to held_heron.horizon_sec; no draws";

/// `land-2026-10-06-held-heron`.
#[derive(Debug, Clone, Default)]
pub struct LandHeldHeron {
    base: LandTwinOtterB,
}

impl LandHeldHeron {
    /// The heuristic over `fit` (twin-otter-b's, for every other state).
    #[must_use]
    pub fn new(fit: Option<Arc<CoefficientFile>>) -> Self {
        LandHeldHeron {
            base: LandTwinOtterB::new(fit),
        }
    }
}

/// The side state `input` is in at `as_of`, when the simulator answers it:
/// `merge_hold`, or `merge_wait` with `loom:sequenced` on the PR.
#[must_use]
pub fn side_state(input: &EstimateInput) -> Option<(SideState, &CurrentStage)> {
    let CurrentState::At(current) = &input.current else {
        return None;
    };
    let sequenced = input
        .features
        .labels
        .as_deref()
        .is_some_and(|labels| pr_flags(labels) & FLAG_SEQUENCED != 0);
    match current.stage {
        Stage::MergeHold => Some((SideState::MergeHold, current)),
        Stage::MergeWait if sequenced => Some((SideState::Sequenced, current)),
        _ => None,
    }
}

impl Heuristic for LandHeldHeron {
    fn id(&self) -> &'static str {
        LAND_HELD_HERON
    }

    fn kind(&self) -> Kind {
        Kind::Land
    }

    /// A candidate: eligible for promotion through the #10233 gate, counted
    /// in the shadow budget.
    fn tier(&self) -> Tier {
        Tier::Candidate
    }

    /// It answers a held PR, so it reads the modeled `merge_hold` input
    /// (#10284), as its base does.
    fn models_hold(&self) -> bool {
        true
    }

    fn estimate(&self, input: &EstimateInput, history: &StageSamples) -> Explanation {
        if let Some((state, current)) = side_state(input) {
            if let Some(explanation) = simulate(input, history, state, current) {
                return explanation;
            }
        }
        let mut explanation = self.base.estimate(input, history);
        explanation.heuristic = LAND_HELD_HERON.to_string();
        explanation.estimate_id =
            estimate_id(&input.subject, Kind::Land, LAND_HELD_HERON, input.as_of);
        explanation
    }
}

/// The simulator's answer for an item in `state`, or `None` when it has too
/// little evidence.
fn simulate(
    input: &EstimateInput,
    history: &StageSamples,
    state: SideState,
    current: &CurrentStage,
) -> Option<Explanation> {
    let as_of = input.as_of;
    let spell_age_sec = match state {
        // The hold visit's age: the episode began at the hold.
        SideState::MergeHold => Some((as_of - visit_entry(current, as_of)).num_seconds()),
        SideState::Sequenced => input.subject.pr_number.and_then(|pr| {
            hazard_sim::sequenced_since(history, &input.subject.repo, pr, as_of)
                .map(|since| (as_of - since).num_seconds())
        }),
    };
    let mut record = hazard_sim::fit(history, &input.subject.repo, as_of, state, spell_age_sec)?;
    let solution = hazard_sim::solve(&record, as_of)?;
    record.landed_by_horizon = solution.landed_by_horizon;
    let (p25, p50, p75, p90) = solution.quantiles;
    let mut explanation = blank(LAND_HELD_HERON, Kind::Land, input);
    explanation.current_stage = Some(CurrentStageRecord {
        stage: current.stage,
        entered_at: current.entered_at,
        age_sec: current.age_sec.max(0),
        age_source: current.age_source,
        rework_rounds: current.rework_rounds,
    });
    explanation.combination = Some(Combination {
        method: METHOD.to_string(),
        draws: 0,
        seed: format!("0x{:016x}", 0),
        rng: "none".to_string(),
        draw_order: DRAW_ORDER.to_string(),
        independence_assumed: false,
    });
    explanation.result = Some(EstimateResult {
        p25_sec: p25,
        p50_sec: p50,
        p75_sec: p75,
        p90_sec: Some(p90),
        eta_p50_at: as_of + Duration::seconds(p50),
        samples_min: record.spell_exits() as usize,
        stage_marks: Vec::new(),
        tail_extrapolated: false,
    });
    explanation.held_heron = Some(record);
    explanation.enforce_cap();
    Some(explanation)
}
