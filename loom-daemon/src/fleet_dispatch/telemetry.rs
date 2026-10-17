//! The captain's fleet-dispatch telemetry (#12449; spec #12452 §12).
//!
//! - One `loom.fleet_dispatch.call` span per call: the calling host, the
//!   endpoint, latency, the HTTP status, the result (`granted` / `refused` /
//!   `ok` / `error`), the reason, `server_epoch`, the assignment id, and
//!   `loom.repo` / `loom.issue` or `loom.pr_number` when an item is named.
//!   `loom.fleet_dispatch.side` is `captain` here; the worker's span of the
//!   same name says `worker`.
//! - Counters, accumulated and flushed as delta points by the server's sweep
//!   task: `loom.fleet_dispatch.refusals` by `reason` (claim refusals and
//!   `refuse` tokens, `kind` = `claim` / `refuse`; `reason` is bounded to the
//!   known tokens, anything else is `other`),
//!   `loom.fleet_dispatch.expired` by `kind` (`assignment` / `hold`) and
//!   `loom.fleet_dispatch.claim_lost_after_grant`.
//!
//! Everything goes through the daemon's ops sink
//! ([`crate::observability::ops`]); with no OTLP exporter it is a no-op.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};

use super::proto::{RefusalReason, REFUSE_REASONS};
use crate::step_role::StepRole;
use crate::telemetry::ops::{MetricName, MetricPoint};
use crate::telemetry::trace::{SpanName, SpanRecord, SpanStatus, TraceAttributes, TraceContext};

/// Span attribute keys the call span carries beyond the lifecycle allowlist.
/// Listed in [`crate::telemetry::ops::OPS_SPAN_ATTRIBUTE_KEYS`] and the
/// gateway collector's `keep_keys`.
pub const KEYS: [&str; 9] = [
    "loom.fleet_dispatch.side",
    "loom.fleet_dispatch.host",
    "loom.fleet_dispatch.endpoint",
    "loom.fleet_dispatch.latency_ms",
    "loom.fleet_dispatch.status",
    "loom.fleet_dispatch.result",
    "loom.fleet_dispatch.reason",
    "loom.fleet_dispatch.server_epoch",
    "loom.assignment.id",
];

/// One call, as the span reports it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Call {
    /// The calling host, once identified.
    pub host: Option<String>,
    /// The request path (a fixed route, or `other`).
    pub endpoint: String,
    pub status: u16,
    /// `granted` / `refused` / `ok` / `error`.
    pub result: &'static str,
    /// The refusal reason, `refuse` token, release outcome or error class.
    pub reason: Option<String>,
    pub server_epoch: String,
    pub assignment_id: Option<String>,
    /// `(repo, item, step)` when the call names an item.
    pub item: Option<(String, u64, String)>,
    pub latency_ms: u64,
}

/// The span for `call`, received at `started_at`. Its own root trace; `seq`
/// keeps two calls in one instant apart. Pure.
#[must_use]
pub fn span(call: &Call, started_at: DateTime<Utc>, seq: u64) -> SpanRecord {
    let name = SpanName::FleetDispatchCall;
    let instant = crate::telemetry::trace::instant(started_at);
    let seq = seq.to_string();
    let host = call.host.as_deref().unwrap_or("-");
    let context = TraceContext::derived(
        name.as_str(),
        &[
            "captain",
            host,
            call.endpoint.as_str(),
            instant.as_str(),
            seq.as_str(),
        ],
    );
    let mut a = TraceAttributes::new();
    a.insert("loom.fleet_dispatch.side".into(), "captain".into());
    a.insert("loom.fleet_dispatch.host".into(), host.to_string());
    a.insert("loom.fleet_dispatch.endpoint".into(), call.endpoint.clone());
    a.insert("loom.fleet_dispatch.latency_ms".into(), call.latency_ms.to_string());
    a.insert("loom.fleet_dispatch.status".into(), call.status.to_string());
    a.insert("loom.fleet_dispatch.result".into(), call.result.to_string());
    if let Some(reason) = &call.reason {
        a.insert("loom.fleet_dispatch.reason".into(), reason.clone());
    }
    a.insert("loom.fleet_dispatch.server_epoch".into(), call.server_epoch.clone());
    if let Some(id) = &call.assignment_id {
        a.insert("loom.assignment.id".into(), id.clone());
    }
    if let Some((repo, item, step)) = &call.item {
        a.insert("loom.repo".into(), repo.clone());
        let on_pr = step.parse::<StepRole>().is_ok_and(StepRole::acts_on_pr);
        let key = if on_pr {
            "loom.pr_number"
        } else {
            "loom.issue"
        };
        a.insert(key.into(), item.to_string());
    }
    crate::telemetry::trace::provenance::stamp(&mut a);
    let ended_at = started_at
        + chrono::Duration::milliseconds(i64::try_from(call.latency_ms).unwrap_or(i64::MAX / 2));
    SpanRecord {
        context,
        parent_span_id: None,
        name,
        started_at,
        ended_at,
        status: if call.result == "error" {
            SpanStatus::Error
        } else {
            SpanStatus::Ok
        },
        attributes: a,
        events: Vec::new(),
        links: Vec::new(),
    }
}

/// Counters accumulated between flushes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Counters {
    /// `(kind, reason)` → count.
    pub refusals: BTreeMap<(&'static str, String), i64>,
    pub expired_assignments: i64,
    pub expired_holds: i64,
    pub claim_lost_after_grant: i64,
}

impl Counters {
    /// Count a claim refusal.
    pub fn claim_refused(&mut self, reason: RefusalReason) {
        *self
            .refusals
            .entry(("claim", reason.as_str().to_string()))
            .or_default() += 1;
    }

    /// Count a claim refused `held` by a no-progress hold (#12541), under
    /// its own reason instead of the plain `held`.
    pub fn claim_cooled(&mut self) {
        *self
            .refusals
            .entry(("claim", super::server::NO_PROGRESS_REASON.to_string()))
            .or_default() += 1;
    }

    /// Count a `refuse` call's token: one of [`REFUSE_REASONS`], or `other`.
    /// The token is worker-supplied, so it is never a label as sent.
    pub fn refused(&mut self, reason: &str) {
        let label = REFUSE_REASONS
            .iter()
            .find(|r| **r == reason)
            .copied()
            .unwrap_or(RefusalReason::Other.as_str());
        *self
            .refusals
            .entry(("refuse", label.to_string()))
            .or_default() += 1;
    }

    /// Count an expiry pass's removals.
    pub fn expired(&mut self, e: super::table::Expired) {
        self.expired_assignments += i64::from(e.assignments);
        self.expired_holds += i64::from(e.holds);
    }

    /// The delta points since the last flush, resetting the counters.
    pub fn take_points(&mut self) -> Vec<MetricPoint> {
        let c = std::mem::take(self);
        let mut points: Vec<MetricPoint> = c
            .refusals
            .into_iter()
            .map(|((kind, reason), n)| {
                MetricPoint::int(MetricName::FleetDispatchRefusals, n)
                    .label("kind", kind)
                    .label("reason", reason)
            })
            .collect();
        for (kind, n) in [
            ("assignment", c.expired_assignments),
            ("hold", c.expired_holds),
        ] {
            if n > 0 {
                points.push(
                    MetricPoint::int(MetricName::FleetDispatchExpired, n).label("kind", kind),
                );
            }
        }
        if c.claim_lost_after_grant > 0 {
            points.push(MetricPoint::int(
                MetricName::FleetDispatchClaimLostAfterGrant,
                c.claim_lost_after_grant,
            ));
        }
        points
    }
}

/// Emit `call`'s span through the ops sink (a no-op without one).
pub fn emit_call(call: &Call, started_at: DateTime<Utc>, seq: u64) {
    if crate::observability::ops::spans_exported() {
        crate::observability::ops::emit_span(span(call, started_at, seq));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn claim_call() -> Call {
        Call {
            host: Some("worker-a".into()),
            endpoint: "/v1/claim".into(),
            status: 200,
            result: "refused",
            reason: Some("held".into()),
            server_epoch: "e1".into(),
            assignment_id: None,
            item: Some(("acme/w".into(), 42, "judge".into())),
            latency_ms: 3,
        }
    }

    #[test]
    fn the_call_span_names_every_fact_and_survives_the_export_allowlist() {
        let s = span(&claim_call(), Utc::now(), 1);
        assert_eq!(s.name.as_str(), "loom.fleet_dispatch.call");
        let attr = |k: &str| s.attributes.get(k).cloned();
        assert_eq!(attr("loom.fleet_dispatch.side").as_deref(), Some("captain"));
        assert_eq!(attr("loom.fleet_dispatch.host").as_deref(), Some("worker-a"));
        assert_eq!(attr("loom.fleet_dispatch.endpoint").as_deref(), Some("/v1/claim"));
        assert_eq!(attr("loom.fleet_dispatch.result").as_deref(), Some("refused"));
        assert_eq!(attr("loom.fleet_dispatch.reason").as_deref(), Some("held"));
        assert_eq!(attr("loom.fleet_dispatch.server_epoch").as_deref(), Some("e1"));
        assert_eq!(attr("loom.fleet_dispatch.latency_ms").as_deref(), Some("3"));
        assert_eq!(attr("loom.repo").as_deref(), Some("acme/w"));
        assert_eq!(attr("loom.pr_number").as_deref(), Some("42"), "a Judge acts on a PR");
        assert!(s.validate().is_ok());
        for key in KEYS {
            assert!(
                crate::telemetry::ops::OPS_SPAN_ATTRIBUTE_KEYS.contains(&key),
                "{key} is dropped at export"
            );
        }
    }

    #[test]
    fn an_issue_step_names_its_issue_and_an_error_is_an_error_span() {
        let mut call = claim_call();
        call.item = Some(("acme/w".into(), 7, "builder".into()));
        call.result = "error";
        let s = span(&call, Utc::now(), 2);
        assert_eq!(s.attributes.get("loom.issue").map(String::as_str), Some("7"));
        assert_eq!(s.status, SpanStatus::Error);
    }

    #[test]
    fn counters_flush_as_labelled_deltas_once() {
        let mut c = Counters::default();
        c.claim_refused(RefusalReason::Held);
        c.claim_refused(RefusalReason::Held);
        c.refused("claim_lost");
        c.expired(super::super::table::Expired {
            assignments: 2,
            holds: 1,
        });
        c.claim_lost_after_grant += 1;
        let points = c.take_points();
        assert_eq!(points.len(), 5);
        let held = points
            .iter()
            .find(|p| p.labels.get("reason").map(String::as_str) == Some("held"))
            .unwrap();
        assert_eq!(held.name, MetricName::FleetDispatchRefusals);
        assert_eq!(held.labels.get("kind").map(String::as_str), Some("claim"));
        assert!(c.take_points().is_empty(), "flushed counters reset");
    }

    #[test]
    fn an_unknown_refuse_token_is_counted_as_other() {
        let mut c = Counters::default();
        c.refused("head_moved");
        for junk in ["x-1", "x-2", "", "claim_lost "] {
            c.refused(junk);
        }
        let labels: Vec<(&str, &str, i64)> = c
            .refusals
            .iter()
            .map(|((k, r), n)| (*k, r.as_str(), *n))
            .collect();
        assert_eq!(labels, vec![("refuse", "head_moved", 1), ("refuse", "other", 4)]);
    }
}
