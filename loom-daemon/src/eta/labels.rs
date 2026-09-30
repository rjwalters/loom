//! Stage resolution from forge labels (pure). An issue or PR whose labels say
//! it is held, gated on a human, or in two stages at once gets a refusal
//! reason, never a guessed stage.

use super::{NoEstimateReason, Stage};
use crate::observability::queue_blocked::{BLOCKED_LABEL, HOLD_LABELS};
use crate::types::{PlanState, RowPlan};
use crate::work_finder::{BUILDING_LABEL, PARK_LABELS, SKIP_LABELS};

/// A PR waiting for a Judge verdict.
pub const REVIEW_REQUESTED: &str = "loom:review-requested";
/// A PR the Judge sent back to the Doctor.
pub const CHANGES_REQUESTED: &str = "loom:changes-requested";
/// A Doctor is working the PR (accompanies `loom:changes-requested`).
pub const TREATING: &str = "loom:treating";
/// A Judge-approved PR.
pub const APPROVED: &str = "loom:pr";

/// Issue labels of the human-gated intake and approval stages.
pub const HUMAN_GATED_LABELS: &[&str] = &["loom:triage", "loom:curating", "loom:curated"];

/// The ready label: approved, not yet dispatched.
pub const READY_LABEL: &str = "loom:issue";

/// Every label that holds or parks an item: the work finder's park and skip
/// sets (minus its own claim label, which is not a hold) and the blocked
/// listing's hold co-labels.
#[must_use]
pub fn hold_labels() -> Vec<&'static str> {
    let mut labels: Vec<&'static str> = Vec::new();
    let all = std::iter::once(BLOCKED_LABEL)
        .chain(PARK_LABELS.iter().copied())
        .chain(SKIP_LABELS.iter().copied())
        .chain(HOLD_LABELS.iter().copied());
    for label in all {
        if label != BUILDING_LABEL && !labels.contains(&label) {
            labels.push(label);
        }
    }
    labels
}

fn has(labels: &[String], wanted: &str) -> bool {
    labels.iter().any(|l| l == wanted)
}

/// Refuse when any hold label is present.
pub fn check_holds(labels: &[String]) -> Result<(), NoEstimateReason> {
    if hold_labels().iter().any(|h| has(labels, h)) {
        Err(NoEstimateReason::Blocked)
    } else {
        Ok(())
    }
}

/// The post-sweep stage of an open PR from its labels: exactly one of
/// review-requested, changes-requested and approved, and no hold.
pub fn stage_from_pr_labels(labels: &[String]) -> Result<Stage, NoEstimateReason> {
    check_holds(labels)?;
    let stages: Vec<Stage> = [
        (REVIEW_REQUESTED, Stage::ReviewWait),
        (CHANGES_REQUESTED, Stage::Doctor),
        (APPROVED, Stage::MergeWait),
    ]
    .into_iter()
    .filter(|(label, _)| has(labels, label))
    .map(|(_, stage)| stage)
    .collect();
    match stages.as_slice() {
        [stage] => Ok(*stage),
        // `loom:treating` alone means a Doctor holds the PR.
        [] if has(labels, TREATING) => Ok(Stage::Doctor),
        _ => Err(NoEstimateReason::UnknownStage),
    }
}

/// Why an issue with no running sweep and no open PR has no estimate, or
/// `None` when it has one: a ready issue the dispatch plan (#9288) gives a
/// position (#9326). `plan` is its row on the last tick, `None` when no plan
/// covers it.
#[must_use]
pub fn unstarted_issue_reason(
    labels: &[String],
    plan: Option<&RowPlan>,
) -> Option<NoEstimateReason> {
    if let Err(reason) = check_holds(labels) {
        return Some(reason);
    }
    if HUMAN_GATED_LABELS.iter().any(|l| has(labels, l)) {
        Some(NoEstimateReason::HumanGated)
    } else if has(labels, READY_LABEL) {
        plan.map_or(Some(NoEstimateReason::NoDispatchPlan), ready_row_reason)
    } else {
        Some(NoEstimateReason::UnknownStage)
    }
}

/// Why a ready row of the dispatch plan has no `start` estimate, or `None`
/// when it has one: it is waiting (`next`/`queued`) at a plan position.
/// A `blocked` row, a row with no position, and a row whose state this build
/// does not know are all outside the plan's coverage: `no_dispatch_plan`.
/// (`running` rows are dispatched; the sweep, not the plan, estimates them.)
#[must_use]
pub fn ready_row_reason(plan: &RowPlan) -> Option<NoEstimateReason> {
    match (plan.plan_state, plan.position) {
        (PlanState::Next | PlanState::Queued, Some(_)) => None,
        _ => Some(NoEstimateReason::NoDispatchPlan),
    }
}
