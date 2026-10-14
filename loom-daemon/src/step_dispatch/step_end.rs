//! A finished step's outcome, recorded (#11909; the ledger is #11900).
//!
//! When a step ends, the registry queues it
//! (`sweep_registry::step_spawn::ended`). Each step tick drains the queue
//! for every root, with the registry lock released:
//!
//! 1. read the item once from the forge (the PR for a PR step, else the
//!    issue): its labels, and whether it is closed (a merged PR counts);
//! 2. compute the role's [`PostState`] from that read ([`post_state`]), or
//!    `None` when the read failed, never a default;
//! 3. read the session's own outcome line from its log
//!    ([`outcome_ledger::parse_outcome_line`]);
//! 4. [`outcome_ledger::classify`] the two, and record the result
//!    ([`SweepRegistry::record_step_outcome`]), which arms the cooldowns the
//!    next draw reads.
//!
//! # The post-state per role
//!
//! `moved` is "the item left the stage that called for this role": no read
//! of the item before the step is needed, so a step adopted after a restart
//! classifies the same way.
//!
//! | Role | moved when the item |
//! |---|---|
//! | Curator | carries `loom:curated`, `loom:issue`, `loom:blocked` or `loom:operator` |
//! | Champion promote | lost `loom:curated`, or carries `loom:issue`, `loom:triage` or `loom:operator` |
//! | Builder | lost `loom:issue` |
//! | Judge | lost `loom:review-requested` |
//! | Doctor | lost `loom:changes-requested` |
//! | Champion merge | lost `loom:pr` |
//!
//! A closed item is `closed` for every role. `claim_held` is the role's claim
//! label still on an item that did not move: a claim left beside a verdict
//! is that verdict's own state (a Builder's `loom:building` stays while its
//! PR is open, #6199).

use std::path::Path;
use std::sync::{Arc, Mutex};

use super::outcome_ledger::{self, PostState};
use crate::step_role::StepRole;
use crate::sweep_registry::step_ledger::StepRun;
use crate::sweep_registry::step_spawn::ended::{self, EndedStep};
use crate::sweep_registry::SweepRegistry;

const TRIAGE: &str = concat!("loom", ":triage");
const CURATED: &str = concat!("loom", ":curated");
const READY: &str = concat!("loom", ":issue");
const BLOCKED: &str = concat!("loom", ":blocked");
const OPERATOR: &str = concat!("loom", ":operator");
const REVIEW_REQUESTED: &str = concat!("loom", ":review-requested");
const CHANGES_REQUESTED: &str = concat!("loom", ":changes-requested");
const APPROVED: &str = concat!("loom", ":pr");

/// The role's post-state from one read of the item (module docs). Pure.
#[must_use]
pub fn post_state(role: StepRole, labels: &[String], closed: bool) -> PostState {
    let has = |l: &str| labels.iter().any(|x| x == l);
    let moved = match role {
        StepRole::Curator => [CURATED, READY, BLOCKED, OPERATOR].iter().any(|l| has(l)),
        StepRole::ChampionPromote => {
            !has(CURATED) || [READY, TRIAGE, OPERATOR].iter().any(|l| has(l))
        }
        StepRole::Builder => !has(READY),
        StepRole::Judge => !has(REVIEW_REQUESTED),
        StepRole::Doctor => !has(CHANGES_REQUESTED),
        StepRole::ChampionMerge => !has(APPROVED),
    };
    let moved = moved && !closed;
    PostState {
        moved,
        closed,
        claim_held: !moved && !closed && role.claim_label().is_some_and(has),
    }
}

/// The item's labels and closed state from a REST issue object (a PR is an
/// issue there; a merged PR is closed).
#[must_use]
pub fn read_object(v: &serde_json::Value) -> Option<(Vec<String>, bool)> {
    let labels = v
        .get("labels")?
        .as_array()?
        .iter()
        .filter_map(|l| l.get("name").and_then(serde_json::Value::as_str))
        .map(str::to_string)
        .collect();
    let closed = v.get("state").and_then(serde_json::Value::as_str) == Some("closed");
    Some((labels, closed))
}

/// The session's outcome line in its log, after its own dispatch header.
fn outcome_line(step: &EndedStep) -> Option<outcome_ledger::StepOutcome> {
    let text = String::from_utf8_lossy(&std::fs::read(&step.log_path).ok()?).into_owned();
    let anchor = format!("sweep_id={} ", step.sweep_id);
    let from = text.rfind(&anchor).unwrap_or(0);
    outcome_ledger::parse_outcome_line(&text[from..], step.role, step.item)
}

/// The run the ledger records for `step`, given its post-state read.
#[must_use]
pub fn run_of(
    step: &EndedStep,
    line: Option<&outcome_ledger::StepOutcome>,
    post: Option<PostState>,
) -> StepRun {
    StepRun {
        item: step.item,
        step: step.role,
        sweep_id: step.sweep_id.clone(),
        outcome: outcome_ledger::classify(line, post),
        ran_for: step.ran_for,
        failure_class: step
            .timed_out
            .then(|| crate::pr_lease_reap::REASON_TIMEOUT.to_string()),
    }
}

/// Record every step of `root` that ended since the last tick (module docs).
pub fn settle(root: &Path, registry: &Arc<Mutex<SweepRegistry>>) {
    let steps = ended::take(root);
    if steps.is_empty() {
        return;
    }
    let target = registry
        .lock()
        .ok()
        .and_then(|reg| reg.open_pr_listing_target());
    let Some((_, _, slug)) = target else {
        return;
    };
    for step in steps {
        let post = crate::comment_trust::records::fetch_issue_object(
            &slug,
            &step.item.to_string(),
            root,
            false,
        )
        .as_ref()
        .and_then(read_object)
        .map(|(labels, closed)| post_state(step.role, &labels, closed));
        let run = run_of(&step, outcome_line(&step).as_ref(), post);
        log::info!(
            "work_finder: {} step on #{} ({}) classified {} {}",
            step.role,
            step.item,
            step.sweep_id,
            run.outcome.class.as_str(),
            run.outcome.reason
        );
        let mut forge =
            crate::operator_decision::cli::GhForge::new(root.to_path_buf(), Some(slug.clone()));
        if let Ok(mut reg) = registry.lock() {
            reg.record_step_outcome(&run, &mut forge);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::step_dispatch::outcome_ledger::OutcomeClass;

    fn labels(ls: &[&str]) -> Vec<String> {
        ls.iter().map(|l| (*l).to_string()).collect()
    }

    #[test]
    fn each_role_moves_off_its_own_stage() {
        let moved = |role, ls: &[&str]| post_state(role, &labels(ls), false).moved;
        assert!(moved(StepRole::Curator, &["loom:curated"]));
        assert!(!moved(StepRole::Curator, &["loom:triage"]));
        assert!(moved(StepRole::ChampionPromote, &["loom:issue"]));
        assert!(moved(StepRole::ChampionPromote, &["loom:triage"]));
        assert!(!moved(StepRole::ChampionPromote, &["loom:curated", "loom:blocked"]));
        assert!(moved(StepRole::Builder, &["loom:building"]));
        assert!(!moved(StepRole::Builder, &["loom:issue"]));
        assert!(moved(StepRole::Judge, &["loom:pr"]));
        assert!(!moved(StepRole::Doctor, &["loom:changes-requested"]));
        assert!(moved(StepRole::ChampionMerge, &["loom:blocked"]));
    }

    #[test]
    fn a_claim_beside_a_verdict_is_not_held_and_a_stale_one_is() {
        // Builder's PR is open: `loom:building` stays, the step moved.
        let p = post_state(StepRole::Builder, &labels(&["loom:building"]), false);
        assert!(p.moved && !p.claim_held);
        // A Judge that left its claim on an unchanged PR.
        let p = post_state(
            StepRole::Judge,
            &labels(&["loom:review-requested", "loom:reviewing"]),
            false,
        );
        assert!(!p.moved && p.claim_held);
        let p = post_state(StepRole::Judge, &labels(&["loom:review-requested"]), true);
        assert!(p.closed && !p.moved && !p.claim_held);
    }

    #[test]
    fn a_failed_read_classifies_as_a_failed_probe() {
        let step = EndedStep {
            sweep_id: "step-judge-7-1".into(),
            item: 7,
            role: StepRole::Judge,
            log_path: "/nonexistent".into(),
            ran_for: std::time::Duration::from_secs(60),
            timed_out: false,
        };
        let run = run_of(&step, None, None);
        assert_eq!(run.outcome.class, OutcomeClass::Failed);
        assert_eq!(run.outcome.reason, outcome_ledger::PROBE_FAILED);
        let obj = serde_json::json!({"state": "closed", "labels": [{"name": "loom:pr"}]});
        assert_eq!(read_object(&obj), Some((labels(&["loom:pr"]), true)));
    }
}
