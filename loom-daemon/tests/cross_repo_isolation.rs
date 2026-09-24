//! Issue #8525 exists in this repository and, one day, in another one. Both
//! sweeps may run at the same moment on the same host, and either may be
//! adopted by a restarted supervisor afterwards. Nothing about that may merge
//! their traces.
//!
//! This is acceptance evidence for the fourth bullet of #8525. The sequential
//! half — same execution label in two roots, reloaded — is already evidenced by
//! `lifecycle_traces.rs`; what was missing is the concurrent half, where a
//! collision would come from shared process state or from keying durable
//! execution identity on the issue number alone rather than on the repository
//! root plus that number.
//!
//! Both arms drive the real [`loom_daemon::observability::lifecycle`] entry
//! points and drain through `lifecycle::backfill`, so the assertions are made
//! against records that were durably queued for export.
#![cfg(feature = "otlp")]
#![allow(clippy::unwrap_used)]
use loom_daemon::{
    observability::{lifecycle, queue::DurableQueue},
    telemetry::{
        trace::{SpanName, SpanRecord, SpanStatus, TraceContext},
        TelemetryRecord,
    },
};
use serde_json::json;
use std::{
    collections::BTreeSet,
    path::Path,
    sync::{
        atomic::{AtomicU32, Ordering},
        Barrier,
    },
};

/// The same issue number, deliberately: that is the collision under test.
const EXECUTION: &str = "sweep-issue-8525";
const ISSUE: &str = "8525";

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

fn exported(root: &Path) -> Vec<SpanRecord> {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let queue = DurableQueue::open(
        root.join(format!("isolation-queue-{}.jsonl", NEXT.fetch_add(1, Ordering::Relaxed))),
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

/// One repository's sweep for issue 8525, rendezvousing with the other
/// repository's at every boundary so the two genuinely interleave rather than
/// merely overlapping in wall-clock time.
fn drive(root: &Path, sweep_id: &str, barrier: &Barrier) -> TraceContext {
    let sweep = lifecycle::begin(
        root,
        EXECUTION,
        SpanName::Sweep,
        lifecycle::attributes(&[("loom.sweep_id", sweep_id), ("loom.issue", ISSUE)]),
    )
    .expect("tracing is enabled in this workspace");
    let context = sweep.context().clone();
    barrier.wait();
    for role in ["builder", "judge"] {
        let metadata = lifecycle::attributes(&[
            ("loom.role", role),
            ("loom.phase", role),
            ("loom.issue", ISSUE),
            ("loom.sweep_id", sweep_id),
        ]);
        let phase = sweep.child(SpanName::Phase, metadata.clone()).unwrap();
        barrier.wait();
        let attempt = phase.child(SpanName::RoleAttempt, metadata).unwrap();
        barrier.wait();
        attempt.finish("success", SpanStatus::Ok);
        phase.finish("success", SpanStatus::Ok);
        barrier.wait();
    }
    context
}

fn span_ids(spans: &[SpanRecord]) -> BTreeSet<String> {
    spans
        .iter()
        .map(|s| s.context.span_id.as_str().to_owned())
        .collect()
}

#[test]
fn the_same_issue_number_in_two_repositories_stays_separate_under_concurrency_and_restart() {
    let alpha = traced_root();
    let beta = traced_root();
    let barrier = Barrier::new(2);

    let (alpha_context, beta_context) = std::thread::scope(|scope| {
        let one = scope.spawn(|| drive(alpha.path(), "alpha-run", &barrier));
        let two = scope.spawn(|| drive(beta.path(), "beta-run", &barrier));
        (one.join().unwrap(), two.join().unwrap())
    });
    assert_ne!(
        alpha_context.trace_id.as_str(),
        beta_context.trace_id.as_str(),
        "two repositories' issue 8525 are two sweeps, not one"
    );

    // Leak check at the durable layer rather than only at the drained one, made
    // while both executions are still live — that is the moment a shared key
    // would have collided. Each repository's context file and journal must
    // mention its own trace and never the other's, whatever order the
    // interleaved writes landed in.
    for (root, own, other) in [
        (alpha.path(), &alpha_context, &beta_context),
        (beta.path(), &beta_context, &alpha_context),
    ] {
        let directory = root.join(".loom/logs/trace-context");
        let mut inspected = 0;
        for entry in std::fs::read_dir(&directory).unwrap() {
            let path = entry.unwrap().path();
            // The durable records themselves; the sibling lock file carries no
            // identity to leak.
            let Some("json" | "jsonl") = path.extension().and_then(|e| e.to_str()) else {
                continue;
            };
            let text = std::fs::read_to_string(&path).unwrap();
            assert!(
                !text.contains(other.trace_id.as_str()),
                "{} leaked the other repository's trace",
                path.display()
            );
            assert!(text.contains(own.trace_id.as_str()), "{}", path.display());
            inspected += 1;
        }
        assert!(inspected >= 2, "expected a context file and its journal in {directory:?}");
    }

    // Restart: a new supervisor re-reads durable state under the same execution
    // label in each repository. It must adopt each repository's own identity,
    // and must not resolve the label against the other repository's state.
    let restored_alpha =
        lifecycle::begin(alpha.path(), EXECUTION, SpanName::Sweep, Default::default()).unwrap();
    let restored_beta =
        lifecycle::begin(beta.path(), EXECUTION, SpanName::Sweep, Default::default()).unwrap();
    assert_eq!(restored_alpha.context(), &alpha_context);
    assert_eq!(restored_beta.context(), &beta_context);

    // Distinct terminal results prove the two executions are still addressed
    // independently after the restart, not collapsed onto one record.
    lifecycle::finish_execution(alpha.path(), EXECUTION, "success", Default::default());
    lifecycle::finish_execution(beta.path(), EXECUTION, "cancelled", Default::default());

    let alpha_spans = exported(alpha.path());
    let beta_spans = exported(beta.path());
    for (spans, context, result, status) in [
        (&alpha_spans, &alpha_context, "success", SpanStatus::Ok),
        (&beta_spans, &beta_context, "cancelled", SpanStatus::Error),
    ] {
        assert!(!spans.is_empty());
        assert!(
            spans.iter().all(|s| s.context.trace_id == context.trace_id),
            "a repository's spans all belong to its own trace: {spans:?}"
        );
        let root = spans
            .iter()
            .find(|s| s.context == *context)
            .unwrap_or_else(|| panic!("the root must be exported: {spans:?}"));
        assert_eq!(root.attributes["loom.result"], result);
        assert_eq!(root.status, status);
        // 2 phases + 2 attempts + the root, counted as distinct spans: the
        // restart replayed the root start, and a replay must not inflate what
        // an operator sees.
        assert_eq!(span_ids(spans).len(), 5, "{spans:?}");
    }
    assert!(
        span_ids(&alpha_spans).is_disjoint(&span_ids(&beta_spans)),
        "no span id may be shared across repositories"
    );
}
