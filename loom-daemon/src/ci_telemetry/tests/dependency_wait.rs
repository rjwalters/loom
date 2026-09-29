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
use crate::ci_telemetry::records::{
    job_envelopes, JobCreationBaseline, JobCreationBaselines, JobJson, RunJson,
};
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
    attempt_job(id, name, 1, created, started, completed)
}

/// The same, on an explicit `run_attempt` — the `filter=all` listing the poller
/// reads carries every attempt of a re-run, so a row's attempt is data, not a
/// constant.
fn attempt_job(
    id: u64,
    name: &str,
    attempt: u32,
    created: Option<&str>,
    started: &str,
    completed: &str,
) -> JobJson {
    let mut value = serde_json::json!({
        "id": id,
        "name": name,
        "status": "completed",
        "conclusion": "success",
        "run_attempt": attempt,
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
    let baseline = JobCreationBaseline::of_attempt(&jobs, 1);
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
    let baseline = JobCreationBaseline::of_attempt(&jobs, 1);
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
    let baseline = JobCreationBaseline::of_attempt(&jobs, 1);
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
        (gated.completed_at.unwrap()
            - JobCreationBaseline::of_attempt(&jobs, 1).instant().unwrap())
        .num_milliseconds(),
        total
    );
}

#[test]
fn clock_skew_and_missing_timestamps_never_fabricate_a_wait() {
    let jobs = fan_in_jobs();
    let baseline = JobCreationBaseline::of_attempt(&jobs, 1);

    // A job created BEFORE the baseline cannot happen through `of_attempt()`, but a
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
    let legacy_baseline = JobCreationBaseline::of_attempt(&legacy, 1);
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
    let baseline = JobCreationBaseline::of_attempt(&jobs, 1);
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

/// The real shape of a **re-run**, taken from this repo's run `36456576713`:
///
/// ```text
/// $ gh api "repos/rjwalters/loom/actions/runs/36456576713/jobs?filter=all&per_page=100" \
///     --jq '[.jobs[]|{run_attempt,created_at}] | group_by(.run_attempt)
///           | map({attempt:.[0].run_attempt, count:length, earliest:(map(.created_at)|min)})'
/// [{"attempt":1,"count":26,"earliest":"2026-09-28T17:13:13Z"},
///  {"attempt":2,"count":26,"earliest":"2026-09-28T17:52:23Z"}]
/// ```
///
/// Two attempts, 39m10s apart, in **one** listing — because `jobs_path` is
/// `filter=all`, GitHub's "include jobs from old executions of this run" mode.
/// Deliberately NOT a cloned attempt-1 row with the same `created_at` (which is
/// what the re-run tests in [`super::rerun_window`] synthesize, and what let
/// this go unnoticed): GitHub stamps attempt 2's rows at the **re-run** instant,
/// so each attempt has its own creation cluster, and each has its own
/// `needs:`-gated second wave ~59s after its own first.
fn rerun_listing() -> Vec<JobJson> {
    vec![
        attempt_job(
            1,
            "Build loom-daemon (debug, shared)",
            1,
            Some("2026-09-28T17:13:13Z"),
            "2026-09-28T17:13:16Z",
            "2026-09-28T17:14:11Z",
        ),
        attempt_job(
            2,
            "Installer Integration Tests",
            1,
            Some("2026-09-28T17:14:12Z"),
            "2026-09-28T17:14:52Z",
            "2026-09-28T17:17:30Z",
        ),
        attempt_job(
            3,
            "Build loom-daemon (debug, shared)",
            2,
            Some("2026-09-28T17:52:23Z"),
            "2026-09-28T17:52:26Z",
            "2026-09-28T17:53:21Z",
        ),
        attempt_job(
            4,
            "Installer Integration Tests",
            2,
            Some("2026-09-28T17:53:22Z"),
            "2026-09-28T17:54:02Z",
            "2026-09-28T17:56:40Z",
        ),
    ]
}

/// The regression: a baseline taken across the whole `filter=all` listing
/// charges attempt 2 the entire inter-attempt gap as a dependency wait.
#[test]
fn a_reruns_baseline_is_its_own_attempts_first_job_not_the_previous_attempts() {
    let jobs = rerun_listing();
    let (attempt_1, attempt_2) = (
        JobCreationBaseline::of_attempt(&jobs, 1),
        JobCreationBaseline::of_attempt(&jobs, 2),
    );
    assert_eq!(
        attempt_1.instant().map(|t| t.to_rfc3339()),
        Some("2026-09-28T17:13:13+00:00".to_string())
    );
    assert_eq!(
        attempt_2.instant().map(|t| t.to_rfc3339()),
        Some("2026-09-28T17:52:23+00:00".to_string()),
        "attempt 2's zero point is its own first job, 39m10s after attempt 1's"
    );

    let baselines = JobCreationBaselines::of_listing(&jobs);
    let (a2_first, a2_gated) = (&jobs[2], &jobs[3]);
    // Attempt 2's first wave waited on nothing at all — `Some(0)`, the same
    // answer attempt 1's first wave gets, by construction.
    assert_eq!(a2_first.dependency_wait_ms(baselines.for_job(a2_first)), Some(0));
    assert_eq!(jobs[0].dependency_wait_ms(baselines.for_job(&jobs[0])), Some(0));
    // Attempt 2's gated job measures only its OWN attempt's 59s, and each
    // attempt's gated job measures the same wait — the re-run is not slower.
    assert_eq!(a2_gated.dependency_wait_ms(baselines.for_job(a2_gated)), Some(59_000));
    assert_eq!(jobs[1].dependency_wait_ms(baselines.for_job(&jobs[1])), Some(59_000));

    // The pre-fix quantity, asserted as the fiction it was: a `min` over the
    // whole listing is attempt 1's instant, so every attempt-2 job reported
    // 39m10s of "dependency wait" for time before attempt 2 existed — enough
    // to fire section 15's `p90_dep_s > p90_queue_s` alert on every re-run.
    let cross_attempt = JobCreationBaseline::of_attempt(&jobs, 1);
    assert_eq!(
        cross_attempt.instant(),
        jobs.iter().filter_map(|job| job.created_at).min(),
        "an unscoped baseline is exactly the OLDEST attempt's"
    );
    assert_eq!(a2_first.dependency_wait_ms(cross_attempt), Some(2_350_000));
    assert_ne!(
        a2_first.dependency_wait_ms(baselines.for_job(a2_first)),
        a2_first.dependency_wait_ms(cross_attempt)
    );

    // The tiling invariant the three segments rest on holds per attempt: an
    // attempt-2 job's dep + queued + duration is its life since ITS attempt's
    // first job, not an overshoot by the inter-attempt gap.
    let dep = a2_gated
        .dependency_wait_ms(baselines.for_job(a2_gated))
        .unwrap();
    let total = dep + a2_gated.queued_ms().unwrap() + 158_000;
    assert_eq!(
        (a2_gated.completed_at.unwrap() - attempt_2.instant().unwrap()).num_milliseconds(),
        total
    );
}

/// A run attempt GitHub reported no `created_at` for must stay unmeasured —
/// never fall back to a *different* attempt's baseline, which would be a
/// fabricated wait of the whole inter-attempt gap.
#[test]
fn an_attempt_with_no_reported_creation_never_borrows_another_attempts_baseline() {
    let mut jobs = rerun_listing();
    jobs.push(attempt_job(
        5,
        "Pre-9089 recording",
        3,
        None,
        "2026-09-28T18:10:00Z",
        "2026-09-28T18:11:00Z",
    ));
    let baselines = JobCreationBaselines::of_listing(&jobs);
    let legacy = &jobs[4];
    assert_eq!(baselines.for_job(legacy).instant(), None);
    assert_eq!(legacy.dependency_wait_ms(baselines.for_job(legacy)), None);
    // An attempt absent from the listing entirely is the same "not measured".
    assert_eq!(JobCreationBaseline::of_attempt(&jobs, 9).instant(), None);
}

/// End to end through `run_cycle`, on the recorded fixture org: a re-run's jobs
/// must be emitted measuring their own attempt, not the gap since attempt 1.
///
/// The committed fixture omits `created_at` on 23 of its 24 job rows, so the
/// arithmetic under test never ran on the re-run path at all (the circular half
/// of this bug). This test supplies both attempts' creation instants, in the
/// `36456576713` shape above.
#[test]
fn the_poll_cycle_measures_a_rerun_job_against_its_own_attempt() {
    let dir = TempDir::new().unwrap();
    let runs_key = format!("repos/{ORG}/beta/actions/runs?per_page=100");
    let jobs_key = format!("repos/{ORG}/beta/actions/runs/2003/jobs?filter=all&per_page=100");

    // Attempt 1: a first wave created together, one gated job 59s later.
    let attempt_1 = |api: &FixtureApi| {
        api.edit(&jobs_key, |entry| {
            for (i, job) in entry["body"]["jobs"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .enumerate()
            {
                job["created_at"] = Value::from(if i == 3 {
                    "2026-09-20T11:00:59Z"
                } else {
                    "2026-09-20T11:00:00Z"
                });
            }
        });
    };
    // Attempt 2, 39m10s later — the real inter-attempt gap of run 36456576713 —
    // with its own first wave and its own 59s-gated job.
    let attempt_2 = |api: &FixtureApi| {
        api.edit(&runs_key, |entry| {
            entry["body"]["workflow_runs"][0]["run_attempt"] = Value::from(2);
            entry["body"]["workflow_runs"][0]["conclusion"] = Value::from("success");
        });
        api.edit(&jobs_key, |entry| {
            let jobs = entry["body"]["jobs"].as_array_mut().unwrap();
            for (id, created, started, completed) in [
                (20035, "2026-09-20T11:39:10Z", "2026-09-20T11:39:20Z", "2026-09-20T11:39:30Z"),
                (20036, "2026-09-20T11:40:09Z", "2026-09-20T11:40:19Z", "2026-09-20T11:40:29Z"),
            ] {
                jobs.push(serde_json::json!({
                    "id": id,
                    "run_id": 2003,
                    "name": format!("job-{id}"),
                    "status": "completed",
                    "conclusion": "success",
                    "run_attempt": 2,
                    "labels": ["ubuntu-latest"],
                    "created_at": created,
                    "started_at": started,
                    "completed_at": completed,
                }));
            }
        });
    };

    let first = FixtureApi::new();
    attempt_1(&first);
    run_cycle(&ctx(dir.path()), &first).unwrap();

    let api = FixtureApi::new();
    attempt_1(&api);
    attempt_2(&api);
    let report = run_cycle(&ctx(dir.path()), &api).unwrap();
    assert_eq!((report.summary.runs_emitted, report.summary.jobs_emitted), (1, 2));

    let waits: BTreeMap<u64, Option<i64>> = journal(dir.path())
        .into_iter()
        .filter_map(|env| match env.record {
            TelemetryRecord::CiJob(r) if r.run_id == 2003 => Some((r.job_id, r.dependency_wait_ms)),
            _ => None,
        })
        .collect();
    // Attempt 2's first wave: `Some(0)`, NOT the 2,350,000 ms since attempt 1.
    assert_eq!(waits[&20035], Some(0));
    assert_ne!(waits[&20035], Some(2_350_000));
    // Attempt 2's gated job: its own attempt's 59s.
    assert_eq!(waits[&20036], Some(59_000));
    // Attempt 1's own jobs are unchanged by the re-run being in the listing.
    assert_eq!(waits[&20031], Some(0));
    assert_eq!(waits[&20034], Some(59_000));
    assert_no_duplicates(dir.path());
}
