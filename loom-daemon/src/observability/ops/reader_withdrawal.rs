//! The `forge.reader.withdrawn` span (W4-A): one per reader withdrawal.
//!
//! A reader App is withdrawn from routing after a failure. Since W4-A that
//! withdrawal is scoped to one `(app, owner, resource)` bucket wherever the
//! failure allows, so a dry pool for one owner no longer takes the reader off
//! every other owner. This span is how that is verified in production: every
//! withdrawal says which App, which owner, which resource (or `all`, or `app`
//! for an App-wide withdrawal), until when, and where the end time came from
//! (`header`, `probe` or `default`).
//!
//! An instant span, its own root trace, with IDs derived from the withdrawn
//! bucket and the instant (`trace-identity.md`). Never emitted when no OTLP
//! exporter is registered.

use chrono::{DateTime, SecondsFormat, Utc};

use crate::telemetry::trace::{SpanName, SpanRecord, SpanStatus, TraceAttributes, TraceContext};

/// One reader withdrawal, as the span reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Withdrawn<'a> {
    /// The reader App id.
    pub app: &'a str,
    /// The owner (lowercased); `-` for an App-wide withdrawal.
    pub owner: &'a str,
    /// `core` / `graphql` / `search` / `all`, or `app` for an App-wide one.
    pub resource: &'a str,
    pub until: DateTime<Utc>,
    /// `header` / `probe` / `default`.
    pub source: &'a str,
    /// Whether a secondary (abuse/concurrency) limit caused it.
    pub secondary: bool,
}

/// The span for `w`, withdrawn at `at`.
#[must_use]
pub fn withdrawn_span(w: &Withdrawn<'_>, at: DateTime<Utc>) -> SpanRecord {
    let mut attributes = TraceAttributes::new();
    attributes.insert("forge.reader.app".into(), w.app.to_string());
    attributes.insert("forge.reader.owner".into(), w.owner.to_string());
    attributes.insert("forge.reader.resource".into(), w.resource.to_string());
    attributes
        .insert("forge.reader.until".into(), w.until.to_rfc3339_opts(SecondsFormat::Secs, true));
    attributes.insert("forge.reader.source".into(), w.source.to_string());
    attributes.insert("forge.reader.secondary".into(), w.secondary.to_string());
    crate::telemetry::trace::provenance::stamp(&mut attributes);
    SpanRecord {
        context: TraceContext::derived(
            SpanName::ForgeReaderWithdrawn.as_str(),
            &[
                w.app,
                w.owner,
                w.resource,
                &crate::telemetry::trace::instant(at),
            ],
        ),
        parent_span_id: None,
        name: SpanName::ForgeReaderWithdrawn,
        started_at: at,
        ended_at: at,
        status: SpanStatus::Ok,
        attributes,
        events: Vec::new(),
        links: Vec::new(),
    }
}

/// Export one withdrawal. A no-op when no ops sink is registered.
pub fn record_withdrawn(w: &Withdrawn<'_>) {
    if !super::spans_exported() {
        return;
    }
    super::emit_span(withdrawn_span(w, Utc::now()));
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn the_span_names_the_bucket_its_end_and_where_the_end_came_from() {
        let at = DateTime::parse_from_rfc3339("2030-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let w = Withdrawn {
            app: "42",
            owner: "acme",
            resource: "core",
            until: at + chrono::Duration::minutes(20),
            source: "header",
            secondary: false,
        };
        let span = withdrawn_span(&w, at);
        assert_eq!(span.name, SpanName::ForgeReaderWithdrawn);
        assert!(span.parent_span_id.is_none(), "its own root");
        let get = |k: &str| span.attributes.get(k).map(String::as_str);
        assert_eq!(get("forge.reader.app"), Some("42"));
        assert_eq!(get("forge.reader.owner"), Some("acme"));
        assert_eq!(get("forge.reader.resource"), Some("core"));
        assert_eq!(get("forge.reader.until"), Some("2030-01-01T00:20:00Z"));
        assert_eq!(get("forge.reader.source"), Some("header"));
        assert_eq!(get("forge.reader.secondary"), Some("false"));
        let again = withdrawn_span(&w, at);
        assert_eq!(span.context, again.context, "IDs are derived, not random");
    }
}
