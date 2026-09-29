//! Per-job dependency wait (#9089, issue problem 5) — the segment that
//! precedes the runner-queue wait [`super::shard_queue`] covers.
//!
//! A job's life has three segments and the telemetry now reports all three
//! separately:
//!
//! ```text
//! run's first job created ── dependency_wait_ms ─▶ this job's created_at
//!                                       ── queued_ms ─▶ started_at
//!                                            ── duration_ms ─▶ completed_at
//! ```
//!
//! The baseline is the run attempt's EARLIEST job creation, not the run row's
//! `run_started_at`: GitHub creates a `needs:`-gated job only once its
//! predecessors finish, so the gap between the first wave's creation and a
//! gated job's creation is the dependency wait — with no second API call and
//! no knowledge of the workflow's `needs:` graph. Observed on a real `main` CI
//! run (`36508530424`, 2026-09-29): ungated jobs at `01:34:16`, every
//! `needs: build-daemon` job at `01:35:15`, one second after `Build
//! loom-daemon` completed.

use super::*;
use crate::ci_telemetry::records::{job_envelopes, JobCreationBaseline, JobJson, RunJson};
use crate::telemetry::trace::SpanName;

fn repo() -> RepoJson {
    RepoJson {
        name: "alpha".into(),
        full_name: "fixture-org/alpha".into(),
        private: false,
        archived: false,
    }
}

/// A run whose own queue segment (`created_at` → `run_started_at`) is 10s, so
/// a test that accidentally measured the dependency wait from the run row
/// instead of the first job would produce a visibly different number.
fn run() -> RunJson {
    serde_json::from_value(serde_json::json!({
        "id": 1001,
        "name": "CI alpha",
        "head_branch": "main",
        "head_sha": "0".repeat(40),
        "event": "push",
        "status": "completed",
        "conclusion": "success",
        "run_attempt": 1,
        "created_at": "2026-09-29T01:34:00Z",
        "run_started_at": "2026-09-29T01:34:10Z",
        "updated_at": "2026-09-29T01:39:42Z",
    }))
    .unwrap()
}

/// One job row, shaped like the real `/actions/runs/{id}/jobs` rows the
/// derivation was verified against.
fn job(id: u64, name: &str, created: Option<&str>, started: &str, completed: &str) -> JobJson {
    let mut value = serde_json::json!({
        "id": id,
        "name": name,
        "status": "completed",
        "conclusion": "success",
        "run_attempt": 1,
        "started_at": started,
        "completed_at": completed,
    });
    if let Some(created) = created {
        value["created_at"] = serde_json::json!(created);
    }
    serde_json::from_value(value).unwrap()
}

/// The shape of run 36508530424: a first wave created at `01:34:16`, and a
/// second wave created at `01:35:15` because it declares
/// `needs: build-daemon`, which completed at `01:35:14`.
fn fan_in_jobs() -> Vec<JobJson> {
    vec![
        job(
            1,
            "Build loom-daemon (debug, shared)",
            Some("2026-09-29T01:34:16Z"),
            "2026-09-29T01:34:19Z",
            "2026-09-29T01:35:14Z",
        ),
        job(
            2,
            "Rust Unit Tests (1/3)",
            Some("2026-09-29T01:34:16Z"),
            "2026-09-29T01:34:19Z",
            "2026-09-29T01:38:53Z",
        ),
        job(
            3,
            "Installer Integration Tests",
            Some("2026-09-29T01:35:15Z"),
            "2026-09-29T01:35:55Z",
            "2026-09-29T01:38:33Z",
        ),
        job(
            4,
            "Daemon Checks",
            Some("2026-09-29T01:35:15Z"),
            "2026-09-29T01:36:12Z",
            "2026-09-29T01:36:35Z",
        ),
    ]
}

#[test]
fn the_baseline_is_the_runs_earliest_job_creation() {
    let jobs = fan_in_jobs();
    let baseline = JobCreationBaseline::of(&jobs);
    assert_eq!(
        baseline.instant().map(|t| t.to_rfc3339()),
        Some("2026-09-29T01:34:16+00:00".to_string())
    );
    // Deliberately NOT the run row's 01:34:00 / 01:34:10 — the run's own queue
    // segment is a different measurement (#9007) and subtracting it per job
    // would double-count it into every leg.
    assert_ne!(baseline.instant(), Some(run().created_at));
    assert_ne!(baseline.instant(), run().run_started_at);
}

#[test]
fn a_needs_gated_job_reports_the_predecessor_time_and_an_ungated_one_reports_zero() {
    let jobs = fan_in_jobs();
    let baseline = JobCreationBaseline::of(&jobs);
    // First wave: created with the run's first job, so nothing was waited on.
    // `Some(0)` and `None` are different answers — this is "waited on nothing".
    assert_eq!(jobs[0].dependency_wait_ms(baseline), Some(0));
    assert_eq!(jobs[1].dependency_wait_ms(baseline), Some(0));
    // Second wave: 01:34:16 → 01:35:15 is 59s blocked on `build-daemon`.
    assert_eq!(jobs[2].dependency_wait_ms(baseline), Some(59_000));
    assert_eq!(jobs[3].dependency_wait_ms(baseline), Some(59_000));
}

#[test]
fn the_three_segments_are_disjoint_and_never_double_count() {
    let jobs = fan_in_jobs();
    let baseline = JobCreationBaseline::of(&jobs);
    // `Installer Integration Tests`: 59s gated, then 40s queued for a runner
    // (01:35:15 → 01:35:55), then 158s running (01:35:55 → 01:38:33).
    let gated = &jobs[2];
    assert_eq!(gated.dependency_wait_ms(baseline), Some(59_000));
    assert_eq!(gated.queued_ms(), Some(40_000));
    let record = match &job_envelopes(&repo(), &run(), gated, baseline, "host-1")[0].record {
        TelemetryRecord::CiJob(r) => r.clone(),
        other => panic!("expected a ci.job record, got {other:?}"),
    };
    assert_eq!(record.duration_ms, 158_000);
    // The three segments tile [first job created, completed_at] exactly: no
    // overlap, no gap. A queue wait measured from the run's start instead of
    // the job's creation would break this.
    let total = record.dependency_wait_ms.unwrap() + record.queued_ms.unwrap() + record.duration_ms;
    assert_eq!(total, 257_000);
    assert_eq!(
        (gated.completed_at.unwrap() - JobCreationBaseline::of(&jobs).instant().unwrap())
            .num_milliseconds(),
        total
    );
}

#[test]
fn clock_skew_and_missing_timestamps_never_fabricate_a_wait() {
    let jobs = fan_in_jobs();
    let baseline = JobCreationBaseline::of(&jobs);

    // A job created BEFORE the baseline cannot happen through `of()`, but a
    // forged or skewed row must floor at zero rather than go negative.
    let skewed = job(
        9,
        "Skewed",
        Some("2026-09-29T01:30:00Z"),
        "2026-09-29T01:34:20Z",
        "2026-09-29T01:34:30Z",
    );
    assert_eq!(skewed.dependency_wait_ms(baseline), Some(0));

    // No `created_at` on the job: unknown, never "waited on nothing".
    let no_created =
        job(10, "Pre-9089 recording", None, "2026-09-29T01:34:20Z", "2026-09-29T01:34:30Z");
    assert_eq!(no_created.dependency_wait_ms(baseline), None);

    // No `created_at` anywhere in the run: no baseline, so no job has a wait.
    let legacy = vec![no_created.clone()];
    let legacy_baseline = JobCreationBaseline::of(&legacy);
    assert_eq!(legacy_baseline.instant(), None);
    assert_eq!(jobs[2].dependency_wait_ms(legacy_baseline), None);
    assert_eq!(no_created.dependency_wait_ms(legacy_baseline), None);
    // `JobCreationBaseline::default()` is the same "not measured" state, which
    // is what an isolated-job caller passes.
    assert_eq!(JobCreationBaseline::default().instant(), None);
    assert_eq!(jobs[2].dependency_wait_ms(JobCreationBaseline::default()), None);
}

#[test]
fn the_record_and_the_job_span_both_carry_it_and_the_step_spans_do_not() {
    let jobs = fan_in_jobs();
    let baseline = JobCreationBaseline::of(&jobs);
    let mut gated = jobs[2].clone();
    gated.steps = serde_json::from_value(serde_json::json!([{
        "name": "Run installer tests", "number": 1, "status": "completed",
        "conclusion": "success",
        "started_at": "2026-09-29T01:35:55Z",
        "completed_at": "2026-09-29T01:38:33Z"
    }]))
    .unwrap();

    let (mut records, mut job_spans, mut step_spans) = (0, 0, 0);
    for env in job_envelopes(&repo(), &run(), &gated, baseline, "host-1") {
        match env.record {
            TelemetryRecord::CiJob(r) => {
                assert_eq!(r.dependency_wait_ms, Some(59_000));
                assert!(r
                    .log_attributes()
                    .iter()
                    .any(|(k, v)| *k == "loom.ci.dependency_wait_ms"
                        && *v == crate::telemetry::ci::CiAttr::Int(59_000)));
                records += 1;
            }
            TelemetryRecord::Span(s) if s.name == SpanName::CiJob => {
                assert_eq!(s.attributes["loom.ci.dependency_wait_ms"], "59000");
                // Still distinct from the runner-queue segment beside it.
                assert_eq!(s.attributes["loom.ci.queued_ms"], "40000");
                job_spans += 1;
            }
            TelemetryRecord::Span(s) if s.name == SpanName::CiStep => {
                // A step span repeats its job's shard identity but NOT its
                // dependency wait: it is a property of the job, and repeating
                // it would multiply one wait across every child in any sum.
                assert!(!s.attributes.contains_key("loom.ci.dependency_wait_ms"));
                step_spans += 1;
            }
            _ => {}
        }
    }
    assert_eq!((records, job_spans, step_spans), (1, 1, 1));
}

#[test]
fn an_unmeasured_wait_is_absent_from_the_record_and_the_span_entirely() {
    let job = job(11, "Pre-9089 recording", None, "2026-09-29T01:34:20Z", "2026-09-29T01:34:30Z");
    for env in job_envelopes(&repo(), &run(), &job, JobCreationBaseline::default(), "host-1") {
        match env.record {
            TelemetryRecord::CiJob(r) => {
                assert_eq!(r.dependency_wait_ms, None);
                assert!(!r
                    .log_attributes()
                    .iter()
                    .any(|(k, _)| *k == "loom.ci.dependency_wait_ms"));
            }
            TelemetryRecord::Span(s) => {
                assert!(!s.attributes.contains_key("loom.ci.dependency_wait_ms"));
            }
            _ => {}
        }
    }
}

/// A `ci.job` line written before this change must still decode, with the new
/// field absent rather than defaulting to a zero wait.
#[test]
fn a_pre_dependency_wait_job_record_decodes_with_no_wait() {
    let old = serde_json::json!({
        "repo": "o/r", "run_id": 1, "job_id": 2, "workflow": "CI", "job": "build",
        "attempts": 1, "status": "completed", "timed_out": false,
        "started_at": "2026-09-20T10:00:00Z", "completed_at": "2026-09-20T10:01:00Z",
        "duration_ms": 60000, "queued_ms": 5000,
        "shard_index": 1, "shard_total": 3, "shard_kind": "nextest-partition",
    });
    let record: crate::telemetry::CiJobRecord = serde_json::from_value(old).unwrap();
    assert_eq!(record.queued_ms, Some(5_000));
    assert_eq!(record.dependency_wait_ms, None);
    assert!(!record
        .log_attributes()
        .iter()
        .any(|(k, _)| *k == "loom.ci.dependency_wait_ms"));
}

/// The attribute key is inside the declared vocabularies, so the gateway's
/// `keep_keys` forwards it and `bounded_attributes` admits it. Without this a
/// query filtering on it returns zero rows — indistinguishable from "no job
/// ever waited".
#[test]
fn the_dependency_wait_key_is_declared_on_both_the_log_and_span_vocabularies() {
    assert!(CI_LOG_ATTRIBUTE_KEYS.contains(&"loom.ci.dependency_wait_ms"));
    assert!(CI_SPAN_ATTRIBUTE_KEYS.contains(&"loom.ci.dependency_wait_ms"));
}

/// The whole recorded-fixture cycle: job 10012 is the only fixture job GitHub
/// reports a `created_at` for, so it is its run's own baseline and measures a
/// zero wait; every other fixture job reports none at all.
#[test]
fn the_fixture_cycle_measures_a_wait_only_where_github_reported_a_creation() {
    let dir = TempDir::new().unwrap();
    run_cycle(&ctx(dir.path()), &FixtureApi::new()).unwrap();
    let (mut measured, mut unmeasured) = (0, 0);
    for env in journal(dir.path()) {
        if let TelemetryRecord::CiJob(r) = env.record {
            if r.job_id == 10012 {
                assert_eq!(r.dependency_wait_ms, Some(0));
                measured += 1;
            } else {
                assert_eq!(r.dependency_wait_ms, None, "job {}", r.job_id);
                unmeasured += 1;
            }
        }
    }
    assert_eq!((measured, unmeasured), (1, 23));
}
