//! The `workspace_resync.landing` span (#13674): one instant span per landing
//! attempt on a resync PR, so "who landed this resync PR, by which path, with
//! what result" is answerable from telemetry alone.
//!
//! Attributes: `github.repo`, `github.number`, `loom.head_sha`, `path`
//! (`queue_handoff` | `waiver`), `outcome` (`Queued` | `Waiting` | `Failed` |
//! `Merged` | `NotQueueMode`), `reason`, `gate_paths_included`.
//!
//! The waiver path's [`Waived`] maps onto the same outcome vocabulary:
//! `Landed` -> `Merged` (the daemon merged it on the proof), `Lane` ->
//! `Failed` (not provable or not green; left to the ordinary lane), `Held` ->
//! `Waiting` (a gate says not now). `gate_paths_included` is `false` until
//! #13665 lets a gate path land.

use chrono::{DateTime, Utc};

use super::waiver::Waived;
use crate::forge_merge_queue::gh_lifecycle::Landed;
use crate::telemetry::trace::{
    instant, SpanName, SpanRecord, SpanStatus, TraceAttributes, TraceContext,
};

/// Which route a landing attempt took.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LandingPath {
    QueueHandoff,
    Waiver,
}

impl LandingPath {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::QueueHandoff => "queue_handoff",
            Self::Waiver => "waiver",
        }
    }
}

/// The `(outcome, reason)` of a queue hand-off.
#[must_use]
pub fn queue_outcome(landed: &Landed) -> (&'static str, String) {
    match landed {
        Landed::NotQueueMode(why) => ("NotQueueMode", why.clone()),
        Landed::Waiting(why) => ("Waiting", why.clone()),
        Landed::Queued => ("Queued", String::new()),
        Landed::Merged => ("Merged", String::new()),
        Landed::Failed(why) => ("Failed", why.clone()),
    }
}

/// The `(outcome, reason)` of a waiver attempt.
#[must_use]
pub fn waiver_outcome(waived: &Waived) -> (&'static str, String) {
    match waived {
        Waived::Landed(why) => ("Merged", why.clone()),
        Waived::Lane(why) => ("Failed", why.clone()),
        Waived::Held(why) => ("Waiting", why.clone()),
    }
}

/// Attribute values are bounded (256 bytes): keep the reason within it.
fn bounded(reason: &str) -> String {
    let mut out: String = reason.chars().take(200).collect();
    if out.len() > 240 {
        out.truncate(240);
    }
    out
}

/// Build the span for one attempt. `number` is `None` only when the forge
/// reply carried no PR number; the attribute is then omitted, never guessed.
#[must_use]
pub fn landing_span(
    nwo: &str,
    number: Option<u64>,
    head_sha: &str,
    path: LandingPath,
    outcome: &str,
    reason: &str,
    at: DateTime<Utc>,
) -> SpanRecord {
    let name = SpanName::WorkspaceResyncLanding;
    let n = number.map(|n| n.to_string()).unwrap_or_default();
    let context = TraceContext::derived(
        name.as_str(),
        &[nwo, &n, head_sha, path.as_str(), outcome, &instant(at)],
    );
    let mut attributes = TraceAttributes::new();
    let mut put = |k: &str, v: String| {
        attributes.insert(k.to_string(), v);
    };
    put("github.repo", nwo.to_string());
    if let Some(n) = number {
        put("github.number", n.to_string());
    }
    put("loom.head_sha", head_sha.to_string());
    put("path", path.as_str().to_string());
    put("outcome", outcome.to_string());
    put("reason", bounded(reason));
    put("gate_paths_included", "false".to_string());
    crate::telemetry::trace::provenance::stamp(&mut attributes);
    SpanRecord {
        context,
        parent_span_id: None,
        name,
        started_at: at,
        ended_at: at,
        status: if outcome == "Failed" {
            SpanStatus::Error
        } else {
            SpanStatus::Ok
        },
        attributes,
        events: Vec::new(),
        links: Vec::new(),
    }
}

/// Emit the span for one attempt (when spans are exported on this host).
pub fn emit(
    nwo: &str,
    number: Option<u64>,
    head_sha: &str,
    path: LandingPath,
    (outcome, reason): (&str, String),
) {
    if crate::observability::ops::spans_exported() {
        crate::observability::ops::emit_span(landing_span(
            nwo,
            number,
            head_sha,
            path,
            outcome,
            &reason,
            Utc::now(),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-07T00:00:00Z")
            .unwrap()
            .into()
    }

    #[test]
    fn queue_variants_map_to_the_vocabulary() {
        let cases = [
            (Landed::NotQueueMode("direct".into()), "NotQueueMode", "direct"),
            (Landed::Waiting("no approval; x".into()), "Waiting", "no approval; x"),
            (Landed::Queued, "Queued", ""),
            (Landed::Merged, "Merged", ""),
            (Landed::Failed("refused".into()), "Failed", "refused"),
        ];
        for (landed, outcome, reason) in cases {
            assert_eq!(queue_outcome(&landed), (outcome, reason.to_string()));
        }
    }

    #[test]
    fn waiver_variants_map_to_the_vocabulary() {
        assert_eq!(waiver_outcome(&Waived::Landed("m".into())).0, "Merged");
        assert_eq!(waiver_outcome(&Waived::Lane("l".into())).0, "Failed");
        assert_eq!(waiver_outcome(&Waived::Held("h".into())).0, "Waiting");
    }

    #[test]
    fn span_carries_the_attributes() {
        let sha = "a".repeat(40);
        let s = landing_span("o/r", Some(7), &sha, LandingPath::Waiver, "Merged", "ok", at());
        assert_eq!(s.name.as_str(), "workspace_resync.landing");
        assert_eq!(s.attributes["github.repo"], "o/r");
        assert_eq!(s.attributes["github.number"], "7");
        assert_eq!(s.attributes["loom.head_sha"], sha);
        assert_eq!(s.attributes["path"], "waiver");
        assert_eq!(s.attributes["outcome"], "Merged");
        assert_eq!(s.attributes["reason"], "ok");
        assert_eq!(s.attributes["gate_paths_included"], "false");
        assert_eq!(s.status, SpanStatus::Ok);
    }

    #[test]
    fn absent_number_is_omitted_and_failure_is_an_error() {
        let s = landing_span("o/r", None, "x", LandingPath::QueueHandoff, "Failed", "no", at());
        assert!(!s.attributes.contains_key("github.number"));
        assert_eq!(s.attributes["path"], "queue_handoff");
        assert_eq!(s.status, SpanStatus::Error);
    }

    #[test]
    fn long_reason_is_bounded() {
        let long = "é".repeat(500);
        let s = landing_span("o/r", Some(1), "x", LandingPath::Waiver, "Waiting", &long, at());
        assert!(s.attributes["reason"].len() <= 240);
    }
}
