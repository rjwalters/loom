//! The `forge.reader.spill` span (W4-B): one per spill-latch transition.
//!
//! A repo's reads normally go to their home reader. When the home bucket is
//! projected to run dry, or is withdrawn, the spill latch moves part or all
//! of that repo's reads to a reader with headroom until the home bucket
//! resets ([`crate::forge_identity::route`]). Each transition — engaged
//! (`partial` / `full`) or released (`off`) — is one instant span naming the
//! repo, the bucket's resource, the home reader (`from`), the target (`to`,
//! `home` when none has headroom or on release) and the release instant.
//! The latch pins its target; when a held latch has to re-pick one (the
//! pinned target was withdrawn, went stale or reached `spillFullPct`), that
//! re-pick is one more span with the latch's current mode and the new `to`.
//! Spills should be rare (a few per repo per day); this is how that is
//! verified.
//!
//! Its own root trace, with IDs derived from the latch and the instant
//! (`trace-identity.md`). Never emitted when no OTLP exporter is registered.

use chrono::{DateTime, SecondsFormat, Utc};

use crate::telemetry::trace::{SpanName, SpanRecord, SpanStatus, TraceAttributes, TraceContext};

/// One latch transition, as the span reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spill<'a> {
    /// `owner/repo`.
    pub owner_repo: &'a str,
    /// `core` / `graphql` / `search`.
    pub resource: &'a str,
    /// The home reader's App id.
    pub from: &'a str,
    /// The target reader's App id, or `home`.
    pub to: &'a str,
    /// `partial` / `full` / `off`.
    pub mode: &'a str,
    /// When the latch releases (the transition instant for `off`).
    pub until: DateTime<Utc>,
}

/// The span for `s`, taken at `at`.
#[must_use]
pub fn spill_span(s: &Spill<'_>, at: DateTime<Utc>) -> SpanRecord {
    let mut attributes = TraceAttributes::new();
    attributes.insert("forge.spill.owner_repo".into(), s.owner_repo.to_string());
    attributes.insert("forge.spill.resource".into(), s.resource.to_string());
    attributes.insert("forge.spill.from".into(), s.from.to_string());
    attributes.insert("forge.spill.to".into(), s.to.to_string());
    attributes.insert("forge.spill.mode".into(), s.mode.to_string());
    attributes
        .insert("forge.spill.until".into(), s.until.to_rfc3339_opts(SecondsFormat::Secs, true));
    crate::telemetry::trace::provenance::stamp(&mut attributes);
    SpanRecord {
        context: TraceContext::derived(
            SpanName::ForgeReaderSpill.as_str(),
            &[
                s.owner_repo,
                s.resource,
                s.from,
                s.mode,
                &crate::telemetry::trace::instant(at),
            ],
        ),
        parent_span_id: None,
        name: SpanName::ForgeReaderSpill,
        started_at: at,
        ended_at: at,
        status: SpanStatus::Ok,
        attributes,
        events: Vec::new(),
        links: Vec::new(),
    }
}

/// Export one transition. A no-op when no ops sink is registered.
pub fn record_spill(s: &Spill<'_>) {
    if !super::spans_exported() {
        return;
    }
    super::emit_span(spill_span(s, Utc::now()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_span_carries_every_attribute_and_keeps_them_through_export() {
        let at = DateTime::parse_from_rfc3339("2026-10-05T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let until = at + chrono::Duration::minutes(40);
        let span = spill_span(
            &Spill {
                owner_repo: "acme/hot",
                resource: "core",
                from: "1001",
                to: "1002",
                mode: "partial",
                until,
            },
            at,
        );
        assert_eq!(span.name.as_str(), "forge.reader.spill");
        assert_eq!(span.attributes["forge.spill.mode"], "partial");
        assert_eq!(span.attributes["forge.spill.until"], "2026-10-05T12:40:00Z");
        for key in [
            "forge.spill.owner_repo",
            "forge.spill.resource",
            "forge.spill.from",
            "forge.spill.to",
            "forge.spill.mode",
            "forge.spill.until",
        ] {
            assert!(span.attributes.contains_key(key), "{key}");
            assert!(
                crate::telemetry::ops::OPS_SPAN_ATTRIBUTE_KEYS.contains(&key),
                "{key} must be an allowed span attribute"
            );
        }
        // Deterministic IDs: the same transition derives the same trace.
        let again = spill_span(
            &Spill {
                owner_repo: "acme/hot",
                resource: "core",
                from: "1001",
                to: "1002",
                mode: "partial",
                until,
            },
            at,
        );
        assert_eq!(span.context, again.context);
    }
}
