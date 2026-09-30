//! Step spans (#9089): one `loom.ci.step` span per executed step of a job,
//! parented to that job's span.
//!
//! The motivation is that a job span alone cannot say whether a Rust leg's
//! ~250s went to compiling or to running tests — every CI tuning decision on
//! #9065 needed step timings, and they were pulled by hand from the jobs API.
//! The `steps[]` array is already in the jobs listing the poller fetches, so
//! this costs no extra request; the tests below pin the parts that are easy
//! to regress silently: which steps become spans, the derived identity, and
//! the fact that a pre-#9089 recording (no `steps[]`) still deserializes.

use super::*;
use crate::ci_telemetry::records::{
    job_envelopes, step_context, JobJson, RunJson, MAX_STEP_SPANS_PER_JOB,
};
use crate::telemetry::trace::{SpanName, SpanStatus};

fn repo() -> RepoJson {
    RepoJson {
        name: "alpha".into(),
        full_name: "fixture-org/alpha".into(),
        private: false,
        archived: false,
    }
}

fn run() -> RunJson {
    serde_json::from_value(serde_json::json!({
        "id": 1001, "name": "CI", "head_sha": "abc", "event": "push",
        "status": "completed", "conclusion": "success", "head_branch": "main",
        "created_at": "2026-09-20T09:00:00Z",
        "run_started_at": "2026-09-20T09:00:05Z",
        "updated_at": "2026-09-20T09:01:00Z",
    }))
    .unwrap()
}

fn job_with_steps(steps: serde_json::Value) -> JobJson {
    serde_json::from_value(serde_json::json!({
        "id": 10011, "name": "Rust Unit Tests (2/3)", "status": "completed",
        "conclusion": "success", "run_attempt": 1,
        "created_at": "2026-09-20T09:00:00Z",
        "started_at": "2026-09-20T09:00:05Z",
        "completed_at": "2026-09-20T09:01:00Z",
        "steps": steps,
    }))
    .unwrap()
}

fn step_spans(job: &JobJson) -> Vec<crate::telemetry::trace::SpanRecord> {
    job_envelopes(&repo(), &run(), job, "host-1")
        .into_iter()
        .filter_map(|env| match env.record {
            TelemetryRecord::Span(s) if s.name == SpanName::CiStep => Some(s),
            _ => None,
        })
        .collect()
}

/// A step with both timestamps becomes a span; a step the job never reached
/// (neither timestamp) becomes nothing at all — never a zero-length span at
/// the job's start, which would read as "ran instantly" rather than "did not
/// run".
#[test]
fn only_steps_with_both_timestamps_become_spans() {
    let job = job_with_steps(serde_json::json!([
        {
            "name": "Set up job", "number": 1, "status": "completed",
            "conclusion": "success",
            "started_at": "2026-09-20T09:00:05Z",
            "completed_at": "2026-09-20T09:00:10Z"
        },
        {
            "name": "Run tests", "number": 2, "status": "completed",
            "conclusion": "failure",
            "started_at": "2026-09-20T09:00:10Z",
            "completed_at": "2026-09-20T09:00:55Z"
        },
        {
            "name": "Never reached", "number": 3, "status": "queued",
            "conclusion": null, "started_at": null, "completed_at": null
        },
        {
            "name": "Started but never finished", "number": 4,
            "status": "in_progress", "conclusion": null,
            "started_at": "2026-09-20T09:00:55Z", "completed_at": null
        }
    ]));
    let spans = step_spans(&job);
    assert_eq!(spans.len(), 2);
    assert_eq!(spans[0].attributes["loom.ci.step"], "Set up job");
    assert_eq!(spans[0].attributes["loom.ci.step_number"], "1");
    assert_eq!(spans[0].status, SpanStatus::Ok);
    assert_eq!(spans[1].attributes["loom.ci.step"], "Run tests");
    assert_eq!(spans[1].attributes["loom.ci.step_number"], "2");
    // A step's conclusion is its own, not its job's — this job succeeded.
    assert_eq!(spans[1].status, SpanStatus::Error);
    assert_eq!(spans[1].attributes["loom.ci.conclusion"], "failure");
}

/// Every step span repeats its job's identity and shard attributes, so
/// "which step of which leg is slow" is one group-by rather than a trace
/// join, and parents to the job span in the run's trace.
#[test]
fn a_step_span_carries_its_jobs_identity_and_shard_attributes() {
    let job = job_with_steps(serde_json::json!([{
        "name": "Run tests", "number": 2, "status": "completed",
        "conclusion": "success",
        "started_at": "2026-09-20T09:00:10Z",
        "completed_at": "2026-09-20T09:00:55Z"
    }]));
    let envelopes = job_envelopes(&repo(), &run(), &job, "host-1");
    let job_span = envelopes
        .iter()
        .find_map(|env| match &env.record {
            TelemetryRecord::Span(s) if s.name == SpanName::CiJob => Some(s),
            _ => None,
        })
        .expect("a job span");
    let spans = step_spans(&job);
    let step = &spans[0];
    assert_eq!(step.parent_span_id.as_ref(), Some(&job_span.context.span_id));
    assert_eq!(step.context.trace_id, job_span.context.trace_id);
    assert_eq!(step.attributes["loom.repo"], "fixture-org/alpha");
    assert_eq!(step.attributes["loom.repo.visibility"], "public");
    assert_eq!(step.attributes["loom.ci.run_id"], "1001");
    assert_eq!(step.attributes["loom.ci.job_id"], "10011");
    assert_eq!(step.attributes["loom.ci.job"], "Rust Unit Tests (2/3)");
    assert_eq!(step.attributes["loom.ci.workflow"], "CI");
    assert_eq!(step.attributes["loom.ci.shard.index"], "2");
    assert_eq!(step.attributes["loom.ci.shard.total"], "3");
    assert_eq!(step.attributes["loom.ci.shard.kind"], "nextest-partition");
    // A step has no queue of its own and is not a job: it must not claim
    // either vocabulary.
    assert!(!step.attributes.contains_key("loom.ci.queued_ms"));
    assert!(!step.attributes.contains_key("loom.ci.attempts"));
    assert!(step.validate().is_ok());
}

/// Span ids are derived, never random — a replayed or second-host emission of
/// the same step is byte-identical in identity, which is what lets the
/// journal deduplicate on `span|<span_id>`. The *number* is the identity, so
/// two steps sharing a name stay distinct and renaming a step does not fork
/// its id.
#[test]
fn step_span_ids_are_derived_from_the_step_number_not_its_name() {
    let twice_named_the_same = job_with_steps(serde_json::json!([
        {
            "name": "Run make", "number": 1, "status": "completed",
            "conclusion": "success",
            "started_at": "2026-09-20T09:00:05Z",
            "completed_at": "2026-09-20T09:00:10Z"
        },
        {
            "name": "Run make", "number": 2, "status": "completed",
            "conclusion": "success",
            "started_at": "2026-09-20T09:00:10Z",
            "completed_at": "2026-09-20T09:00:20Z"
        }
    ]));
    let spans = step_spans(&twice_named_the_same);
    assert_ne!(spans[0].context.span_id, spans[1].context.span_id);
    for (i, span) in spans.iter().enumerate() {
        let expected =
            step_context("fixture-org/alpha", 1001, 1, 10011, u32::try_from(i).unwrap() + 1);
        assert_eq!(span.context.span_id, expected.span_id);
        assert_eq!(span.context.trace_id, expected.trace_id);
    }
    // Re-deriving the same step twice yields the same id.
    assert_eq!(
        step_context("fixture-org/alpha", 1001, 1, 10011, 1).span_id,
        step_context("fixture-org/alpha", 1001, 1, 10011, 1).span_id
    );
    // A different job's step 1 is a different span.
    assert_ne!(
        step_context("fixture-org/alpha", 1001, 1, 10011, 1).span_id,
        step_context("fixture-org/alpha", 1001, 1, 10012, 1).span_id
    );
}

/// One hostile or pathological job can never expand into unbounded span
/// volume, and a long or control-character-bearing step name still produces a
/// usable `loom.ci.step` — `bounded_attributes` DROPS a value over 256 chars
/// or containing a control character, so an unsanitized name would silently
/// lose the attribute that identifies the span.
#[test]
fn step_spans_are_capped_per_job_and_their_names_survive_bounding() {
    let mut steps: Vec<serde_json::Value> = (1..=MAX_STEP_SPANS_PER_JOB + 10)
        .map(|n| {
            serde_json::json!({
                "name": format!("step {n}"), "number": n, "status": "completed",
                "conclusion": "success",
                "started_at": "2026-09-20T09:00:05Z",
                "completed_at": "2026-09-20T09:00:06Z"
            })
        })
        .collect();
    steps[0] = serde_json::json!({
        "name": format!("Run\tthe\nvery {} step", "long".repeat(200)),
        "number": 1, "status": "completed", "conclusion": "success",
        "started_at": "2026-09-20T09:00:05Z",
        "completed_at": "2026-09-20T09:00:06Z"
    });
    let spans = step_spans(&job_with_steps(serde_json::json!(steps)));
    assert_eq!(spans.len(), MAX_STEP_SPANS_PER_JOB);
    let bounded = spans[0].clone().bounded();
    let name = &bounded.attributes["loom.ci.step"];
    assert!(name.starts_with("Run the very long"), "{name}");
    assert!(name.ends_with('…'), "{name}");
    assert!(name.chars().count() <= 256 && !name.chars().any(char::is_control));
}

/// A recording made before #9089 has no `steps[]` at all, and a job that
/// failed before any step ran reports an empty one. Neither is an error, and
/// neither emits a step span — the job unit is exactly what it was.
#[test]
fn a_job_without_steps_emits_exactly_the_pre_9089_envelopes() {
    let pre_9089: JobJson = serde_json::from_value(serde_json::json!({
        "id": 10011, "name": "Lint", "status": "completed",
        "conclusion": "success", "run_attempt": 1,
        "started_at": "2026-09-20T09:00:05Z",
        "completed_at": "2026-09-20T09:01:00Z"
    }))
    .unwrap();
    assert!(pre_9089.steps.is_empty());
    assert_eq!(job_envelopes(&repo(), &run(), &pre_9089, "host-1").len(), 3);
    assert!(step_spans(&pre_9089).is_empty());
    assert!(step_spans(&job_with_steps(serde_json::json!([]))).is_empty());
}

/// Both `loom.ci.step` attribute keys are inside the declared span
/// vocabulary, so the gateway's `keep_keys` forwards them and
/// `bounded_attributes` admits them. Without this a step span would arrive in
/// SigNoz stripped of the only two attributes that say which step it is.
#[test]
fn step_attributes_are_inside_the_declared_span_vocabulary() {
    let job = job_with_steps(serde_json::json!([{
        "name": "Run tests", "number": 2, "status": "completed",
        "conclusion": "success",
        "started_at": "2026-09-20T09:00:10Z",
        "completed_at": "2026-09-20T09:00:55Z"
    }]));
    let span = step_spans(&job).remove(0);
    let before: Vec<String> = span.attributes.keys().cloned().collect();
    let after = span.bounded();
    assert_eq!(before, after.attributes.keys().cloned().collect::<Vec<_>>());
    for key in ["loom.ci.step", "loom.ci.step_number"] {
        assert!(CI_SPAN_ATTRIBUTE_KEYS.contains(&key), "{key} is undeclared");
        assert!(after.attributes.contains_key(key));
    }
}

/// One trace per run attempt, three levels deep since #9089: the run span is
/// the root, each job span parents to it, and each step span parents to its
/// own job's span (never to the run root). Asserted over a whole cycle of the
/// recorded fixture org, so it covers the poller's journal, not just
/// `job_envelopes` in isolation. (Before #9089 this test lived in
/// `super::job_spans_parent_to_their_run_span_in_one_trace`; it moved here
/// with the third level.)
#[test]
fn job_spans_parent_to_their_run_span_and_step_spans_to_their_job_span() {
    let dir = TempDir::new().unwrap();
    run_cycle(&ctx(dir.path()), &FixtureApi::new()).unwrap();
    let spans: Vec<_> = journal(dir.path())
        .into_iter()
        .filter_map(|e| match e.record {
            TelemetryRecord::Span(s) => Some(s),
            _ => None,
        })
        .collect();
    let roots: HashMap<String, _> = spans
        .iter()
        .filter(|s| s.parent_span_id.is_none())
        .map(|s| (s.attributes["loom.ci.run_id"].clone(), s))
        .collect();
    assert_eq!(roots.len(), 6);
    let jobs: HashMap<String, _> = spans
        .iter()
        .filter(|s| s.name == SpanName::CiJob)
        .map(|s| (s.attributes["loom.ci.job_id"].clone(), s))
        .collect();
    assert_eq!(jobs.len(), 24);
    for job in jobs.values() {
        let root = roots[&job.attributes["loom.ci.run_id"]];
        assert_eq!(job.parent_span_id.as_ref(), Some(&root.context.span_id));
        assert_eq!(job.context.trace_id, root.context.trace_id);
    }
    let mut steps = 0;
    for step in spans.iter().filter(|s| s.name == SpanName::CiStep) {
        let job = jobs[&step.attributes["loom.ci.job_id"]];
        assert_eq!(step.parent_span_id.as_ref(), Some(&job.context.span_id));
        assert_eq!(step.context.trace_id, job.context.trace_id);
        // A step never escapes its job's window.
        assert!(step.started_at >= job.started_at && step.ended_at <= job.ended_at);
        steps += 1;
    }
    assert_eq!(steps, FIXTURE_STEP_SPANS);
}
