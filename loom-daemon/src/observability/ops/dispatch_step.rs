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

use crate::step_role::StepRole;
use crate::telemetry::trace::{SpanName, SpanRecord, SpanStatus, TraceAttributes, TraceContext};

/// `loom.dispatch.admission_result` of a step that launched.
pub const RESULT_SPAWNED: &str = "spawned";
/// `loom.dispatch.admission_result` of a step a gate or its claim refused.
pub const RESULT_REFUSED: &str = "refused";

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
    /// The step's sweep id, once one was assigned.
    pub sweep_id: Option<&'a str>,
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
    // A PR step's item is a PR; the issue key would name a story that does
    // not exist (`sweep_registry/step_story.rs`).
    let item_key = if step.role.acts_on_pr() {
        "loom.pr_number"
    } else {
        "loom.issue"
    };
    attributes.insert(item_key.into(), item.clone());
    attributes.insert("loom.step.item".into(), item);
    attributes.insert("loom.role".into(), step.role.role_name().to_string());
    attributes.insert("loom.step.role".into(), step.role.as_str().to_string());
    attributes.insert("loom.dispatch.admission_result".into(), step.result.to_string());
    attributes.insert("loom.dispatch.reason".into(), step.reason.to_string());
    attributes.insert("loom.step.draw_attempt".into(), step.draw_attempt.to_string());
    attributes.insert("loom.step.age_source".into(), step.age_source.to_string());
    attributes.insert("loom.step.resumed".into(), step.resumed.to_string());
    if let Some(id) = step.sweep_id {
        attributes.insert("loom.sweep_id".into(), id.to_string());
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
    let visibility = repo.as_ref().map(|r| match r.visibility {
        crate::telemetry::RepoVisibility::Public => "public",
        crate::telemetry::RepoVisibility::Private => "private",
    });
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
            sweep_id: Some("step-judge-42-1"),
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
}
