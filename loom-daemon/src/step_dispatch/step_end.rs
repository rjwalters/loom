//! A finished step's outcome, recorded (#11909; the ledger is #11900).
//!
//! When a step ends, the registry queues it
//! (`sweep_registry::step_spawn::ended`). Each step tick drains the queue
//! for every root, with the registry lock released:
//!
//! 1. take the read the claim release made before its restore write
//!    (`sweep_registry::step_spawn::ended::take_post`), or, when nothing
//!    released the claim, read the item once from the forge (the PR for a PR
//!    step, else the issue): its labels, and whether it is closed;
//! 2. compute the role's [`PostState`] from that read ([`post_state`]), or
//!    `None` when the read failed, never a default;
//! 3. read the session's own outcome line from its log
//!    ([`outcome_ledger::parse_outcome_line`]);
//! 4. [`outcome_ledger::classify`] the two, and record the result
//!    ([`SweepRegistry::record_step_outcome`]), which arms the cooldowns the
//!    next draw reads;
//! 5. publish `step.item.{N}.ended` ([`SweepRegistry::emit_step_ended`],
//!    #12152), the step's terminal bus event, from which the narration sink
//!    narrates a merged PR's public-feed `completion`.
//!
//! # The post-state per role
//!
//! `SweepRegistry::step_post_state`, the release's own rule: each claim
//! label replaces its role's ready label, so the ready label's absence proves
//! nothing (a crashed step leaves exactly that). The item moved only when it
//! carries a label the role moves it *to* (`advanced_labels`: Curator
//! `loom:curated` / `loom:issue`, Champion promote `loom:issue` /
//! `loom:triage`, Judge `loom:pr` / `loom:changes-requested`, Doctor
//! `loom:review-requested`), a Builder's PR is open (the release's open-PR
//! hold), or a merge step's PR left `loom:pr`. A claim label still on an item
//! that did not move is held: the step died mid-work, which classifies as
//! `failed` and takes the fast-fail ladder, never a verdict.

use std::path::Path;
use std::sync::{Arc, Mutex};

use super::outcome_ledger::{self, PostState};
use super::session_ledger;
use crate::step_role::StepRole;
use crate::sweep_registry::step_ledger::StepRun;
use crate::sweep_registry::step_spawn::ended::{self, EndedStep};
use crate::sweep_registry::SweepRegistry;

/// The role's post-state from one read of the item: the claim release's own
/// rule ([`SweepRegistry::step_post_state`]). A read taken here, after
/// nothing released the claim, cannot see a Builder's PR, so a Builder read
/// here moved only by its labels. Pure.
#[must_use]
pub fn post_state(role: StepRole, labels: &[String], closed: bool) -> PostState {
    SweepRegistry::step_post_state(role, labels, closed, false)
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

/// The run the ledger records for `step`, given its post-state read. A step
/// whose provider feedback said its api-key seat was exhausted
/// (`step.provider_exhausted`, #13966) records a refusal
/// ([`outcome_ledger::PROVIDER_EXHAUSTED`]) whatever the forge post-state:
/// the launch died on an empty seat, so the pair arms no cooldown and the
/// run is not counted for the repeat cap.
///
/// No cancel carve-out is needed here, unlike the two `step_failure`s: the
/// flag is set only by the exit-time provider feedback, which the cancel
/// path (`reaper::finish_cancel`, the only producer of `StepEnd::Cancelled`)
/// never runs, so a cancelled step cannot carry it.
#[must_use]
pub fn run_of(
    step: &EndedStep,
    line: Option<&outcome_ledger::StepOutcome>,
    post: Option<PostState>,
) -> StepRun {
    let outcome = if step.provider_exhausted {
        outcome_ledger::StepOutcome::new(
            outcome_ledger::OutcomeClass::Refused,
            outcome_ledger::PROVIDER_EXHAUSTED,
        )
    } else {
        outcome_ledger::classify(line, post)
    };
    StepRun {
        item: step.item,
        step: step.role,
        sweep_id: step.sweep_id.clone(),
        outcome,
        ran_for: step.ran_for,
        failure_class: step
            .timed_out
            .then(|| crate::pr_lease_reap::REASON_TIMEOUT.to_string()),
    }
}

/// The ledger's refusal reason for a spawn-time refusal
/// (`sweep_registry::step_spawn::StepRefusal::reason`): the pool gate and a
/// claim held elsewhere cool the `(item, step)` down; a lost claim is a
/// counted redraw. `None` for a refusal that says nothing about the item (a
/// roll pause, a workspace missing the role's command).
#[must_use]
pub fn ledger_reason(spawn_reason: &str) -> Option<&'static str> {
    match spawn_reason {
        "pool_exhausted" | "no_token_pool" | "token_selection_failed" => Some("pool_gate"),
        "claimed" | "claim_unverified" | "backstop_hold" | "peer_active" => Some("claimed"),
        // A failed spawn cools down like a gate, so it is not respawned on
        // every tick. A missing `/loom:<role>` command is refused before the
        // spawn (`workspace_commands_missing`) and says nothing about the
        // item, so it falls through to `None`.
        "spawn_failed" | "label_flip_failed" => Some("pool_gate"),
        "claim_lost" => Some("claim_lost"),
        _ => None,
    }
}

/// Record a spawn-time refusal of `(item, role)` in the ledger, so the next
/// draw skips the pair while it cools down and the `claim_lost` counter
/// counts (the integrated review's M1).
pub fn record_refusal(
    root: &Path,
    registry: &Arc<Mutex<SweepRegistry>>,
    item: u32,
    role: StepRole,
    spawn_reason: &str,
) {
    let Some(reason) = ledger_reason(spawn_reason) else {
        return;
    };
    let run = StepRun {
        item,
        step: role,
        sweep_id: String::new(),
        outcome: outcome_ledger::StepOutcome::new(outcome_ledger::OutcomeClass::Refused, reason),
        ran_for: std::time::Duration::ZERO,
        failure_class: None,
    };
    let Ok(mut reg) = registry.lock() else {
        return;
    };
    let mut forge = refusal_forge(&reg, root);
    reg.record_step_outcome(&run, &mut forge);
}

/// The forge a spawn-time refusal's ledger write goes through (#12621): the
/// work item's repo is its target (as in [`settle`], #12606), so the repo
/// owner's writer is selected rather than the process credential.
pub(crate) fn refusal_forge(
    reg: &SweepRegistry,
    root: &Path,
) -> crate::operator_decision::cli::GhForge {
    let slug = reg.open_pr_listing_target().map(|(_, _, slug)| slug);
    crate::operator_decision::cli::GhForge::new(root.to_path_buf(), slug)
        .with_call_site("step_ledger.reeval_ladder")
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
        // No forge target this tick (the repo slug did not resolve): keep
        // the steps for the next tick rather than drop their outcomes.
        for step in steps {
            ended::push(root, step);
        }
        return;
    };
    for step in steps {
        // The claim release's own read, taken before its restore write; else
        // one read now (nothing restored the claim, so nothing moved it).
        let post = ended::take_post(root, &step.sweep_id).unwrap_or_else(|| {
            crate::comment_trust::records::fetch_issue_object(
                &slug,
                &step.item.to_string(),
                root,
                false,
            )
            .as_ref()
            .and_then(read_object)
            .map(|(labels, closed)| post_state(step.role, &labels, closed))
        });
        let run = run_of(&step, outcome_line(&step).as_ref(), post);
        // #11913: the step's session, for a later step on the same item.
        if let Some(noted) = session_ledger::take_session(root, &step.sweep_id) {
            session_ledger::record_ended(root, step.item, step.role, &noted, chrono::Utc::now());
        }
        log::info!(
            "work_finder: {} step on #{} ({}) classified {} {}",
            step.role,
            step.item,
            step.sweep_id,
            run.outcome.class.as_str(),
            run.outcome.reason
        );
        // #12606: the ladder write is vetted under the writer it carries (the
        // item owner's per-owner credential), not the workspace root's.
        let mut forge =
            crate::operator_decision::cli::GhForge::new(root.to_path_buf(), Some(slug.clone()))
                .with_call_site("step_ledger.reeval_ladder");
        if let Ok(mut reg) = registry.lock() {
            reg.record_step_outcome(&run, &mut forge);
            // #12152: the step's terminal bus event (the narration sink's
            // completion trigger for a merged PR).
            reg.emit_step_ended(&run);
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
    fn each_role_moves_only_to_its_advanced_labels() {
        let moved = |role, ls: &[&str]| post_state(role, &labels(ls), false).moved;
        assert!(moved(StepRole::Curator, &["loom:curated"]));
        assert!(!moved(StepRole::Curator, &["loom:curating"]));
        assert!(moved(StepRole::ChampionPromote, &["loom:issue"]));
        assert!(moved(StepRole::ChampionPromote, &["loom:triage"]));
        assert!(!moved(StepRole::ChampionPromote, &["loom:curated", "loom:blocked"]));
        assert!(moved(StepRole::Judge, &["loom:pr"]));
        assert!(!moved(StepRole::Doctor, &["loom:changes-requested"]));
        assert!(moved(StepRole::ChampionMerge, &["loom:blocked"]));
        // A PR triage (#12499) moved only once it carries a stage or a hold:
        // an unlabelled PR it left alone did not.
        assert!(moved(StepRole::ChampionMerge, &["loom:review-requested"]));
        assert!(moved(StepRole::ChampionMerge, &["loom:operator"]));
        assert!(!moved(StepRole::ChampionMerge, &[]));
        assert!(!moved(StepRole::ChampionMerge, &["loom:pr", "loom:blocked"]));
        // A Builder whose PR is open moved, whatever its labels say.
        let p = SweepRegistry::step_post_state(
            StepRole::Builder,
            &labels(&["loom:building"]),
            false,
            true,
        );
        assert!(p.moved && !p.claim_held);
    }

    /// A crashed or timed-out step left its claim label where its ready label
    /// was: not a verdict, a held claim, which classifies as `failed`.
    #[test]
    fn a_crashed_builder_and_a_crashed_judge_classify_as_failed() {
        let builder = post_state(StepRole::Builder, &labels(&["loom:building"]), false);
        assert!(!builder.moved && builder.claim_held);
        let judge = post_state(StepRole::Judge, &labels(&["loom:reviewing"]), false);
        assert!(!judge.moved && judge.claim_held);
        let line = outcome_ledger::StepOutcome::new(OutcomeClass::Verdict, "approved");
        for post in [builder, judge] {
            assert_eq!(
                outcome_ledger::classify(Some(&line), Some(post)).class,
                OutcomeClass::Failed
            );
            assert_eq!(outcome_ledger::classify(None, Some(post)).class, OutcomeClass::Failed);
        }
        // Closed is closed, never a held claim.
        let p = post_state(StepRole::Judge, &labels(&["loom:reviewing"]), true);
        assert!(p.closed && !p.moved && !p.claim_held);
    }

    #[test]
    fn spawn_refusals_map_onto_the_ledgers_reasons() {
        assert_eq!(ledger_reason("pool_exhausted"), Some("pool_gate"));
        assert_eq!(ledger_reason("token_selection_failed"), Some("pool_gate"));
        assert_eq!(ledger_reason("backstop_hold"), Some("claimed"));
        // #12105: another actor working the PR cools the Doctor down.
        assert_eq!(ledger_reason("peer_active"), Some("claimed"));
        assert_eq!(ledger_reason("claim_unverified"), Some("claimed"));
        assert_eq!(ledger_reason("claim_lost"), Some("claim_lost"));
        assert_eq!(ledger_reason("roll_paused"), None);
        assert_eq!(ledger_reason("spawn_failed"), Some("pool_gate"));
        // #12048: says nothing about the item, so no window doubles against it.
        assert_eq!(ledger_reason("workspace_commands_missing"), None);
    }

    /// #13966: a step whose provider feedback said its api-key seat was
    /// exhausted records a refusal, not a run — the fast-fail ladder and the
    /// repeat cap never see it, and no item cooldown is armed.
    #[test]
    fn a_provider_exhausted_step_records_a_refusal_that_charges_nothing() {
        let step = EndedStep {
            sweep_id: "step-builder-13966-1".into(),
            item: 13966,
            role: StepRole::Builder,
            log_path: "/nonexistent".into(),
            ran_for: std::time::Duration::from_secs(80),
            timed_out: false,
            provider_exhausted: true,
        };
        // The post-state of a crashed step (claim restored, item unchanged)
        // would classify `failed`; the exhaustion override wins.
        let post = post_state(StepRole::Builder, &labels(&["loom:building"]), false);
        assert!(post.claim_held, "fixture: a crashed step's shape");
        let run = run_of(&step, None, Some(post));
        assert_eq!(run.outcome.class, OutcomeClass::Refused);
        assert_eq!(run.outcome.reason, outcome_ledger::PROVIDER_EXHAUSTED);
        // And the refusal charges nothing: not a repeat-cap run, and not the
        // state-refusal effect either, whose second identical refusal in a
        // row arms a doubling window.
        assert!(!outcome_ledger::counts_as_run(&run.outcome));
        assert_eq!(
            outcome_ledger::effect_of(
                StepRole::Builder,
                &run.outcome,
                run.ran_for,
                &outcome_ledger::LedgerConfig::default(),
            ),
            outcome_ledger::Effect::Uncharged
        );
        // Measured on the ledger itself, across consecutive exhaustions (the
        // shape of a multi-seat outage, each launch discovering the next
        // seat): the pair is never cooling, whatever the count.
        let mut ledger = outcome_ledger::OutcomeLedger::default();
        let t0 = chrono::Utc::now();
        for k in 0..6 {
            let at = t0 + chrono::Duration::minutes(k);
            let rec = ledger.record(run.item, run.step, &run.outcome, run.ran_for, at);
            assert_eq!(rec.effect, outcome_ledger::Effect::Uncharged, "exhaustion {k}");
            assert_eq!(rec.cooling_until, None, "exhaustion {k} armed a window");
            assert_eq!(ledger.cooling_cause(run.item, run.step, at), None, "exhaustion {k}");
            assert!(!rec.repeat_capped, "exhaustion {k} counted as a run");
        }
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
            provider_exhausted: false,
        };
        let run = run_of(&step, None, None);
        assert_eq!(run.outcome.class, OutcomeClass::Failed);
        assert_eq!(run.outcome.reason, outcome_ledger::PROBE_FAILED);
        let obj = serde_json::json!({"state": "closed", "labels": [{"name": "loom:pr"}]});
        assert_eq!(read_object(&obj), Some((labels(&["loom:pr"]), true)));
    }
}
