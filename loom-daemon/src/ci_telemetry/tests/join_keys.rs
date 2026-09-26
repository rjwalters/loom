//! Join keys on CI spans (Issue #9007): `head_sha`, `ref`, `pr_number`.
//!
//! A sibling module of `super` rather than more lines in it: `tests.rs` is at
//! the file-size ratchet, and this is a self-contained concern (the same
//! split #8825's [`super::job_logs`] and #8898's [`super::rerun_window`]
//! made).

use super::*;
use crate::ci_telemetry::records::{JobJson, PullRequestRefJson, RunJson};

/// A minimal, otherwise-default `RunJson` for the `pr_number()`/span-attribute
/// tests below — only the fields each test cares about are varied.
fn bare_run() -> RunJson {
    serde_json::from_value(serde_json::json!({
        "id": 42,
        "name": "CI",
        "head_branch": "main",
        "head_sha": "abc123",
        "event": "push",
        "status": "completed",
        "conclusion": "success",
        "run_attempt": 1,
        "created_at": "2026-09-20T09:00:00Z",
        "run_started_at": "2026-09-20T09:00:10Z",
        "updated_at": "2026-09-20T09:01:10Z",
    }))
    .unwrap()
}

fn bare_repo() -> RepoJson {
    RepoJson {
        name: "example".into(),
        full_name: "fixture-org/example".into(),
        private: false,
        archived: false,
    }
}

fn bare_job() -> JobJson {
    serde_json::from_value(serde_json::json!({
        "id": 7,
        "name": "build",
        "status": "completed",
        "conclusion": "success",
        "started_at": "2026-09-20T09:00:10Z",
        "completed_at": "2026-09-20T09:01:00Z",
        "labels": ["ubuntu-latest"],
        "run_attempt": 1,
    }))
    .unwrap()
}

#[test]
fn pr_number_prefers_the_pull_requests_array_over_the_branch_fallback() {
    let mut run = bare_run();
    run.head_branch = Some("feature/issue-123".into());
    run.pull_requests = vec![PullRequestRefJson { number: 999 }];
    // A true PR number wins even though the branch also parses to an issue.
    assert_eq!(run.pr_number(), Some(999));
}

#[test]
fn pr_number_falls_back_to_the_branch_name_when_no_pull_request_is_reported() {
    let mut run = bare_run();
    run.head_branch = Some("feature/issue-456".into());
    assert!(run.pull_requests.is_empty());
    assert_eq!(run.pr_number(), Some(456));
}

#[test]
fn pr_number_is_absent_not_zero_when_neither_source_resolves() {
    let mut run = bare_run();
    run.head_branch = Some("main".into());
    assert_eq!(run.pr_number(), None);

    run.head_branch = None;
    assert_eq!(run.pr_number(), None);
}

fn span_attrs(env: &TelemetryEnvelope) -> Option<&crate::telemetry::trace::TraceAttributes> {
    match &env.record {
        TelemetryRecord::Span(s) => Some(&s.attributes),
        _ => None,
    }
}

#[test]
fn run_span_carries_head_sha_and_ref_always_and_pr_number_when_derivable() {
    let repo = bare_repo();
    let mut run = bare_run();
    run.head_branch = Some("feature/issue-789".into());
    let envelopes = run_envelopes(&repo, &run, "test-host");
    let span = envelopes
        .iter()
        .find_map(span_attrs)
        .expect("a span envelope");
    assert_eq!(span.get("loom.ci.head_sha"), Some(&"abc123".to_string()));
    assert_eq!(span.get("loom.ci.ref"), Some(&"feature/issue-789".to_string()));
    assert_eq!(span.get("loom.pr_number"), Some(&"789".to_string()));
}

#[test]
fn run_span_omits_pr_number_when_it_cannot_be_derived() {
    let repo = bare_repo();
    let mut run = bare_run();
    run.head_branch = Some("main".into());
    let envelopes = run_envelopes(&repo, &run, "test-host");
    let span = envelopes
        .iter()
        .find_map(span_attrs)
        .expect("a span envelope");
    assert_eq!(span.get("loom.ci.head_sha"), Some(&"abc123".to_string()));
    assert_eq!(span.get("loom.ci.ref"), Some(&"main".to_string()));
    assert!(!span.contains_key("loom.pr_number"));
}

#[test]
fn job_span_carries_the_same_join_keys_as_its_run() {
    let repo = bare_repo();
    let mut run = bare_run();
    run.pull_requests = vec![PullRequestRefJson { number: 555 }];
    let job = bare_job();
    let envelopes = job_envelopes(&repo, &run, &job, "test-host");
    let span = envelopes
        .iter()
        .find_map(span_attrs)
        .expect("a span envelope");
    assert_eq!(span.get("loom.ci.head_sha"), Some(&"abc123".to_string()));
    assert_eq!(span.get("loom.ci.ref"), Some(&"main".to_string()));
    assert_eq!(span.get("loom.pr_number"), Some(&"555".to_string()));
}
