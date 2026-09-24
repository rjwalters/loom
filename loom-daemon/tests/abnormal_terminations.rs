//! Preflight rejection, cancellation and crash must stay mutually
//! distinguishable, and none of them may fabricate a model-run span.
//!
//! This is acceptance evidence for the third bullet of #8525. Every arm drives
//! the **real** instrumentation — the same
//! [`loom_daemon::observability::lifecycle`] entry points a dispatched sweep
//! uses, the same journal, the same `lifecycle::backfill` drain onto a
//! `DurableQueue`, and for the rejection arm the actually-built `spawn-worker`
//! CLI. None of it uses the synthetic `loom_daemon::telemetry::fixture`
//! generator, which hand-builds span records and so could not demonstrate what
//! the lifecycle itself records.
//!
//! The property under test is not "each arm looks plausible" but the
//! comparative one: an operator reading a trace must be able to tell *which*
//! abnormal ending occurred, must never see a `loom.runtime.run` span for a
//! model run that was never launched, and must never see an `Ok` one whose exit
//! nobody observed.
#![cfg(feature = "otlp")]
#![allow(clippy::unwrap_used)]
use loom_daemon::{
    observability::{lifecycle, queue::DurableQueue},
    telemetry::{
        trace::{journal::Journal, store::TraceStore, SpanName, SpanRecord, SpanStatus},
        TelemetryRecord,
    },
};
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU32, Ordering},
    sync::OnceLock,
};

/// A workspace whose resolved observability config actually enables tracing.
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

/// Drain the journal exactly as the daemon does, so every assertion below is
/// made against spans that were durably queued for export — not against
/// in-memory handles the exporter would never see.
fn exported(root: &Path) -> Vec<SpanRecord> {
    // A distinct queue file per call: two arms must never read each other's
    // records.
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let queue = DurableQueue::open(
        root.join(format!("terminations-queue-{}.jsonl", NEXT.fetch_add(1, Ordering::Relaxed))),
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

fn root_span(spans: &[SpanRecord]) -> &SpanRecord {
    let roots: Vec<_> = spans
        .iter()
        .filter(|s| s.parent_span_id.is_none())
        .collect();
    assert_eq!(roots.len(), 1, "an execution has exactly one root: {spans:?}");
    roots[0]
}

fn result_of(span: &SpanRecord) -> &str {
    span.attributes
        .get("loom.result")
        .map(String::as_str)
        .unwrap_or("<absent>")
}

/// Start the sweep root plus the phase/attempt pair a dispatched role produces,
/// leaving all three active. Returns the started attempt span.
fn begin_attempt(root: &Path, execution: &str, role: &str) -> lifecycle::Span {
    let metadata = lifecycle::attributes(&[
        ("loom.role", role),
        ("loom.phase", role),
        ("loom.issue", "8525"),
        ("loom.timing_source", "owned_boundary"),
    ]);
    let sweep = lifecycle::begin(
        root,
        execution,
        SpanName::Sweep,
        lifecycle::attributes(&[("loom.sweep_id", execution), ("loom.issue", "8525")]),
    )
    .expect("tracing is enabled in this workspace");
    let phase = sweep.child(SpanName::Phase, metadata.clone()).unwrap();
    phase.child(SpanName::RoleAttempt, metadata).unwrap()
}

/// Arm 1 — cancelled before the runtime was launched. The authoritative
/// terminal result belongs to the root; the phase/attempt spans that were still
/// open when the cancellation arrived have an unobserved ending, not a failed
/// one.
fn cancelled_before_launch(root: &Path) -> Vec<SpanRecord> {
    let attempt = begin_attempt(root, "cancelled-pre", "builder");
    attempt
        .child(SpanName::RuntimePreflight, Default::default())
        .unwrap()
        .finish("accepted", SpanStatus::Ok);
    lifecycle::finish_execution(root, "cancelled-pre", "cancelled", Default::default());
    exported(root)
}

/// Arm 2 — cancelled after the runtime was launched but before its exit was
/// seen. The model-run span exists because a launch really happened; its
/// outcome is unknown, and inventing `success` here is the specific failure
/// this arm exists to catch.
fn cancelled_after_launch(root: &Path) -> Vec<SpanRecord> {
    let attempt = begin_attempt(root, "cancelled-post", "builder");
    let run = attempt
        .child(SpanName::RuntimeRun, Default::default())
        .unwrap();
    Journal::for_context(&TraceStore::new(root).path(root, "cancelled-post"))
        .set_owner(run.context(), std::process::id())
        .unwrap();
    // No child_exited: the operator cancelled while the child was still live.
    lifecycle::finish_execution(root, "cancelled-post", "cancelled", Default::default());
    exported(root)
}

/// Arm 3 — the supervising daemon died with the execution still open, so no
/// terminal result was ever recorded. Recovery closes the journal at the time
/// the loss is *seen*, marked recovered and unknown.
#[cfg(unix)]
fn crashed_without_a_terminal_record(root: &Path) -> Vec<SpanRecord> {
    let attempt = begin_attempt(root, "crashed", "builder");
    attempt
        .child(SpanName::RuntimeRun, Default::default())
        .unwrap();
    let journal = Journal::for_context(&TraceStore::new(root).path(root, "crashed"));
    // A PID that provably belongs to no live process: spawn a child, reap it,
    // and reuse its number — the technique the lifecycle unit tests use.
    let mut child = Command::new("/usr/bin/true").spawn().unwrap();
    let gone = child.id();
    child.wait().unwrap();
    for span in journal.active().unwrap() {
        journal.set_owner(&span.record.context, gone).unwrap();
        journal.set_supervisor(&span.record.context, gone).unwrap();
    }
    // Deliberately no finish_execution: nobody survived to write one.
    exported(root)
}

/// The `spawn-worker` preflight resolves a runtime binary before it can judge
/// capability, so the rejection arm needs a real executable to point at.
#[cfg(unix)]
fn runtime_fixture() -> &'static PathBuf {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let bin = tempfile::tempdir().unwrap().keep().join("harness");
        assert!(Command::new("rustc")
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/worker_cli.rs"))
            .arg("-o")
            .arg(&bin)
            .status()
            .unwrap()
            .success());
        bin
    })
}

/// Arm 4 — the runtime refused the work. No model ran, so no `loom.runtime.run`
/// span may exist, and the rejection must be legible on the boundary that
/// refused rather than inferred from the absence of anything.
#[cfg(unix)]
fn rejected_preflight(root: &Path) -> Vec<SpanRecord> {
    let roles = root.join(".loom/roles");
    std::fs::create_dir_all(&roles).unwrap();
    std::fs::write(roles.join("builder.json"), r#"{"runtimeRequirements":["mcp"]}"#).unwrap();
    let sweep = lifecycle::begin(
        root,
        "rejected",
        SpanName::Sweep,
        lifecycle::attributes(&[("loom.sweep_id", "rejected"), ("loom.issue", "8525")]),
    )
    .unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_loom-daemon"));
    command
        .current_dir(root)
        .env("LOOM_WORKSPACE", root)
        .env("LOOM_CONFIG_DEFAULTS_FILE", "")
        .env("LOOM_SHARED_API_KEYS_DIR", "")
        .env_remove("LOOM_ROLE")
        .env_remove("LOOM_MODEL")
        .env_remove("LOOM_MODEL_PROFILE")
        .env("LOOM_OBSERVABILITY_ENABLED", "true")
        .env("LOOM_OBSERVABILITY_EXPORTER", "otlp")
        .env("LOOM_OBSERVABILITY_ENDPOINT", "http://127.0.0.1:4318")
        .env("LOOM_PI_BIN", runtime_fixture())
        .env("FIXTURE_VERSION", "1.18.31")
        .env(
            "LOOM_NATIVE_GUARD_DIR",
            concat!(env!("CARGO_MANIFEST_DIR"), "/../defaults/hooks"),
        )
        .env("LOOM_RUNTIME", "pi")
        .args(["spawn-worker", "--", "-p", "/loom:builder 8525"]);
    sweep.command(&mut command);
    // EX_CONFIG: admission refused before any launch. An admission skip is not
    // a failed model call and must never be recorded as one.
    assert_eq!(command.output().unwrap().status.code(), Some(78));
    lifecycle::finish_execution(root, "rejected", "failure", Default::default());
    exported(root)
}

#[test]
fn cancellation_before_launch_never_fabricates_a_model_run_span() {
    let dir = traced_root();
    let spans = cancelled_before_launch(dir.path());
    assert!(
        !spans.iter().any(|s| s.name == SpanName::RuntimeRun),
        "no runtime was launched, so no model-run span may exist: {spans:?}"
    );
    let root = root_span(&spans);
    assert_eq!(result_of(root), "cancelled");
    assert_eq!(root.status, SpanStatus::Error);
    // The preflight that really did complete keeps its observed result; only
    // the spans still open at cancellation are unknown.
    let preflight = spans
        .iter()
        .find(|s| s.name == SpanName::RuntimePreflight)
        .unwrap();
    assert_eq!(result_of(preflight), "accepted");
    assert_eq!(preflight.status, SpanStatus::Ok);
    for open in spans
        .iter()
        .filter(|s| matches!(s.name, SpanName::Phase | SpanName::RoleAttempt))
    {
        assert_eq!(result_of(open), "exit_unobserved", "{open:?}");
        assert_eq!(open.status, SpanStatus::Unset);
        assert_eq!(open.attributes["loom.timing_source"], "terminal_observed");
    }
}

#[test]
fn cancellation_after_launch_leaves_the_model_run_unobserved_not_successful() {
    let dir = traced_root();
    let spans = cancelled_after_launch(dir.path());
    let run = spans
        .iter()
        .find(|s| s.name == SpanName::RuntimeRun)
        .expect("a launch really happened, so its span must exist");
    assert_eq!(
        result_of(run),
        "exit_unobserved",
        "an unseen exit is unknown, never a success: {run:?}"
    );
    assert_eq!(run.status, SpanStatus::Unset);
    assert!(
        !spans.iter().any(|s| s.status == SpanStatus::Ok),
        "nothing in this arm was observed to succeed: {spans:?}"
    );
    assert_eq!(result_of(root_span(&spans)), "cancelled");
}

#[cfg(unix)]
#[test]
fn a_crash_with_no_terminal_record_is_recovered_as_lost_not_as_success() {
    let dir = traced_root();
    let spans = crashed_without_a_terminal_record(dir.path());
    assert!(!spans.is_empty(), "recovery must export what the crash left behind");
    for span in &spans {
        assert_eq!(result_of(span), "process_lost", "{span:?}");
        assert_eq!(span.attributes["loom.recovered"], "true");
        assert_eq!(span.attributes["loom.timing_source"], "recovery_observed");
        // An unknown ending is Unset. Neither Ok nor Error would be honest: no
        // process survived to observe either one.
        assert_eq!(span.status, SpanStatus::Unset);
    }
}

#[cfg(unix)]
#[test]
fn preflight_rejection_cancellation_and_crash_stay_mutually_distinguishable() {
    // One workspace per arm: distinct roots are what a real fleet has, and it
    // keeps each arm's recovery scan off the others' journals.
    let rejected = traced_root();
    let cancelled = traced_root();
    let crashed = traced_root();
    let arms = [
        ("rejected_preflight", rejected_preflight(rejected.path())),
        ("cancelled", cancelled_before_launch(cancelled.path())),
        ("crashed", crashed_without_a_terminal_record(crashed.path())),
    ];

    // What an operator can read off the trace to tell these endings apart,
    // using only fields the lifecycle itself records — deliberately *not* the
    // model-run count, which depends on how far each arm got rather than on
    // how the ending was classified.
    let signature = |spans: &[SpanRecord]| {
        let root = root_span(spans);
        format!(
            "result={} status={:?} recovered={}",
            result_of(root),
            root.status,
            root.attributes.contains_key("loom.recovered"),
        )
    };
    let signatures: Vec<_> = arms
        .iter()
        .map(|(name, spans)| (*name, signature(spans)))
        .collect();
    let distinct: std::collections::BTreeSet<_> =
        signatures.iter().map(|(_, s)| s.clone()).collect();
    assert_eq!(
        distinct.len(),
        signatures.len(),
        "each abnormal ending must be readable as itself, not collapsed into one shared \
         'it failed' shape: {signatures:?}"
    );

    // Neither the rejection nor the pre-launch cancellation launched a model,
    // so neither may carry a model-run span. This is the "no fictitious
    // model-run span" half of the criterion, asserted across arms rather than
    // once inside a single arm.
    for (name, spans) in &arms {
        if *name == "crashed" {
            continue;
        }
        assert!(
            !spans.iter().any(|s| s.name == SpanName::RuntimeRun),
            "{name} never launched a runtime: {spans:?}"
        );
    }

    // The rejection is legible at the boundary that actually refused: an
    // errored preflight and a rejected attempt, not merely a missing run span.
    let rejection_spans = &arms[0].1;
    assert!(rejection_spans
        .iter()
        .any(|s| s.name == SpanName::RuntimePreflight && s.status == SpanStatus::Error));
    assert!(rejection_spans
        .iter()
        .any(|s| s.name == SpanName::RoleAttempt && result_of(s) == "rejected"));
}
