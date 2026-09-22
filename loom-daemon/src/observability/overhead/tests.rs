//! The representative run is also the waterfall/bounds fixture: measuring it
//! and asserting its shape use the same execution, so a passing overhead
//! number can never describe a run whose span graph was never checked.
use super::*;
use crate::telemetry::trace::{SpanId, SpanName, SpanRecord};
use serde_json::json;

/// Every attribute key `bounded_attributes` permits. Kept as a literal so a
/// key silently added to the allowlist still has to be acknowledged here.
const ALLOWED_KEYS: &[&str] = &[
    "loom.repo",
    "loom.repo.visibility",
    "loom.sweep_id",
    "loom.issue",
    "loom.pr_number",
    "loom.role",
    "loom.phase",
    "loom.attempt",
    "loom.runtime",
    "loom.provider",
    "loom.model",
    "loom.configured_model",
    "loom.result",
    "loom.failure_class",
    "loom.effort",
    "loom.doctor_cycles",
    "loom.judge_verdict",
    "loom.recovered",
    "loom.timing_source",
    "loom.tool.name",
];

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

fn untraced_root() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
    std::fs::write(
        dir.path().join(".loom/config.json"),
        json!({"observability":{"enabled":false}}).to_string(),
    )
    .unwrap();
    dir
}

fn child_of<'a>(spans: &'a [SpanRecord], parent: &SpanId, name: SpanName) -> Vec<&'a SpanRecord> {
    spans
        .iter()
        .filter(|s| s.name == name && s.parent_span_id.as_ref() == Some(parent))
        .collect()
}

#[test]
fn representative_run_nests_the_declared_waterfall_and_keeps_every_repair_attempt() {
    let dir = traced_root();
    let shape = RunShape::default();
    let spans = run_once(dir.path(), &shape, "waterfall").unwrap().spans;
    assert_eq!(spans.len(), shape.expected_spans());

    let roots: Vec<_> = spans
        .iter()
        .filter(|s| s.parent_span_id.is_none())
        .collect();
    assert_eq!(roots.len(), 1, "a representative run has exactly one root");
    let sweep = roots[0];
    assert_eq!(sweep.name, SpanName::Sweep);
    assert_eq!(sweep.attributes["loom.issue"], "8525");
    assert_eq!(sweep.attributes["loom.repo"], "rjwalters/loom");
    assert!(
        spans
            .iter()
            .all(|s| s.context.trace_id == sweep.context.trace_id),
        "one sweep is one trace"
    );

    let phases = child_of(&spans, &sweep.context.span_id, SpanName::Phase);
    assert_eq!(phases.len(), shape.phases.len());
    for phase in &phases {
        // Trace-to-issue/PR association lives on attributes, never in the
        // low-cardinality span name.
        assert_eq!(phase.attributes["loom.issue"], "8525");
        assert_eq!(phase.attributes["loom.pr_number"], "8579");
        let attempts = child_of(&spans, &phase.context.span_id, SpanName::RoleAttempt);
        assert_eq!(attempts.len(), 1, "one attempt per phase in this shape");
        let attempt = attempts[0];
        assert_eq!(child_of(&spans, &attempt.context.span_id, SpanName::RuntimePreflight).len(), 1);
        let runs = child_of(&spans, &attempt.context.span_id, SpanName::RuntimeRun);
        assert_eq!(runs.len(), 1);
        assert_eq!(
            child_of(&spans, &runs[0].context.span_id, SpanName::Tool).len(),
            shape.tools_per_attempt
        );
    }

    // The repair sequence: a rejected Judge, a Doctor recovery, then a second
    // accepted Judge. Both Judge attempts must survive as distinct spans.
    let judges: Vec<_> = spans
        .iter()
        .filter(|s| {
            s.name == SpanName::RoleAttempt
                && s.attributes.get("loom.role").is_some_and(|r| r == "judge")
        })
        .collect();
    assert_eq!(judges.len(), 2);
    assert_ne!(judges[0].context.span_id, judges[1].context.span_id);
    let mut verdicts: Vec<_> = judges.iter().map(|s| s.status).collect();
    verdicts.sort_by_key(|s| format!("{s:?}"));
    assert_eq!(verdicts, vec![SpanStatus::Error, SpanStatus::Ok]);
    let mut attempts: Vec<_> = judges
        .iter()
        .map(|s| s.attributes["loom.attempt"].clone())
        .collect();
    attempts.sort();
    assert_eq!(attempts, vec!["1".to_string(), "2".to_string()]);
    assert!(spans.iter().any(|s| s.name == SpanName::RoleAttempt
        && s.attributes.get("loom.role").is_some_and(|r| r == "doctor")
        && s.status == SpanStatus::Ok));
}

#[test]
fn measured_overhead_is_recorded_with_bounded_attributes_and_events() {
    let traced = traced_root();
    let untraced = untraced_root();
    let reference = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(reference.path().join(".loom/logs")).unwrap();
    let report =
        measure(traced.path(), untraced.path(), reference.path(), &RunShape::default(), 3).unwrap();

    assert!(report.offline);
    assert_eq!(report.repetitions, 3);
    assert_eq!(report.spans_per_run, 41);
    // Persisting 41 spans with a durable write per boundary is not free; a
    // zero here would mean the instrumented arm never actually traced.
    assert!(
        report.instrumented_median_ns > report.baseline_median_ns,
        "instrumented {} must exceed baseline {}",
        report.instrumented_median_ns,
        report.baseline_median_ns
    );
    assert!(report.added_median_ns > 0);
    assert!(report.added_max_ns >= report.added_min_ns);
    assert!(report.bytes.journal_bytes > 0);
    assert!(report.bytes.bounded_record_bytes > 0);
    assert_eq!(report.bytes.bytes_per_span, report.bytes.bounded_record_bytes / 41);
    assert!(!report.excludes.is_empty(), "exclusions are stated, not implied");

    // Bounds, realised over the spans the run actually emitted.
    assert!(report.bounds.max_attribute_value_bytes <= 256);
    assert!(report.bounds.max_events_per_span <= 32);
    assert!(report.bounds.max_links_per_span <= 16);
    assert!(report.bounds.max_attributes_per_span > 0);
    for key in &report.bounds.attribute_keys {
        assert!(
            ALLOWED_KEYS.contains(&key.as_str()),
            "emitted attribute key {key} is outside the allowlist"
        );
    }
}

#[test]
fn an_absent_outcome_history_reports_an_unknown_denominator_not_zero() {
    let dir = tempfile::tempdir().unwrap();
    assert!(observed_reference(dir.path()).is_none());
}

#[test]
fn the_denominator_comes_from_observed_durations_and_ignores_zero_length_records() {
    let dir = tempfile::tempdir().unwrap();
    let path = crate::sweep_outcomes::default_outcomes_path(dir.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let lines: String = [0_i64, 10, 20, 30, 40, 50, 60, 70, 80, 90, 100]
        .iter()
        .map(|seconds| {
            format!(
                "{}\n",
                json!({
                    "timestamp": "2026-09-21T00:00:00Z",
                    "repo": "rjwalters/loom",
                    "issue": 8525,
                    "sweep_id": format!("s{seconds}"),
                    "outcome": "exited",
                    "token_name": "unknown",
                    "duration_sec": seconds,
                })
            )
        })
        .collect();
    std::fs::write(&path, lines).unwrap();
    let reference = observed_reference(dir.path()).expect("observed history is present");
    assert_eq!(reference.source, "observed_sweep_outcomes");
    // The zero-length record is excluded: it is an unmeasured run, not a
    // zero-second sweep, and including it would deflate the denominator.
    assert_eq!(reference.sample_size, 10);
    assert_eq!(reference.p50_seconds, 60);
    assert_eq!(reference.p90_seconds, 100);
}

#[test]
fn measure_refuses_arms_that_are_not_actually_different() {
    let traced = traced_root();
    let reference = tempfile::tempdir().unwrap();
    let error = measure(traced.path(), traced.path(), reference.path(), &RunShape::default(), 1)
        .unwrap_err()
        .to_string();
    assert!(error.contains("baseline arm is tracing"), "{error}");

    let untraced = untraced_root();
    let error =
        measure(untraced.path(), untraced.path(), reference.path(), &RunShape::default(), 1)
            .unwrap_err()
            .to_string();
    assert!(error.contains("instrumented arm is not tracing"), "{error}");
}
