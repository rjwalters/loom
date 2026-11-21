//! Report real release-tag lookups separately from cached provenance reads.

use std::time::Instant;

use chrono::{DateTime, Utc};

use super::Memory;
use crate::observability::ops;
use crate::release_provenance::{Provenance, RETRY_CAP};
use crate::telemetry::trace::{
    instant, SpanName, SpanRecord, SpanStatus, TraceAttributes, TraceContext,
};

pub(super) fn report(
    memory: &mut Memory,
    host: &str,
    found: &Provenance,
    attempt: Option<u32>,
    started_at: DateTime<Utc>,
) {
    if let Some(attempt) = attempt.filter(|_| ops::spans_exported()) {
        ops::emit_span(attempt_span(host, found, attempt, started_at, Utc::now().max(started_at)));
    }
    if should_log(memory, found, attempt, Instant::now()) {
        log::info!("workspace_resync: release provenance: {found}");
    }
}

fn should_log(memory: &mut Memory, found: &Provenance, attempt: Option<u32>, now: Instant) -> bool {
    if !matches!(found, Provenance::Unverified { .. }) {
        return memory.noted.insert(format!("provenance:{found}"));
    }
    // Backoff ticks are cached observations, not failures. While actual
    // failures continue, print at most once an hour, even if the reason varies.
    if attempt.is_none()
        || memory
            .provenance_failure_logged_at
            .is_some_and(|last| now.duration_since(last) < RETRY_CAP)
    {
        return false;
    }
    memory.provenance_failure_logged_at = Some(now);
    true
}

/// A root trace keyed by host, lookup start, and the process-local attempt
/// number. `retry_at` is present only when another lookup is scheduled.
fn attempt_span(
    host: &str,
    found: &Provenance,
    attempt: u32,
    started_at: DateTime<Utc>,
    ended_at: DateTime<Utc>,
) -> SpanRecord {
    let name = SpanName::WorkspaceResyncVerify;
    let ordinal = attempt.to_string();
    let context = TraceContext::derived(name.as_str(), &[host, &instant(started_at), &ordinal]);
    let (outcome, mut why, retry_at) = match found {
        Provenance::Verified => ("verified", "release tag names this build's commit".into(), None),
        Provenance::Mismatch { .. } => ("mismatch", found.to_string(), None),
        Provenance::NotStamped(why) => ("not_stamped", why.clone(), None),
        Provenance::Unverified { why, retry_at } => ("unverified", why.clone(), *retry_at),
    };
    // The lookup supplies fixed diagnostic categories and a validated tag.
    // Keep the value bounded for the telemetry attribute size limit.
    why.truncate(why.floor_char_boundary(256));
    let mut attributes = TraceAttributes::from([
        ("loom.workspace_resync.verify.host".to_owned(), host.to_owned()),
        ("loom.workspace_resync.verify.attempt".to_owned(), ordinal),
        ("loom.workspace_resync.verify.outcome".to_owned(), outcome.to_owned()),
        ("loom.workspace_resync.verify.why".to_owned(), why),
    ]);
    if let Some(retry_at) = retry_at {
        attributes.insert("loom.workspace_resync.verify.retry_at".to_owned(), instant(retry_at));
    }
    crate::telemetry::trace::provenance::stamp(&mut attributes);
    SpanRecord {
        context,
        parent_span_id: None,
        name,
        started_at,
        ended_at,
        attributes,
        status: if matches!(found, Provenance::Verified) {
            SpanStatus::Ok
        } else {
            SpanStatus::Error
        },
        events: Vec::new(),
        links: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failed(why: &str, retry_at: DateTime<Utc>) -> Provenance {
        Provenance::Unverified {
            why: why.into(),
            retry_at: Some(retry_at),
        }
    }

    #[test]
    fn failed_lookup_logs_resume_at_the_limit_and_cached_ticks_stay_quiet() {
        let at = Instant::now();
        let mut memory = Memory::default();
        let failure = failed("timeout", Utc::now());
        let other = failed("unparsable response", Utc::now());
        assert!(!should_log(&mut memory, &failure, None, at));
        assert!(should_log(&mut memory, &failure, Some(1), at));
        let second = std::time::Duration::from_secs(1);
        assert!(!should_log(&mut memory, &other, Some(2), at + RETRY_CAP - second));
        assert!(!should_log(&mut memory, &failure, None, at + RETRY_CAP));
        assert!(should_log(&mut memory, &failure, Some(3), at + RETRY_CAP));
        assert!(should_log(&mut memory, &Provenance::Verified, Some(4), at + RETRY_CAP));
        assert!(!should_log(&mut memory, &Provenance::Verified, None, at + RETRY_CAP));
    }

    #[test]
    fn reporting_emits_only_attempts_even_when_the_failure_log_is_throttled() {
        let at = Utc::now();
        let failure = failed("timeout", at + chrono::Duration::minutes(5));
        let ((), records) = ops::capture::capture(|| {
            let mut memory = Memory::default();
            report(&mut memory, "worker-a", &failure, Some(1), at);
            report(&mut memory, "worker-a", &failure, None, at);
            report(&mut memory, "worker-a", &failure, Some(2), at);
            report(&mut memory, "worker-a", &Provenance::Verified, Some(3), at);
            report(&mut memory, "worker-a", &Provenance::Verified, None, at);
        });
        let attempts: Vec<_> = records
            .spans
            .iter()
            .map(|span| span.attributes["loom.workspace_resync.verify.attempt"].as_str())
            .collect();
        assert_eq!(attempts, ["1", "2", "3"]);
    }

    #[test]
    fn attempts_have_distinct_host_scoped_traces_and_report_the_retry() {
        let at = DateTime::parse_from_rfc3339("2026-11-20T00:00:00Z")
            .unwrap()
            .to_utc();
        let ended_at = at + chrono::Duration::seconds(2);
        let retry_at = at + chrono::Duration::minutes(5);
        let found = failed("tag lookup timed out", retry_at);
        let span = attempt_span("worker-a", &found, 1, at, ended_at).bounded();
        assert_eq!(span.name.as_str(), "workspace_resync.verify");
        assert_eq!(span.status, SpanStatus::Error);
        assert_eq!((span.started_at, span.ended_at), (at, ended_at));
        assert!(span.parent_span_id.is_none());
        assert_eq!(span.attributes["loom.workspace_resync.verify.host"], "worker-a");
        assert_eq!(span.attributes["loom.workspace_resync.verify.attempt"], "1");
        assert_eq!(span.attributes["loom.workspace_resync.verify.outcome"], "unverified");
        assert_eq!(span.attributes["loom.workspace_resync.verify.why"], "tag lookup timed out");
        assert_eq!(span.attributes["loom.workspace_resync.verify.retry_at"], instant(retry_at));
        assert_ne!(span.context, attempt_span("worker-b", &found, 1, at, ended_at).context);

        let recovered = attempt_span("worker-a", &Provenance::Verified, 2, retry_at, retry_at);
        assert_ne!(span.context, recovered.context);
        assert_eq!(recovered.status, SpanStatus::Ok);
        assert_eq!(recovered.attributes["loom.workspace_resync.verify.attempt"], "2");
        assert_eq!(recovered.attributes["loom.workspace_resync.verify.outcome"], "verified");
        assert!(!recovered
            .attributes
            .contains_key("loom.workspace_resync.verify.retry_at"));

        let mismatch = Provenance::Mismatch {
            tag_commit: "a".repeat(40),
        };
        let terminal = attempt_span("worker-a", &mismatch, 2, retry_at, retry_at);
        assert_eq!(terminal.status, SpanStatus::Error);
        assert_eq!(terminal.attributes["loom.workspace_resync.verify.outcome"], "mismatch");
        assert!(
            terminal.attributes["loom.workspace_resync.verify.why"].contains("tag names commit")
        );
        assert!(!terminal
            .attributes
            .contains_key("loom.workspace_resync.verify.retry_at"));
    }
}
