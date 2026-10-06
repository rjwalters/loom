#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use super::*;
use crate::gh_invocation::{
    AccessIntent, GhBinSource, GhCompletion, GhInvocation, GhTarget, Operation, ParentContext,
};
use crate::observability::ops::capture::capture;
use crate::telemetry::trace::journal::Journal;
use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

fn op(target: GhTarget, timeout: Duration) -> GhInvocation {
    GhInvocation::new(Operation::new("issue.list"), AccessIntent::Read, target, timeout)
        .parent(ParentContext::Missing)
}

fn read_op() -> GhInvocation {
    op(GhTarget::repo("acme/widgets").unwrap(), Duration::from_secs(10))
}

/// A stub `gh` running `body`, which may print `$TRACEPARENT`.
fn stub(dir: &Path, body: &str) -> String {
    let path = dir.join("gh-stub");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_string_lossy().into_owned()
}

/// Execute `inv` against `body` with the ops path captured and the failure
/// sink pointed at a fresh dir: `(completion, spans, failure records)`.
fn run(
    inv: GhInvocation,
    body: &str,
) -> (
    Result<GhCompletion, crate::proc_exec::ExecError>,
    Vec<SpanRecord>,
    Vec<FailureRecord>,
) {
    let tmp = tempfile::tempdir().unwrap();
    let records = tmp.path().join("records");
    set_test_record_dir(Some(records.clone()));
    let program = stub(tmp.path(), body);
    let (result, captured) = capture(|| inv.execute_with(&program, GhBinSource::EnvOverride));
    set_test_record_dir(None);
    (result, captured.spans, read_records(&records))
}

fn attr<'a>(span: &'a SpanRecord, key: &str) -> Option<&'a str> {
    span.attributes.get(key).map(String::as_str)
}

fn stdout_of(result: Result<GhCompletion, crate::proc_exec::ExecError>) -> String {
    match result.unwrap() {
        GhCompletion::Captured(Completion::Exited(out)) => {
            String::from_utf8_lossy(&out.stdout).into_owned()
        }
        other => panic!("expected a captured exit, got {other:?}"),
    }
}

#[test]
fn outcome_vocabulary_is_closed_and_matches_serde() {
    let names: BTreeSet<_> = Outcome::ALL.iter().map(|o| o.as_str()).collect();
    assert_eq!(names.len(), Outcome::ALL.len(), "duplicate outcome name");
    for outcome in Outcome::ALL {
        let json = serde_json::to_string(&outcome).unwrap();
        assert_eq!(json, format!("\"{}\"", outcome.as_str()));
    }
}

#[test]
fn a_successful_invocation_emits_one_ok_span_and_no_failure_record() {
    let (result, spans, records) = run(read_op(), "echo ok");
    assert!(matches!(
        result,
        Ok(GhCompletion::Captured(ref c)) if c.succeeded()
    ));
    assert_eq!(spans.len(), 1, "exactly one invoke github span");
    let span = &spans[0];
    assert_eq!(span.name, SpanName::GithubInvoke);
    assert_eq!(span.name.as_str(), "invoke github");
    assert_eq!(span.status, SpanStatus::Ok);
    assert_eq!(attr(span, "github.operation"), Some("issue.list"));
    assert_eq!(attr(span, "github.access_intent"), Some("read"));
    assert_eq!(attr(span, "github.target"), Some("acme/widgets"));
    assert_eq!(attr(span, "github.outcome"), Some("ok"));
    assert_eq!(attr(span, "github.exit_code"), Some("0"));
    assert_eq!(attr(span, "github.launcher"), Some("env_override"));
    assert!(attr(span, "loom.daemon.revision").is_some(), "provenance stamped");
    assert!(span.ended_at >= span.started_at);
    assert!(records.is_empty(), "{records:?}");
}

#[test]
fn a_parentless_invocation_is_its_own_root_and_records_context_source_missing() {
    let (result, spans, _) = run(read_op(), "echo \"tp=${TRACEPARENT:-unset}\"");
    let span = &spans[0];
    assert_eq!(attr(span, "context_source"), Some("missing"));
    assert_eq!(span.parent_span_id, None);
    // The span is exported, so the child is handed the span's own context.
    assert!(stdout_of(result).contains(&format!("tp={}", span.context.traceparent())));
}

#[test]
fn a_parented_invocation_is_a_child_of_the_callers_span() {
    let parent = TraceContext::derived("sweep", &["acme/widgets", "sweep-issue-1-1"]);
    let inv = read_op().parent(ParentContext::Parent(parent.clone()));
    let (result, spans, _) = run(inv, "echo \"tp=${TRACEPARENT:-unset}\"");
    let span = &spans[0];
    assert_eq!(attr(span, "context_source"), Some("parent"));
    assert_eq!(span.context.trace_id, parent.trace_id, "joins the sweep's trace");
    assert_eq!(span.parent_span_id.as_ref(), Some(&parent.span_id));
    assert_ne!(span.context.span_id, parent.span_id);
    assert!(stdout_of(result).contains(&format!("tp={}", span.context.traceparent())));
}

#[test]
fn without_an_exporter_the_child_gets_the_callers_context_not_a_phantom_span() {
    let parent = TraceContext::derived("sweep", &["acme/widgets", "sweep-issue-2-1"]);
    let inv = read_op().parent(ParentContext::Parent(parent.clone()));
    let tmp = tempfile::tempdir().unwrap();
    let program = stub(tmp.path(), "echo \"tp=${TRACEPARENT:-unset}\"");
    // No capture, no global sink, no ambient journal: nothing is exported.
    let out = stdout_of(inv.execute_with(&program, GhBinSource::EnvOverride));
    assert!(out.contains(&format!("tp={}", parent.traceparent())), "{out}");
}

#[test]
fn span_ids_are_deterministic_and_distinct_per_invocation() {
    let parent = TraceContext::derived("sweep", &["acme/widgets", "sweep-issue-3-1"]);
    let inv = read_op().parent(ParentContext::Parent(parent));
    let at = Utc::now();
    let a = InvocationSpan::open_at(&inv, at, "100.1".into());
    let again = InvocationSpan::open_at(&inv, at, "100.1".into());
    let b = InvocationSpan::open_at(&inv, at, "100.2".into());
    assert_eq!(a.context, again.context, "same facts, same IDs");
    assert_ne!(a.context.span_id, b.context.span_id, "same instant, distinct spans");

    let root = read_op();
    let r1 = InvocationSpan::open_at(&root, at, "100.1".into());
    let r2 = InvocationSpan::open_at(&root, at, "100.2".into());
    assert_ne!(r1.context.trace_id, r2.context.trace_id, "ad-hoc roots never share a trace");
    assert_eq!(r1.context, InvocationSpan::open_at(&root, at, "100.1".into()).context);
}

#[test]
fn concurrent_invocations_never_share_a_span() {
    let inv = read_op();
    let spans: Vec<_> = (0..16).map(|_| InvocationSpan::open(&inv)).collect();
    let ids: BTreeSet<_> = spans
        .iter()
        .map(|s| s.context.span_id.as_str().to_string())
        .collect();
    assert_eq!(ids.len(), spans.len());
}

#[test]
fn a_non_zero_exit_leaves_a_local_completion_record() {
    let (result, spans, records) = run(read_op(), "echo nope >&2; exit 7");
    assert!(result.is_ok(), "a non-zero exit is reported, not turned into an error");
    assert_eq!(attr(&spans[0], "github.outcome"), Some("exit_nonzero"));
    assert_eq!(attr(&spans[0], "github.exit_code"), Some("7"));
    assert_eq!(spans[0].status, SpanStatus::Error);
    assert_eq!(records.len(), 1, "{records:?}");
    let r = &records[0];
    assert_eq!(r.outcome, Outcome::ExitNonzero);
    assert_eq!(r.exit_code, Some(7));
    assert_eq!(r.operation, "issue.list");
    assert_eq!(r.access_intent, "read");
    assert_eq!(r.target, "acme/widgets");
    assert_eq!(r.context_source, "missing");
    assert_eq!(r.span_id, spans[0].context.span_id.as_str());
}

#[test]
fn a_launcher_routing_refusal_is_its_own_outcome() {
    let body = format!("echo 'gh-exec: {ROUTING_REFUSAL_MARKER}: api.github.com' >&2; exit 1");
    let (_, spans, records) = run(read_op(), &body);
    assert_eq!(attr(&spans[0], "github.outcome"), Some("routing_refused"));
    assert_eq!(records[0].outcome, Outcome::RoutingRefused);
}

#[test]
fn a_timeout_leaves_a_local_completion_record() {
    let inv = op(GhTarget::None, Duration::from_millis(300));
    let (result, spans, records) = run(inv, "sleep 5");
    assert!(matches!(result, Ok(GhCompletion::Captured(Completion::TimedOut { .. }))));
    assert_eq!(attr(&spans[0], "github.outcome"), Some("timeout"));
    assert_eq!(attr(&spans[0], "github.exit_code"), None);
    assert_eq!(attr(&spans[0], "github.target"), Some("none"));
    assert_eq!(records[0].outcome, Outcome::Timeout);
}

#[test]
fn a_spawn_failure_leaves_a_local_completion_record() {
    let tmp = tempfile::tempdir().unwrap();
    let records_dir = tmp.path().join("records");
    set_test_record_dir(Some(records_dir.clone()));
    let missing = tmp.path().join("no-such-gh");
    let (result, captured) =
        capture(|| read_op().execute_with(&missing.to_string_lossy(), GhBinSource::Path));
    set_test_record_dir(None);
    assert!(matches!(result, Err(crate::proc_exec::ExecError::Spawn(_))));
    assert_eq!(captured.spans.len(), 1);
    assert_eq!(attr(&captured.spans[0], "github.outcome"), Some("spawn_failed"));
    assert_eq!(attr(&captured.spans[0], "github.launcher"), Some("path"));
    assert_eq!(read_records(&records_dir)[0].outcome, Outcome::SpawnFailed);
}

#[test]
fn passthrough_runs_are_recorded_without_reading_their_output() {
    let inv = read_op().passthrough();
    let (result, spans, records) = run(inv, "exit 3");
    assert!(matches!(result, Ok(GhCompletion::Passthrough(_))));
    assert_eq!(attr(&spans[0], "github.outcome"), Some("exit_nonzero"));
    assert_eq!(records[0].exit_code, Some(3));
}

#[test]
fn a_sweep_child_journals_its_span_into_the_ambient_execution_trace() {
    let tmp = tempfile::tempdir().unwrap();
    let sweep = TraceContext::derived("sweep", &["acme/widgets", "sweep-issue-4-1"]);
    let context_file = tmp
        .path()
        .join("trace-context")
        .join("sweep-issue-4-1.json");
    set_test_ambient(Some(&sweep.traceparent()), Some(context_file.clone()));
    // The default parent is the ambient one.
    let inv = GhInvocation::new(
        Operation::new("pr.view"),
        AccessIntent::Read,
        GhTarget::None,
        Duration::from_secs(10),
    );
    assert_eq!(inv.context_source(), "parent");
    let program = stub(tmp.path(), "echo \"tp=${TRACEPARENT:-unset}\"");
    let out = stdout_of(inv.execute_with(&program, GhBinSource::EnvOverride));
    set_test_ambient(None, None);

    let spans = Journal::for_context(&context_file).completed().unwrap();
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].name, SpanName::GithubInvoke);
    assert_eq!(spans[0].context.trace_id, sweep.trace_id);
    assert_eq!(spans[0].parent_span_id.as_ref(), Some(&sweep.span_id));
    assert!(out.contains(&format!("tp={}", spans[0].context.traceparent())), "{out}");
}

#[test]
fn ambient_parsing_rejects_garbage_and_never_reads_third_party_traceparent() {
    assert_eq!(parse_ambient(Some("not-a-traceparent"), None), (None, None));
    assert_eq!(parse_ambient(None, Some(PathBuf::new())), (None, None));
    let ctx = TraceContext::derived("sweep", &["x"]);
    assert_eq!(parse_ambient(Some(&ctx.traceparent()), None).0, Some(ctx));
}

#[test]
fn every_span_attribute_survives_export_bounding() {
    let inv = read_op();
    let span = InvocationSpan::open(&inv).record(
        &inv,
        GhBinSource::Policy,
        Outcome::ExitNonzero,
        Some(4),
        Utc::now(),
    );
    let bounded = span.clone().bounded();
    for key in SPAN_ATTRIBUTE_KEYS {
        assert_eq!(bounded.attributes.get(*key), span.attributes.get(*key), "{key}");
        assert!(span.attributes.contains_key(*key), "record() never sets {key}");
    }
}

#[test]
fn spans_carry_github_api_kind() {
    let rest = read_op().args(["api", "repos/acme/widgets/issues"]);
    let gql = read_op().args(["api", "graphql", "-f", "query=x"]);
    let porcelain = read_op().args(["pr", "list"]);
    let run_list = read_op().args(["run", "list"]);
    // #10344 review: REST porcelain that was mislabelled `graphql`.
    let latest_release = read_op().args(["release", "view", "--json", "tagName"]);
    let search = read_op().args(["search", "prs", "is:open"]);
    let tagged_release = read_op().args(["release", "view", "v1.0.0"]);
    for (inv, want) in [
        (rest, "rest"),
        (gql, "graphql"),
        (porcelain, "graphql"),
        (run_list, "rest"),
        (latest_release, "rest"),
        (search, "rest"),
        (tagged_release, "mixed"),
    ] {
        let (_, spans, _) = run(inv, "echo ok");
        assert_eq!(attr(&spans[0], "github.api"), Some(want));
    }
}
