//! `loom.dispatch.step` — one span per step spawn attempt (Issue #11906;
//! design #11894 §7).
//!
//! The step engine spawns one role session per drawn item. Each attempt,
//! spawned or refused, is one span naming the repo, the item, the role, the
//! admission result and its reason, which draw attempt it was within its
//! slot, where the item's stage age came from (`age_source`, §4.3) and
//! whether the step resumed a paused session. Its own root trace: a step is
//! spawned after the tick that drew it has been recorded.

use std::path::Path;

use chrono::{DateTime, Utc};

use crate::observability::lifecycle::role_draw;
use crate::runtime_preference::role_draw::RoleDrawRecord;
use crate::step_role::StepRole;
use crate::sweep_registry::step_spawn::StepDrawTerms;
use crate::telemetry::trace::{SpanName, SpanRecord, SpanStatus, TraceAttributes, TraceContext};

/// `loom.dispatch.admission_result` of a step that launched.
pub const RESULT_SPAWNED: &str = "spawned";
/// `loom.dispatch.admission_result` of a step a gate or its claim refused.
pub const RESULT_REFUSED: &str = "refused";
/// The story-points attribute (#12957), on this span and the step's
/// `loom.sweep` close: the key `sweep.started` / `sweep.outcome` use.
pub const STORY_POINTS: &str = "loom.story_points";

/// One spawn attempt's facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepDispatch<'a> {
    pub item: u32,
    pub role: StepRole,
    /// [`RESULT_SPAWNED`] or [`RESULT_REFUSED`].
    pub result: &'a str,
    /// A closed token: `ok`, or why the step was refused.
    pub reason: &'a str,
    /// 1-based draw attempt within the slot (design §4.2 step 4).
    pub draw_attempt: u32,
    /// Which stage-entry source answered the item's age (design §4.3).
    pub age_source: &'a str,
    pub resumed: bool,
    /// The weight terms of the draw that picked the step (#12185).
    pub draw: Option<StepDrawTerms>,
    /// The item's story points (#12957): `loom.story_points`, absent when
    /// the item is unsized. A PR step's are its closing issue's.
    pub story_points: Option<u32>,
    /// The step's sweep id, once one was assigned.
    pub sweep_id: Option<&'a str>,
    /// How the step's pre-claim ran, for a role-keyed lease claim (#12183).
    pub claim: Option<&'a crate::sweep_registry::step_spawn::lease::ClaimFacts>,
    /// The fleet-dispatch server's assignment, when it granted the claim
    /// (#12450): `loom.assignment.id`, which joins this span to the
    /// `loom.fleet_dispatch.call` spans of the same assignment.
    pub assignment_id: Option<&'a str>,
    /// The attempt this grant replaces (#12875): `loom.dispatch.attempt`,
    /// `loom.dispatch.prev_assignment_id` and `loom.dispatch.prev_end`. Unset
    /// on a first grant.
    pub lineage: Option<&'a crate::fleet_dispatch::worker::GrantLineage>,
    /// The captain's refusal details (#12707): `loom.dispatch.held_by` and
    /// `loom.dispatch.retry_after_s`, plus `loom.dispatch.hold_until` (wall
    /// clock) when the refusal is a no-progress hold.
    pub captain: Option<&'a crate::fleet_dispatch::worker::Refusal>,
    /// The role draw's model against the model actually launched (#12748).
    pub models: Option<StepModels<'a>>,
    /// The admitted runtime the step launches on (`loom.runtime`); `None`
    /// when the step was refused before admission resolved one.
    pub runtime: Option<&'a str>,
    /// The launch's reasoning effort (`loom.effort`), when one was resolved
    /// (#14167); `None` when the runtime keeps its session default.
    pub effort: Option<&'a str>,
    /// The `runtimes.roleDraw` draw that decided the launch (#14233);
    /// `None` when no table did, or the step was refused before launch.
    /// Stamps `loom.role_draw.pick` and `loom.role_draw.attempt` only. The
    /// attempt is the role draw's escalation count (1 + consecutive failed
    /// attempts), unrelated to `draw_attempt` above, the item redraw count.
    pub role_draw: Option<&'a RoleDrawRecord>,
}

/// Selected versus launched model of a spawned step. Distinct on purpose: a
/// model in the draw record is a *selection*, not proof the launch used it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepModels<'a> {
    /// The model the role draw's pick named, if it named one.
    pub selected: Option<&'a str>,
    /// The `--model` the child was launched with; `None` when the runtime
    /// picks its own.
    pub launched: Option<&'a str>,
    /// The model the launch was configured for, before any selection-time
    /// fallback (#13873); `None` when no model was resolved. Equal to
    /// `launched` whenever no fallback fired — the pair is how an operator
    /// sees how much work ran below its configured model.
    pub configured: Option<&'a str>,
    /// The selection-time fallback that changed configured → launched
    /// (#13873), when one fired.
    pub fallback: Option<StepModelFallback<'a>>,
}

/// One selection-time model fallback's facts (#13873): the configured model
/// the launch fell back from, and the selector result that triggered it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepModelFallback<'a> {
    /// The configured model the launch fell back from.
    pub from: &'a str,
    /// Always `no-account-for-class` today (`model_fallback::REASON`).
    pub reason: &'static str,
}

/// The span for `step` in `repo` at `at`. Pure.
#[must_use]
pub fn span(step: &StepDispatch<'_>, repo: Option<(&str, &str)>, at: DateTime<Utc>) -> SpanRecord {
    let name = SpanName::DispatchStep;
    let item = step.item.to_string();
    let instant = crate::telemetry::trace::instant(at);
    let context = TraceContext::derived(
        name.as_str(),
        &[
            repo.map_or("", |r| r.0),
            &item,
            step.role.as_str(),
            &instant,
        ],
    );
    let mut attributes = TraceAttributes::new();
    if let Some((slug, visibility)) = repo {
        attributes.insert("loom.repo".into(), slug.to_string());
        attributes.insert("loom.repo.visibility".into(), visibility.to_string());
    }
    // A PR step's item is a PR; the issue key would name an issue that does
    // not exist (`sweep_registry/step_issue.rs`).
    let item_key = if step.role.acts_on_pr() {
        "loom.pr_number"
    } else {
        "loom.issue"
    };
    attributes.insert(item_key.into(), item.clone());
    attributes.insert("loom.step.item".into(), item);
    attributes.insert("loom.role".into(), step.role.role_name().to_string());
    attributes.insert("loom.step.role".into(), step.role.as_str().to_string());
    if let Some(runtime) = step.runtime {
        attributes.insert("loom.runtime".into(), runtime.to_string());
    }
    if let Some(effort) = step.effort {
        attributes.insert("loom.effort".into(), effort.to_string());
    }
    attributes.insert("loom.dispatch.admission_result".into(), step.result.to_string());
    attributes.insert("loom.dispatch.reason".into(), step.reason.to_string());
    attributes.insert("loom.step.draw_attempt".into(), step.draw_attempt.to_string());
    attributes.insert("loom.step.age_source".into(), step.age_source.to_string());
    attributes.insert("loom.step.resumed".into(), step.resumed.to_string());
    if let Some(draw) = step.draw {
        attributes.insert("loom.step.weight".into(), draw.weight.to_string());
        attributes.insert("loom.step.total_weight".into(), draw.total_weight.to_string());
        attributes.insert("loom.step.repo_items".into(), draw.repo_items.to_string());
        attributes.insert("loom.step.in_flight_repo".into(), draw.in_flight_repo.to_string());
    }
    if let Some(points) = step.story_points {
        attributes.insert(STORY_POINTS.into(), points.to_string());
    }
    if let Some(id) = step.sweep_id {
        attributes.insert("loom.sweep_id".into(), id.to_string());
    }
    if let Some(claim) = step.claim {
        attributes.insert("loom.dispatch.claim_outcome".into(), claim.outcome.to_string());
        if let Some(code) = claim.exit_code {
            attributes.insert("loom.dispatch.claim_exit_code".into(), code.to_string());
        }
        attributes.insert("loom.dispatch.claim_ms".into(), claim.claim_ms.to_string());
        if let Some(tail) = &claim.stderr_tail {
            attributes.insert("loom.dispatch.claim_stderr_tail".into(), tail.clone());
        }
        if let Some(rollback) = claim.rollback {
            attributes.insert("loom.dispatch.claim_rollback".into(), rollback.to_string());
        }
    }
    if let Some(id) = step.assignment_id {
        attributes.insert("loom.assignment.id".into(), id.to_string());
    }
    if let Some(lineage) = step.lineage {
        attributes.insert("loom.dispatch.attempt".into(), lineage.attempt.to_string());
        attributes
            .insert("loom.dispatch.prev_assignment_id".into(), lineage.prev_assignment_id.clone());
        attributes.insert("loom.dispatch.prev_end".into(), lineage.prev_end.clone());
    }
    if let Some(captain) = step.captain {
        if let Some(held_by) = &captain.held_by {
            attributes.insert("loom.dispatch.held_by".into(), held_by.clone());
        }
        if let Some(retry) = captain.retry_after_s {
            attributes.insert("loom.dispatch.retry_after_s".into(), retry.to_string());
            if step.reason == "captain_cooldown" {
                attributes.insert(
                    "loom.dispatch.hold_until".into(),
                    crate::fleet_dispatch::telemetry::hold_until(at, retry),
                );
            }
        }
    }
    if let Some(models) = step.models {
        if let Some(configured) = models.configured {
            attributes.insert("loom.step.model_configured".into(), configured.to_string());
        }
        if let Some(selected) = models.selected {
            attributes.insert("loom.step.model_selected".into(), selected.to_string());
        }
        if let Some(launched) = models.launched {
            attributes.insert("loom.step.model_launched".into(), launched.to_string());
        }
        if let Some(fallback) = models.fallback {
            attributes.insert("loom.step.model_fallback_from".into(), fallback.from.to_string());
            attributes
                .insert("loom.step.model_fallback_reason".into(), fallback.reason.to_string());
        }
    }
    if let Some(draw) = step.role_draw {
        attributes
            .insert(role_draw::PICK.into(), draw.pick.clone().unwrap_or_else(|| "none".into()));
        attributes.insert(role_draw::ATTEMPT.into(), draw.attempt.to_string());
    }
    crate::telemetry::trace::provenance::stamp(&mut attributes);
    SpanRecord {
        context,
        parent_span_id: None,
        name,
        started_at: at,
        ended_at: at,
        status: SpanStatus::Ok,
        attributes,
        events: Vec::new(),
        links: Vec::new(),
    }
}

/// Emit `step`'s span for the workspace at `root`. A no-op without an
/// exporter; never shells out (the repo comes from the collector's cache, or
/// the checkout's memoised origin before its first pass).
pub fn record(root: &Path, step: &StepDispatch<'_>) {
    if !super::spans_exported() {
        return;
    }
    let repo = super::super::repo_ref::cached_repo_ref(&root.display().to_string())
        .or_else(|| super::dispatch_facts::fallback_repo_ref(root));
    let visibility = repo.as_ref().map(|r| r.visibility.as_str());
    let repo = repo.as_ref().map(|r| r.repo.as_str()).zip(visibility);
    super::emit_span(span(step, repo, Utc::now()));
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn judge(result: &'static str, reason: &'static str) -> StepDispatch<'static> {
        StepDispatch {
            item: 42,
            role: StepRole::Judge,
            result,
            reason,
            draw_attempt: 3,
            age_source: "labels_at",
            resumed: false,
            draw: Some(StepDrawTerms {
                weight: 4096,
                total_weight: 65_536,
                repo_items: 7,
                in_flight_repo: 2,
            }),
            story_points: None,
            sweep_id: Some("step-judge-42-1"),
            claim: None,
            assignment_id: None,
            lineage: None,
            captain: None,
            models: None,
            runtime: Some("claude"),
            effort: Some("high"),
            role_draw: None,
        }
    }

    #[test]
    fn a_step_span_carries_every_named_fact_and_passes_the_allowlist() {
        let span = span(&judge(RESULT_SPAWNED, "ok"), Some(("acme/w", "private")), Utc::now());
        assert_eq!(span.name.as_str(), "loom.dispatch.step");
        let attr = |k: &str| span.attributes.get(k).cloned();
        assert_eq!(attr("loom.repo").as_deref(), Some("acme/w"));
        assert_eq!(attr("loom.pr_number").as_deref(), Some("42"), "a Judge's item is a PR");
        assert_eq!(attr("loom.issue"), None);
        assert_eq!(attr("loom.role").as_deref(), Some("judge"));
        assert_eq!(attr("loom.dispatch.admission_result").as_deref(), Some("spawned"));
        assert_eq!(attr("loom.dispatch.reason").as_deref(), Some("ok"));
        assert_eq!(attr("loom.step.draw_attempt").as_deref(), Some("3"));
        assert_eq!(attr("loom.step.age_source").as_deref(), Some("labels_at"));
        assert_eq!(attr("loom.step.resumed").as_deref(), Some("false"));
        assert_eq!(attr("loom.step.weight").as_deref(), Some("4096"));
        assert_eq!(attr("loom.step.total_weight").as_deref(), Some("65536"));
        assert_eq!(attr("loom.step.repo_items").as_deref(), Some("7"));
        assert_eq!(attr("loom.step.in_flight_repo").as_deref(), Some("2"));
        for key in span
            .attributes
            .keys()
            .filter(|k| k.starts_with("loom.step."))
        {
            assert!(
                crate::telemetry::ops::OPS_SPAN_ATTRIBUTE_KEYS.contains(&key.as_str()),
                "{key} is dropped at export"
            );
        }
    }

    /// #12748: the span keeps the draw's selected model apart from the one
    /// actually launched.
    #[test]
    fn a_step_span_separates_the_selected_model_from_the_launched_one() {
        let step = StepDispatch {
            models: Some(StepModels {
                selected: Some("sonnet"),
                launched: Some("opus"),
                configured: None,
                fallback: None,
            }),
            ..judge(RESULT_SPAWNED, "ok")
        };
        let span = span(&step, None, Utc::now());
        assert_eq!(span.attributes["loom.step.model_selected"], "sonnet");
        assert_eq!(span.attributes["loom.step.model_launched"], "opus");
        for key in ["loom.step.model_selected", "loom.step.model_launched"] {
            assert!(crate::telemetry::ops::OPS_SPAN_ATTRIBUTE_KEYS.contains(&key));
        }
        let none = super::span(&judge(RESULT_SPAWNED, "ok"), None, Utc::now());
        assert!(!none.attributes.contains_key("loom.step.model_selected"));
        assert!(!none.attributes.contains_key("loom.step.model_launched"));
    }

    /// #13873: the span names the configured model, and — when a
    /// selection-time fallback fired — what it fell back from and why, so an
    /// operator can see how much work ran below its configured model.
    #[test]
    fn a_step_span_names_the_configured_model_and_any_fallback() {
        let step = StepDispatch {
            models: Some(StepModels {
                selected: None,
                launched: Some("opus"),
                configured: Some("fable"),
                fallback: Some(StepModelFallback {
                    from: "fable",
                    reason: "no-account-for-class",
                }),
            }),
            ..judge(RESULT_SPAWNED, "ok")
        };
        let span = span(&step, None, Utc::now());
        assert_eq!(span.attributes["loom.step.model_configured"], "fable");
        assert_eq!(span.attributes["loom.step.model_launched"], "opus");
        assert_eq!(span.attributes["loom.step.model_fallback_from"], "fable");
        assert_eq!(span.attributes["loom.step.model_fallback_reason"], "no-account-for-class");
        for key in [
            "loom.step.model_configured",
            "loom.step.model_fallback_from",
            "loom.step.model_fallback_reason",
        ] {
            assert!(
                crate::telemetry::ops::OPS_SPAN_ATTRIBUTE_KEYS.contains(&key),
                "{key} is dropped at export"
            );
        }
        // Without a fallback the two keys stay absent — `model_configured`
        // alone records the pre-fallback model.
        let steady = StepDispatch {
            models: Some(StepModels {
                selected: None,
                launched: Some("fable"),
                configured: Some("fable"),
                fallback: None,
            }),
            ..judge(RESULT_SPAWNED, "ok")
        };
        let steady_span = super::span(&steady, None, Utc::now());
        assert_eq!(steady_span.attributes["loom.step.model_configured"], "fable");
        assert!(!steady_span
            .attributes
            .contains_key("loom.step.model_fallback_from"));
        assert!(!steady_span
            .attributes
            .contains_key("loom.step.model_fallback_reason"));
    }

    /// #14167: every admitted step names the runtime it launches on and its
    /// resolved effort, so a per-step comparison joins `loom.dispatch.step`
    /// the same way `loom.runtime.run` does; a step whose launch resolved
    /// neither omits the keys rather than guessing. Both keys are on the
    /// lifecycle span allowlist and the collector `keep_keys` (the
    /// long-standing `loom.runtime.run` keys).
    #[test]
    fn an_admitted_step_names_its_runtime_and_effort() {
        let span = span(&judge(RESULT_SPAWNED, "ok"), None, Utc::now());
        assert_eq!(span.attributes["loom.runtime"], "claude");
        assert_eq!(span.attributes["loom.effort"], "high");
        let unset = super::span(
            &StepDispatch {
                runtime: None,
                effort: None,
                ..judge(RESULT_SPAWNED, "ok")
            },
            None,
            Utc::now(),
        );
        assert!(!unset.attributes.contains_key("loom.runtime"));
        assert!(!unset.attributes.contains_key("loom.effort"));
    }

    /// #14233: a step a `roleDraw` table decided carries the draw's pick and
    /// escalation attempt (not `draw_attempt`, the item redraw count); a step
    /// no table decided carries neither key, never `0` or `none`.
    #[test]
    fn a_role_drawn_step_names_the_pick_and_attempt() {
        use crate::runtime_preference::role_draw::{DrawWeight, Layer};
        let record = RoleDrawRecord {
            role: "judge".into(),
            attempt: 2,
            table_attempt: 2,
            layer: Layer::FleetRepo,
            table: vec![DrawWeight {
                candidate: "codex".into(),
                weight: 100,
            }],
            off: Vec::new(),
            seed: "00000000000000ff".into(),
            seed_inputs: None,
            steps: Vec::new(),
            passed_over: Vec::new(),
            pick: Some("codex".into()),
            model: None,
        };
        let step = StepDispatch {
            role_draw: Some(&record),
            ..judge(RESULT_SPAWNED, "ok")
        };
        let span = span(&step, None, Utc::now());
        assert_eq!(span.attributes["loom.role_draw.pick"], "codex");
        assert_eq!(span.attributes["loom.role_draw.attempt"], "2");
        assert_eq!(span.attributes["loom.step.draw_attempt"], "3", "item redraws stay apart");
        for key in ["loom.role_draw.pick", "loom.role_draw.attempt"] {
            assert!(
                crate::observability::lifecycle::role_draw::KEYS.contains(&key),
                "{key} is dropped at export"
            );
        }
        let undrawn = super::span(&judge(RESULT_SPAWNED, "ok"), None, Utc::now());
        assert!(!undrawn.attributes.contains_key("loom.role_draw.pick"));
        assert!(!undrawn.attributes.contains_key("loom.role_draw.attempt"));
    }

    /// #12450: a granted step carries the captain's assignment, and it
    /// survives the export allowlist; a step with none carries no key.
    #[test]
    fn a_granted_step_names_its_assignment() {
        let step = StepDispatch {
            assignment_id: Some("a-7"),
            ..judge(RESULT_SPAWNED, "ok")
        };
        let span = span(&step, None, Utc::now());
        assert_eq!(
            span.attributes
                .get("loom.assignment.id")
                .map(String::as_str),
            Some("a-7")
        );
        assert!(crate::telemetry::ops::OPS_SPAN_ATTRIBUTE_KEYS.contains(&"loom.assignment.id"));
        let none = super::span(&judge(RESULT_SPAWNED, "ok"), None, Utc::now());
        assert!(!none.attributes.contains_key("loom.assignment.id"));
    }

    /// #12875: a re-granted step names the attempt it replaces; a first
    /// grant carries none of the three keys.
    #[test]
    fn a_retried_step_names_its_predecessor() {
        let lineage = crate::fleet_dispatch::worker::GrantLineage {
            attempt: 2,
            prev_assignment_id: "a-6".into(),
            prev_end: "expired".into(),
        };
        let step = StepDispatch {
            assignment_id: Some("a-7"),
            lineage: Some(&lineage),
            ..judge(RESULT_SPAWNED, "ok")
        };
        let span = span(&step, None, Utc::now());
        assert_eq!(span.attributes["loom.dispatch.attempt"], "2");
        assert_eq!(span.attributes["loom.dispatch.prev_assignment_id"], "a-6");
        assert_eq!(span.attributes["loom.dispatch.prev_end"], "expired");
        let first = super::span(&judge(RESULT_SPAWNED, "ok"), None, Utc::now());
        for key in [
            "loom.dispatch.attempt",
            "loom.dispatch.prev_assignment_id",
            "loom.dispatch.prev_end",
        ] {
            assert!(crate::telemetry::ops::OPS_SPAN_ATTRIBUTE_KEYS.contains(&key));
            assert!(!first.attributes.contains_key(key), "{key} on a first grant");
        }
    }

    /// #12707: a captain refusal's holder and retry survive the allowlist; a
    /// cooldown adds the wall-clock end.
    #[test]
    fn a_captain_refusal_names_its_holder_and_retry() {
        let captain = crate::fleet_dispatch::worker::Refusal {
            held_by: Some("host-b".into()),
            retry_after_s: Some(60),
        };
        let at: DateTime<Utc> = "2026-10-17T12:00:00Z".parse().unwrap();
        let held = StepDispatch {
            captain: Some(&captain),
            ..judge(RESULT_REFUSED, "captain_held")
        };
        let s = span(&held, None, at);
        let attr = |k: &str| s.attributes.get(k).map(String::as_str);
        assert_eq!(attr("loom.dispatch.held_by"), Some("host-b"));
        assert_eq!(attr("loom.dispatch.retry_after_s"), Some("60"));
        assert_eq!(attr("loom.dispatch.hold_until"), None);
        let cooled = StepDispatch {
            captain: Some(&captain),
            ..judge(RESULT_REFUSED, "captain_cooldown")
        };
        let s = span(&cooled, None, at);
        assert_eq!(
            s.attributes
                .get("loom.dispatch.hold_until")
                .map(String::as_str),
            Some("2026-10-17T12:01:00Z")
        );
        for key in [
            "loom.dispatch.held_by",
            "loom.dispatch.retry_after_s",
            "loom.dispatch.hold_until",
        ] {
            assert!(
                crate::telemetry::ops::OPS_SPAN_ATTRIBUTE_KEYS.contains(&key),
                "{key} is dropped at export"
            );
        }
    }

    /// #12957: a sized step carries its points, and they survive the
    /// export allowlist; an unsized one has no attribute, never `0`.
    #[test]
    fn a_sized_step_carries_its_story_points_and_an_unsized_one_none() {
        let builder = StepDispatch {
            role: StepRole::Builder,
            story_points: Some(5),
            ..judge(RESULT_SPAWNED, "ok")
        };
        let span = span(&builder, None, Utc::now());
        assert_eq!(span.attributes.get(STORY_POINTS).map(String::as_str), Some("5"));
        assert!(crate::telemetry::ops::OPS_SPAN_ATTRIBUTE_KEYS.contains(&STORY_POINTS));
        // A PR step's points are its closing issue's, passed in the same field.
        let judge_step = StepDispatch {
            story_points: Some(8),
            ..judge(RESULT_REFUSED, "claim_lost")
        };
        let span = super::span(&judge_step, None, Utc::now());
        assert_eq!(span.attributes.get(STORY_POINTS).map(String::as_str), Some("8"));
        let unsized_step = super::span(&judge(RESULT_SPAWNED, "ok"), None, Utc::now());
        assert!(!unsized_step.attributes.contains_key(STORY_POINTS));
    }

    #[test]
    fn a_refused_issue_step_names_its_issue_and_reason() {
        let step = StepDispatch {
            role: StepRole::Curator,
            sweep_id: None,
            ..judge(RESULT_REFUSED, "claim_lost")
        };
        let span = span(&step, None, Utc::now());
        assert_eq!(span.attributes.get("loom.issue").map(String::as_str), Some("42"));
        assert_eq!(
            span.attributes
                .get("loom.dispatch.reason")
                .map(String::as_str),
            Some("claim_lost")
        );
        assert!(!span.attributes.contains_key("loom.repo"));
    }

    #[test]
    fn claim_facts_appear_only_when_set_and_pass_the_allowlist() {
        let bare = span(&judge(RESULT_REFUSED, "claim_unverified"), None, Utc::now());
        assert!(!bare
            .attributes
            .keys()
            .any(|k| k.starts_with("loom.dispatch.claim_")));
        let facts = crate::sweep_registry::step_spawn::lease::ClaimFacts {
            outcome: "exited",
            exit_code: Some(5),
            claim_ms: 1234,
            stderr_tail: Some("gh: rate limited".to_string()),
            rollback: Some("lease_yielded"),
        };
        let step = StepDispatch {
            claim: Some(&facts),
            ..judge(RESULT_REFUSED, "claim_unverified")
        };
        let span = span(&step, None, Utc::now());
        let attr = |k: &str| span.attributes.get(k).cloned();
        assert_eq!(attr("loom.dispatch.reason").as_deref(), Some("claim_unverified"));
        assert_eq!(attr("loom.dispatch.claim_outcome").as_deref(), Some("exited"));
        assert_eq!(attr("loom.dispatch.claim_exit_code").as_deref(), Some("5"));
        assert_eq!(attr("loom.dispatch.claim_ms").as_deref(), Some("1234"));
        assert_eq!(attr("loom.dispatch.claim_stderr_tail").as_deref(), Some("gh: rate limited"));
        assert_eq!(attr("loom.dispatch.claim_rollback").as_deref(), Some("lease_yielded"));
        for key in span.attributes.keys().filter(|k| k.contains("claim_")) {
            assert!(
                crate::telemetry::ops::OPS_SPAN_ATTRIBUTE_KEYS.contains(&key.as_str()),
                "{key} is dropped at export"
            );
        }
    }
}
