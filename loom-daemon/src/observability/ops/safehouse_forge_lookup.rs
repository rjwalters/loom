//! The `safehouse.forge_lookup.failed` span (#13156): one per safehouse `gh`
//! read that produced no answer.
//!
//! `log_gh_failure_once` warns once per `(call, workspace)` and then drops to
//! `debug`, so a workspace whose narration has stopped looked the same as a
//! quiet one. This span is exported on **every** failure, so an alert can key
//! on sustained failures per workspace. The one-shot `warn` is unchanged.
//!
//! Only the failure arm of `run_sink_gh` emits it: a `gh` that ran and exited
//! nonzero is an answer, not an `Unavailable`. An instant span, its own root
//! trace, with IDs derived from the call, repo and instant
//! (`trace-identity.md`). No stderr, path or free text. Never emitted when no
//! OTLP exporter is registered.

use chrono::{DateTime, Utc};

use crate::cmd_out::Unavailable;
use crate::telemetry::trace::{SpanName, SpanRecord, SpanStatus, TraceAttributes, TraceContext};

/// The reason class a failed lookup is reported under.
#[must_use]
pub fn reason_class(u: &Unavailable) -> &'static str {
    match u {
        Unavailable::TimedOut { .. } => "timed_out",
        Unavailable::Shed { .. } => "shed",
        _ => "unavailable",
    }
}

/// The span for a failed `call` in `repo`, failing for `reason` at `at`.
#[must_use]
pub fn failed_span(call: &str, repo: &str, reason: &str, at: DateTime<Utc>) -> SpanRecord {
    let mut attributes = TraceAttributes::new();
    attributes.insert("safehouse.call".into(), call.to_string());
    attributes.insert("loom.repo".into(), repo.to_string());
    attributes.insert("safehouse.reason".into(), reason.to_string());
    crate::telemetry::trace::provenance::stamp(&mut attributes);
    SpanRecord {
        context: TraceContext::derived(
            SpanName::SafehouseForgeLookupFailed.as_str(),
            &[call, repo, &crate::telemetry::trace::instant(at)],
        ),
        parent_span_id: None,
        name: SpanName::SafehouseForgeLookupFailed,
        started_at: at,
        ended_at: at,
        status: SpanStatus::Ok,
        attributes,
        events: Vec::new(),
        links: Vec::new(),
    }
}

/// Export one failed lookup. A no-op when no ops sink is registered.
pub fn record_failed(call: &str, repo: &str, u: &Unavailable) {
    if !super::spans_exported() {
        return;
    }
    super::emit_span(failed_span(call, repo, reason_class(u), Utc::now()));
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use super::*;

    #[test]
    fn outcomes_map_to_the_three_reason_classes() {
        let timed_out = Unavailable::TimedOut {
            after: Duration::from_secs(1),
            partial_stdout: vec![],
        };
        let shed = Unavailable::Shed {
            until: SystemTime::now(),
        };
        assert_eq!(reason_class(&timed_out), "timed_out");
        assert_eq!(reason_class(&shed), "shed");
        assert_eq!(reason_class(&Unavailable::Spawn("no gh".into())), "unavailable");
    }

    #[test]
    fn the_span_carries_exactly_the_attributes_and_they_survive_keep_keys() {
        let span = failed_span("pr list", "acme/widgets", "timed_out", Utc::now());
        assert_eq!(span.name, SpanName::SafehouseForgeLookupFailed);
        assert_eq!(span.attributes["safehouse.call"], "pr list");
        assert_eq!(span.attributes["loom.repo"], "acme/widgets");
        assert_eq!(span.attributes["safehouse.reason"], "timed_out");
        for key in ["safehouse.call", "loom.repo", "safehouse.reason"] {
            assert!(
                crate::telemetry::ops::OPS_SPAN_ATTRIBUTE_KEYS.contains(&key),
                "{key} must survive the collector's keep_keys"
            );
        }
    }

    #[test]
    fn a_repeat_failure_for_the_same_call_and_repo_is_a_new_span() {
        let t = Utc::now();
        let first = failed_span("pr list", "acme/widgets", "shed", t);
        let later =
            failed_span("pr list", "acme/widgets", "shed", t + chrono::Duration::seconds(60));
        assert_ne!(first.context, later.context);
    }
}
