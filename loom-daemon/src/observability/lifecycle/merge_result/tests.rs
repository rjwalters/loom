#![allow(clippy::unwrap_used)]
use super::super::{begin, checkpoint_begun, finish_execution};
use super::*;
use crate::telemetry::trace::store::{CONTEXT_FILE_ENV, TRACEPARENT_ENV};
use crate::telemetry::trace::TraceAttributes;
use serial_test::serial;

fn sweep(dir: &std::path::Path) -> super::super::Span {
    std::fs::create_dir_all(dir.join(".loom")).unwrap();
    std::fs::write(
        dir.join(".loom/config.json"),
        r#"{"observability":{"enabled":true,"exporter":"otlp","endpoint":"http://127.0.0.1:4318"}}"#,
    )
    .unwrap();
    let span = begin(dir, "sweep-12029", SpanName::Sweep, TraceAttributes::new()).unwrap();
    std::env::set_var(CONTEXT_FILE_ENV, span.journal.path().with_extension("json"));
    std::env::set_var(TRACEPARENT_ENV, span.context().traceparent());
    std::env::remove_var("LOOM_WORKSPACE");
    span
}

fn drained(span: &super::super::Span) -> Vec<crate::telemetry::trace::SpanRecord> {
    let mut spans = Vec::new();
    span.journal
        .drain(|s| {
            spans.push(s);
            Ok(())
        })
        .unwrap();
    spans
}

fn result<'a>(pr: u32, result: &'a str) -> MergeResult<'a> {
    MergeResult {
        pr,
        head_sha: Some("ABCDEF1"),
        result,
        reason: Some("head-mismatch"),
        exit_code: Some(3),
    }
}

/// `begin N merge --pr-number 7` then `record-result` closes the attempt with
/// the PR, head and result, so execution end has nothing left to close as
/// `exit_unobserved`.
#[test]
#[serial]
fn a_merge_result_closes_the_open_merge_attempt() {
    let dir = tempfile::tempdir().unwrap();
    let span = sweep(dir.path());
    checkpoint_begun(dir.path(), 12029, "merge", None, None, Some(7));
    assert!(!record(dir.path(), &result(8, "refused")), "another PR's result closes nothing");
    assert!(!record(dir.path(), &result(7, "bogus")));
    assert!(record(dir.path(), &result(7, "refused")));
    assert!(!record(dir.path(), &result(7, "refused")), "closed once");
    finish_execution(dir.path(), "sweep-12029", "success", TraceAttributes::new()).unwrap();
    std::env::remove_var(CONTEXT_FILE_ENV);
    std::env::remove_var(TRACEPARENT_ENV);
    let spans = drained(&span);
    let merges: Vec<_> = spans
        .iter()
        .filter(|s| s.attributes.get("loom.role").is_some_and(|r| r == "merge"))
        .collect();
    assert_eq!(merges.len(), 2, "phase + attempt");
    for s in merges {
        assert_eq!(s.attributes["loom.result"], "refused");
        assert_eq!(s.attributes["loom.pr_number"], "7");
        assert_eq!(s.attributes["loom.head_sha"], "abcdef1");
        assert_eq!(s.attributes["loom.merge.reason"], "head-mismatch");
        assert_eq!(s.attributes["loom.exit_code"], "3");
        assert_eq!(s.attributes["loom.attempt.worked"], "true");
        assert!(s.attributes["loom.timing_source"] != "terminal_observed");
    }
}

#[test]
#[serial]
fn a_merged_result_is_ok_and_failed_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let span = sweep(dir.path());
    checkpoint_begun(dir.path(), 1, "merge", None, None, Some(7));
    assert!(record(
        dir.path(),
        &MergeResult {
            reason: None,
            exit_code: Some(0),
            ..result(7, "merged")
        }
    ));
    checkpoint_begun(dir.path(), 2, "merge", None, None, Some(9));
    assert!(record(dir.path(), &result(9, "failed")));
    std::env::remove_var(CONTEXT_FILE_ENV);
    std::env::remove_var(TRACEPARENT_ENV);
    let spans = drained(&span);
    let status = |pr: &str| {
        spans
            .iter()
            .find(|s| s.name == SpanName::RoleAttempt && s.attributes["loom.pr_number"] == pr)
            .unwrap()
            .status
    };
    assert_eq!(status("7"), SpanStatus::Ok);
    assert_eq!(status("9"), SpanStatus::Error);
}

/// Champion's and attended merges have no open sweep merge attempt: nothing
/// is closed and nothing is written.
#[test]
#[serial]
fn with_no_open_merge_attempt_it_is_a_no_op() {
    let dir = tempfile::tempdir().unwrap();
    let span = sweep(dir.path());
    assert!(!record(dir.path(), &result(7, "merged")));
    checkpoint_begun(dir.path(), 1, "builder", None, None, Some(7));
    assert!(!record(dir.path(), &result(7, "merged")), "only a merge attempt closes");
    std::env::remove_var(CONTEXT_FILE_ENV);
    std::env::remove_var(TRACEPARENT_ENV);
    assert!(!record(dir.path(), &result(7, "merged")), "no inherited trace");
    drop(span);
}
