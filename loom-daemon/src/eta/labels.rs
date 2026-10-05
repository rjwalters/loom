//! Stage resolution from forge labels (pure). An issue or PR whose labels say
//! it is held, gated on a human, or in two stages at once gets a refusal
//! reason, never a guessed stage. The one hold that is a stage is an approved
//! PR held for a human, `merge_hold` (#10218), defined here and nowhere else.

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

/// The operator holds that make an approved PR's wait a
/// [`Stage::MergeHold`] (#10218) instead of a refusal: experiment v2's
/// (#10193) "held for a human".
///
/// Read through [`stage_from_pr_labels`], this is the **only** definition of
/// the held stage. The episode derivation, the tracker and every later
/// consumer (roster counts, fleet-state reconstruction) call that function,
/// so the stage a model is trained on and the stage it is served cannot
/// drift apart. Every entry is also a [`hold_labels`] entry (tested), so a
/// label-registry rename fails loudly instead of silently un-holding a PR.
/// It is also the set behind [`pr_flags`]' [`FLAG_OP_HOLD`].
pub const MERGE_HOLD_LABELS: &[&str] = &[
    "loom:operator",
    "loom:operator-only",
    "loom:operator-decision",
];

/// Hold labels that may accompany a [`MERGE_HOLD_LABELS`] entry without
/// turning the hold into a refusal: `loom:operator-mechanical` is a
/// `loom:operator-only` sub-kind and never stands alone.
pub const MERGE_HOLD_COMPANION_LABELS: &[&str] = &["loom:operator-mechanical"];

/// [`pr_flags`] bit 0: an operator hold (any of [`MERGE_HOLD_LABELS`]).
pub const FLAG_OP_HOLD: u8 = 1;
/// [`pr_flags`] bit 1: `loom:sequenced`.
pub const FLAG_SEQUENCED: u8 = 1 << 1;
/// [`pr_flags`] bit 2: starred at any level (`loom:operator-priority`, or a
/// higher level label, own or inherited: levels nest, #10307).
pub const FLAG_STARRED: u8 = 1 << 2;
/// [`pr_flags`] bit 3: `loom:merge-conflict`.
pub const FLAG_CONFLICT: u8 = 1 << 3;
/// [`pr_flags`] bit 4: `loom:ci-failure`.
pub const FLAG_CI_FAIL: u8 = 1 << 4;
/// [`pr_flags`] bit 5: `loom:blocked`.
pub const FLAG_BLOCKED: u8 = 1 << 5;
/// [`pr_flags`] bit 6: effective operator priority level 2 or more
/// (`loom:operator-high-priority`, own or inherited, #10307). Not a model
/// flag: no shipped heuristic reads it. It puts the level on the flag
/// timeline so the priority features (#10333) can read it point-in-time;
/// a level 3 is one more bit.
pub const FLAG_LEVEL_2: u8 = 1 << 6;

/// The effective operator priority level a [`pr_flags`] mask records: 2 with
/// [`FLAG_LEVEL_2`], 1 with only [`FLAG_STARRED`], else 0.
#[must_use]
pub fn level_from_flags(mask: u8) -> u8 {
    if mask & FLAG_LEVEL_2 != 0 {
        2
    } else {
        u8::from(mask & FLAG_STARRED != 0)
    }
}

/// Each [`pr_flags`] bit and the labels that set it.
const PR_FLAG_LABELS: [(u8, &[&str]); 6] = [
    (FLAG_OP_HOLD, MERGE_HOLD_LABELS),
    (FLAG_SEQUENCED, &["loom:sequenced"]),
    (FLAG_STARRED, &["loom:operator-priority"]),
    (FLAG_CONFLICT, &["loom:merge-conflict"]),
    (FLAG_CI_FAIL, &["loom:ci-failure"]),
    (FLAG_BLOCKED, &[BLOCKED_LABEL]),
];

/// A PR's six model flags as a bit mask ([`FLAG_OP_HOLD`] …
/// [`FLAG_BLOCKED`]), plus [`FLAG_LEVEL_2`]. This is the **one** label → flag definition: the fit's
/// training rows (#10245) and `land-2026-10-04-twin-otter`'s serving input
/// (#10243) both call it, so train and serve cannot drift.
#[must_use]
pub fn pr_flags(labels: &[String]) -> u8 {
    PR_FLAG_LABELS
        .iter()
        .filter(|(_, set)| labels.iter().any(|l| set.contains(&l.as_str())))
        .fold(0, |mask, (bit, _)| mask | bit)
        | match crate::operator_levels::level(labels) {
            0 => 0,
            1 => FLAG_STARRED,
            _ => FLAG_STARRED | FLAG_LEVEL_2,
        }
}

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

/// The operator hold labels (#10210): a human is needed before the item moves.
/// A subset of [`hold_labels`]; `loom:operator-priority` is the operator's
/// star, never a hold, and is deliberately absent.
pub const OPERATOR_HOLD_LABELS: &[&str] = &[
    "loom:operator",
    "loom:operator-only",
    "loom:operator-decision",
    "loom:operator-mechanical",
];

/// The first operator hold label on the item, when it has one.
#[must_use]
pub fn operator_hold_label(labels: &[String]) -> Option<&'static str> {
    OPERATOR_HOLD_LABELS
        .iter()
        .copied()
        .find(|h| has(labels, h))
}

/// Whether every hold on the item is an operator hold (and there is at least
/// one): the one hold whose stall a stall-aware heuristic can model (#10210).
/// `loom:blocked`, a park label or `loom:needs-capability` alongside it keeps
/// the plain `blocked` refusal.
#[must_use]
pub fn held_only_by_operator(labels: &[String]) -> bool {
    operator_hold_label(labels).is_some()
        && hold_labels()
            .iter()
            .filter(|h| has(labels, h))
            .all(|h| OPERATOR_HOLD_LABELS.contains(h))
}

/// The post-sweep stage of an open PR from its labels: exactly one of
/// review-requested, changes-requested and approved, and no hold — or
/// [`Stage::MergeHold`] (#10218) for an approved PR whose only holds are
/// operator holds ([`MERGE_HOLD_LABELS`], optionally with
/// [`MERGE_HOLD_COMPANION_LABELS`]).
///
/// Every other hold is still `Err(Blocked)`: `loom:blocked`,
/// `loom:needs-capability`, and an operator hold on a PR that is not
/// approved. Shipped heuristics refuse `merge_hold` as `blocked` too, so
/// their output is unchanged.
pub fn stage_from_pr_labels(labels: &[String]) -> Result<Stage, NoEstimateReason> {
    if check_holds(labels).is_err() {
        return if is_merge_hold(labels) {
            Ok(Stage::MergeHold)
        } else {
            Err(NoEstimateReason::Blocked)
        };
    }
    verdict_stage(labels)
}

/// Whether `labels` are an approved PR held only by operator holds.
fn is_merge_hold(labels: &[String]) -> bool {
    let held: Vec<&str> = hold_labels()
        .into_iter()
        .filter(|h| has(labels, h))
        .collect();
    held.iter().any(|h| MERGE_HOLD_LABELS.contains(h))
        && held
            .iter()
            .all(|h| MERGE_HOLD_LABELS.contains(h) || MERGE_HOLD_COMPANION_LABELS.contains(h))
        && verdict_stage(labels) == Ok(Stage::MergeWait)
}

/// The stage a held PR's review labels name underneath its hold (#10210) —
/// [`stage_from_pr_labels`] without the hold check.
pub fn stage_ignoring_holds(labels: &[String]) -> Result<Stage, NoEstimateReason> {
    verdict_stage(labels)
}

/// The stage the review labels alone name, ignoring holds.
fn verdict_stage(labels: &[String]) -> Result<Stage, NoEstimateReason> {
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
