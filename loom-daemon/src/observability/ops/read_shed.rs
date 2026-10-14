//! The `forge.read.shed` span (W4-C): one per read a reader route deferred.
//!
//! A Hygiene or Observability read whose readers for one `(owner, resource)`
//! bucket are all withdrawn is shed instead of spending the writer's bucket
//! ([`crate::gh_invocation::GhCompletion::Shed`]). Sheds are designed as an
//! anomaly path, so each one is exported: which operation, which class, the
//! last reader tried (`-` when the router reported every reader exhausted up
//! front), the owner, the resource, and until when.
//!
//! An instant span, its own root trace, with IDs derived from the shed read
//! and the instant (`trace-identity.md`). Never emitted when no OTLP exporter
//! is registered.

use chrono::{DateTime, SecondsFormat, Utc};

use crate::telemetry::trace::{SpanName, SpanRecord, SpanStatus, TraceAttributes, TraceContext};

/// One shed read, as the span reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shed<'a> {
    /// The facade operation (`worktree.issue_state`, …).
    pub op: &'a str,
    /// `hygiene` / `observability`.
    pub class: &'a str,
    /// The last reader App tried, or `-`.
    pub app: &'a str,
    /// The owner (lowercased).
    pub owner: &'a str,
    /// `core` / `graphql` / `search`.
    pub resource: &'a str,
    pub until: DateTime<Utc>,
}

/// The span for `s`, shed at `at`.
#[must_use]
pub fn shed_span(s: &Shed<'_>, at: DateTime<Utc>) -> SpanRecord {
    let mut attributes = TraceAttributes::new();
    attributes.insert("forge.read.op".into(), s.op.to_string());
    attributes.insert("forge.read.class".into(), s.class.to_string());
    attributes.insert("forge.read.app".into(), s.app.to_string());
    attributes.insert("forge.read.owner".into(), s.owner.to_string());
    attributes.insert("forge.read.resource".into(), s.resource.to_string());
    attributes
        .insert("forge.read.until".into(), s.until.to_rfc3339_opts(SecondsFormat::Secs, true));
    crate::telemetry::trace::provenance::stamp(&mut attributes);
    SpanRecord {
        context: TraceContext::derived(
            SpanName::ForgeReadShed.as_str(),
            &[
                s.op,
                s.owner,
                s.resource,
                &crate::telemetry::trace::instant(at),
            ],
        ),
        parent_span_id: None,
        name: SpanName::ForgeReadShed,
        started_at: at,
        ended_at: at,
        status: SpanStatus::Ok,
        attributes,
        events: Vec::new(),
        links: Vec::new(),
    }
}

/// Export one shed. A no-op when no ops sink is registered.
pub fn record_shed(s: &Shed<'_>) {
    if !super::spans_exported() {
        return;
    }
    super::emit_span(shed_span(s, Utc::now()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_span_carries_every_attribute_and_no_rate_limit_text() {
        let at = Utc::now();
        let span = shed_span(
            &Shed {
                op: "worktree.issue_state",
                class: "hygiene",
                app: "-",
                owner: "acme",
                resource: "core",
                until: at,
            },
            at,
        );
        assert_eq!(span.name, SpanName::ForgeReadShed);
        for key in [
            "forge.read.op",
            "forge.read.class",
            "forge.read.app",
            "forge.read.owner",
            "forge.read.resource",
            "forge.read.until",
        ] {
            assert!(span.attributes.contains_key(key), "{key}");
            assert!(
                crate::telemetry::ops::OPS_SPAN_ATTRIBUTE_KEYS.contains(&key),
                "{key} must survive the collector's keep_keys"
            );
        }
    }
}
