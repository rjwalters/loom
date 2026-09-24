//! A successful sweep must export one strictly nested waterfall an operator can
//! reach from the issue number or the PR number alone.
//!
//! This is acceptance evidence for the first bullet of #8525. The repair
//! sequence (failed Judge, Doctor, second Judge) is already evidenced by
//! `lifecycle_traces.rs`; what was missing is the plain successful path, where
//! the interesting property is not "some spans exist" but the *shape*: every
//! phase hangs off the sweep root, every role attempt off its own phase,
//! nothing dangles, and every span in the waterfall carries the issue/PR
//! association that makes the trace findable from the forge side.
//!
//! Every arm drives the real `sweep-checkpoint` CLI against the real
//! [`loom_daemon::observability::lifecycle`] hook and drains the journal through
//! `lifecycle::backfill`, so the assertions are made against records that were
//! durably queued for export — not against the synthetic
//! `loom_daemon::telemetry::fixture` generator, which hand-builds span records
//! and so could not demonstrate what a sweep itself records.
#![cfg(feature = "otlp")]
#![allow(clippy::unwrap_used)]
use loom_daemon::{
    observability::{lifecycle, queue::DurableQueue},
    telemetry::{
        trace::{SpanName, SpanRecord, SpanStatus},
        TelemetryRecord,
    },
};
use serde_json::json;
use std::{
    collections::BTreeSet,
    path::Path,
    process::Command,
    sync::atomic::{AtomicU32, Ordering},
};

const ISSUE: &str = "8525";
const PR: &str = "8600";
const EXECUTION: &str = "sweep-issue-8525-successful";
/// A task id is operator-supplied free text routed through the same write as the
/// telemetry hook. It is not on the emission allowlist, so it must never reach a
/// span.
const TASK_SENTINEL: &str = "PRIVATE_TASK_ID_SENTINEL";

fn traced_root() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
    std::fs::write(
        dir.path().join(".loom/config.json"),
        json!({"observability":{"enabled":true,"exporter":"otlp","endpoint":"http://127.0.0.1:4318"}})
            .to_string(),
    )
    .unwrap();
    dir
}

fn command(root: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_loom-daemon"));
    cmd.current_dir(root)
        .env("LOOM_WORKSPACE", root)
        .env("LOOM_CONFIG_DEFAULTS_FILE", "")
        .env("LOOM_SHARED_API_KEYS_DIR", "")
        .env("LOOM_OBSERVABILITY_ENABLED", "true")
        .env("LOOM_OBSERVABILITY_EXPORTER", "otlp")
        .env("LOOM_OBSERVABILITY_ENDPOINT", "http://127.0.0.1:4318");
    cmd
}

/// Drain the journal exactly as the daemon does, so every assertion is made
/// against spans that were durably queued for export.
fn exported(root: &Path) -> Vec<SpanRecord> {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let queue = DurableQueue::open(
        root.join(format!("waterfall-queue-{}.jsonl", NEXT.fetch_add(1, Ordering::Relaxed))),
        10_000,
    );
    lifecycle::backfill(root, &queue);
    queue
        .peek_batch(10_000)
        .into_iter()
        .filter_map(|envelope| match envelope.record {
            TelemetryRecord::Span(span) => Some(span),
            _ => None,
        })
        .collect()
}

fn attribute<'a>(span: &'a SpanRecord, key: &str) -> &'a str {
    span.attributes
        .get(key)
        .map(String::as_str)
        .unwrap_or("<absent>")
}

/// The whole successful lifecycle: Curator through merge, no repair, each phase
/// recorded by the real checkpoint CLI under the dispatched sweep's context.
fn successful_sweep(root: &Path) -> Vec<SpanRecord> {
    let sweep = lifecycle::begin(
        root,
        EXECUTION,
        SpanName::Sweep,
        lifecycle::attributes(&[("loom.sweep_id", EXECUTION), ("loom.issue", ISSUE)]),
    )
    .expect("tracing is enabled in this workspace");
    for phase in ["curator-done", "builder-done", "judge-done", "merge-done"] {
        let mut cmd = command(root);
        cmd.args([
            "sweep-checkpoint",
            "write",
            ISSUE,
            phase,
            "--pr-number",
            PR,
            "--attempt",
            "1",
            "--model",
            "claude-opus-5",
            "--task-id",
            TASK_SENTINEL,
        ]);
        sweep.command(&mut cmd);
        let out = cmd.output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }
    lifecycle::finish_execution(root, EXECUTION, "success", Default::default());
    exported(root)
}

#[test]
fn a_successful_sweep_exports_one_strictly_nested_waterfall() {
    let dir = traced_root();
    let spans = successful_sweep(dir.path());

    let roots: Vec<_> = spans
        .iter()
        .filter(|s| s.parent_span_id.is_none())
        .collect();
    assert_eq!(roots.len(), 1, "a sweep is one trace with one root: {spans:?}");
    assert_eq!(roots[0].name, SpanName::Sweep);
    assert_eq!(attribute(roots[0], "loom.result"), "success");
    assert_eq!(roots[0].status, SpanStatus::Ok);

    let traces: BTreeSet<_> = spans
        .iter()
        .map(|s| s.context.trace_id.as_str().to_owned())
        .collect();
    assert_eq!(traces.len(), 1, "one sweep must not fragment into several traces");

    let phases: Vec<_> = spans.iter().filter(|s| s.name == SpanName::Phase).collect();
    let attempts: Vec<_> = spans
        .iter()
        .filter(|s| s.name == SpanName::RoleAttempt)
        .collect();
    assert_eq!(phases.len(), 4, "curator, builder, judge and merge: {spans:?}");
    assert_eq!(attempts.len(), 4, "each observed phase ran exactly one attempt: {spans:?}");
    assert_eq!(spans.len(), 9, "root + 4 phases + 4 attempts, nothing else: {spans:?}");

    // Nesting, asserted as a property of every span rather than by spot-check:
    // a phase hangs off the root, an attempt off its own phase, and no parent
    // reference dangles outside the exported set.
    let by_id: std::collections::BTreeMap<_, _> = spans
        .iter()
        .map(|s| (s.context.span_id.as_str().to_owned(), s))
        .collect();
    for span in &spans {
        let Some(parent_id) = span.parent_span_id.as_ref() else {
            continue;
        };
        let parent = by_id
            .get(parent_id.as_str())
            .unwrap_or_else(|| panic!("dangling parent on {span:?}"));
        let expected = match span.name {
            SpanName::Phase => SpanName::Sweep,
            SpanName::RoleAttempt => SpanName::Phase,
            other => panic!("unexpected span {other:?} in a checkpoint-only sweep"),
        };
        assert_eq!(parent.name, expected, "{span:?} is parented to the wrong level");
        assert!(span.started_at >= parent.started_at, "a child cannot precede its parent");
    }
    for phase in &phases {
        let children = attempts
            .iter()
            .filter(|a| a.parent_span_id.as_ref() == Some(&phase.context.span_id))
            .count();
        assert_eq!(children, 1, "each phase owns exactly one attempt: {phase:?}");
    }
    assert!(
        spans.iter().all(|s| s.status == SpanStatus::Ok),
        "nothing failed in this sweep: {spans:?}"
    );
}

#[test]
fn every_span_in_a_successful_sweep_resolves_to_its_issue_and_pr() {
    let dir = traced_root();
    let spans = successful_sweep(dir.path());

    // The forge-side question is "show me the trace for issue 8525" / "for PR
    // 8600". Both must select spans, and every selection must land in exactly
    // one trace — an ambiguous association is the same as none.
    for (key, value) in [("loom.issue", ISSUE), ("loom.pr_number", PR)] {
        let selected: Vec<_> = spans
            .iter()
            .filter(|s| attribute(s, key) == value)
            .collect();
        assert!(!selected.is_empty(), "{key} selects nothing: {spans:?}");
        let traces: BTreeSet<_> = selected
            .iter()
            .map(|s| s.context.trace_id.as_str().to_owned())
            .collect();
        assert_eq!(traces.len(), 1, "{key}={value} must identify one trace: {selected:?}");
    }
    // The association is on the work spans, not only on a root a log line may
    // never mention.
    for span in spans
        .iter()
        .filter(|s| matches!(s.name, SpanName::Phase | SpanName::RoleAttempt))
    {
        assert_eq!(attribute(span, "loom.issue"), ISSUE, "{span:?}");
        assert_eq!(attribute(span, "loom.pr_number"), PR, "{span:?}");
        assert_eq!(attribute(span, "loom.configured_model"), "claude-opus-5", "{span:?}");
        assert_eq!(attribute(span, "loom.attempt"), "1", "{span:?}");
    }

    // Phases are readable in the order they were actually observed, which is
    // what makes the waterfall a waterfall rather than a bag of spans.
    let mut observed: Vec<_> = spans
        .iter()
        .filter(|s| s.name == SpanName::Phase)
        .collect::<Vec<_>>();
    observed.sort_by_key(|s| s.started_at);
    let roles: Vec<_> = observed.iter().map(|s| attribute(s, "loom.role")).collect();
    assert_eq!(roles, ["curator", "builder", "judge", "merge"], "{observed:?}");
    let judge = observed[2];
    assert_eq!(attribute(judge, "loom.judge_verdict"), "approved");
    assert_eq!(
        attribute(judge, "loom.timing_source"),
        "checkpoint_write_observed",
        "a completion observation must not be reported as a measured phase interval"
    );
}

#[test]
fn operator_supplied_task_text_never_reaches_an_exported_span() {
    let dir = traced_root();
    let spans = successful_sweep(dir.path());
    let payload = serde_json::to_string(&spans).unwrap();
    assert!(
        !payload.contains(TASK_SENTINEL),
        "free-text metadata is not on the emission allowlist: {payload}"
    );
    // The durable checkpoint really did carry it, so the absence above is the
    // allowlist working rather than the value never having existed.
    let written = std::fs::read_to_string(
        dir.path()
            .join(".loom/sweep-checkpoint")
            .join(format!("issue-{ISSUE}.json")),
    )
    .unwrap();
    assert!(written.contains(TASK_SENTINEL), "{written}");
}
