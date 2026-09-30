//! Per-job queue wait and shard attributes (#9089), the job-level extension
//! of #9007's run-level `queued_ms` (tested in [`super::queue_time`]):
//!
//! - `queued_ms = started_at − created_at` per job, so a job's own wait for a
//!   runner is visible, not just the run's.
//! - `loom.ci.shard.{index,total,kind}` for a matrix leg, parsed from the
//!   job's display name — no extra API call, since the poller already fetches
//!   jobs.

use super::*;
use crate::ci_telemetry::records::{parse_shard, JobJson, ShardKind};
use crate::telemetry::trace::SpanName;

fn job(name: &str, created: Option<&str>, started: Option<&str>) -> JobJson {
    let mut value = serde_json::json!({
        "id": 1, "name": name, "status": "completed", "conclusion": "success",
    });
    if let Some(created) = created {
        value["created_at"] = serde_json::json!(created);
    }
    if let Some(started) = started {
        value["started_at"] = serde_json::json!(started);
    }
    serde_json::from_value(value).unwrap()
}

#[test]
fn job_queued_ms_is_created_to_started_and_absent_without_either() {
    let j = job("job-1", Some("2026-09-20T10:00:00Z"), Some("2026-09-20T10:00:05Z"));
    assert_eq!(j.queued_ms(), Some(5_000));
    // Clock skew never produces a negative queue.
    let skewed = job("job-1", Some("2026-09-20T10:00:05Z"), Some("2026-09-20T10:00:00Z"));
    assert_eq!(skewed.queued_ms(), Some(0));
    assert_eq!(job("job-1", None, Some("2026-09-20T10:00:05Z")).queued_ms(), None);
    assert_eq!(job("job-1", Some("2026-09-20T10:00:00Z"), None).queued_ms(), None);
}

#[test]
fn parse_shard_reads_the_ci_yml_display_name_convention() {
    let nextest = parse_shard("Rust Unit Tests (2/3)");
    assert_eq!(nextest.kind, ShardKind::NextestPartition);
    assert_eq!(nextest.index, Some(2));
    assert_eq!(nextest.total, Some(3));

    let feature = parse_shard("Rust OTLP Feature Tests (1/3)");
    assert_eq!(feature.kind, ShardKind::NextestPartition);
    assert_eq!(feature.index, Some(1));
    assert_eq!(feature.total, Some(3));

    // Shell Test Suites' name carries an extra "hermetic" clause before the
    // shard fraction.
    let shell = parse_shard("Shell Test Suites (hermetic, 1/2)");
    assert_eq!(shell.kind, ShardKind::ShellSuiteShard);
    assert_eq!(shell.index, Some(1));
    assert_eq!(shell.total, Some(2));

    for unsharded in ["Lint", "Build (daemon)", "Rust Unit Tests"] {
        let info = parse_shard(unsharded);
        assert_eq!(info.kind, ShardKind::None, "{unsharded}");
        assert_eq!(info.index, None, "{unsharded}");
        assert_eq!(info.total, None, "{unsharded}");
    }
}

#[test]
fn shard_kind_vocabulary_is_stable() {
    assert_eq!(ShardKind::NextestPartition.as_str(), "nextest-partition");
    assert_eq!(ShardKind::ShellSuiteShard.as_str(), "shell-suite-shard");
    assert_eq!(ShardKind::None.as_str(), "none");
}

/// Job 10012 of run 1001 is the one fixture job GitHub reports a
/// `created_at` for, and its name carries a shard suffix
/// (`Shell Test Suites (hermetic, 2/2)`); every other fixture job is
/// unsharded and reports no queue wait.
#[test]
fn the_one_sharded_fixture_job_carries_queue_wait_and_shard_attributes_on_record_and_span() {
    let dir = TempDir::new().unwrap();
    run_cycle(&ctx(dir.path()), &FixtureApi::new()).unwrap();
    let (mut sharded_records, mut sharded_spans, mut plain_records, mut plain_spans) = (0, 0, 0, 0);
    for env in journal(dir.path()) {
        match env.record {
            TelemetryRecord::CiJob(r) if r.job_id == 10012 => {
                assert_eq!(r.queued_ms, Some(5_000));
                assert_eq!(r.shard_index, Some(2));
                assert_eq!(r.shard_total, Some(2));
                assert_eq!(r.shard_kind, "shell-suite-shard");
                sharded_records += 1;
            }
            TelemetryRecord::CiJob(r) => {
                assert_eq!(r.queued_ms, None, "job {}", r.job_id);
                assert_eq!(r.shard_index, None, "job {}", r.job_id);
                assert_eq!(r.shard_total, None, "job {}", r.job_id);
                assert_eq!(r.shard_kind, "none", "job {}", r.job_id);
                plain_records += 1;
            }
            // Job spans only: a `loom.ci.step` span (#9089) repeats its job's
            // shard attributes but carries no queue segment of its own, and
            // is asserted in `super::step_spans`.
            TelemetryRecord::Span(s) if s.name != SpanName::CiJob => {}
            TelemetryRecord::Span(s)
                if s.attributes.get("loom.ci.job_id").map(String::as_str) == Some("10012") =>
            {
                assert_eq!(s.attributes["loom.ci.queued_ms"], "5000");
                assert_eq!(s.attributes["loom.ci.shard.index"], "2");
                assert_eq!(s.attributes["loom.ci.shard.total"], "2");
                assert_eq!(s.attributes["loom.ci.shard.kind"], "shell-suite-shard");
                sharded_spans += 1;
            }
            TelemetryRecord::Span(s) if s.attributes.contains_key("loom.ci.job_id") => {
                assert!(!s.attributes.contains_key("loom.ci.queued_ms"));
                assert!(!s.attributes.contains_key("loom.ci.shard.index"));
                assert!(!s.attributes.contains_key("loom.ci.shard.total"));
                assert_eq!(s.attributes["loom.ci.shard.kind"], "none");
                plain_spans += 1;
            }
            _ => {}
        }
    }
    assert_eq!((sharded_records, sharded_spans), (1, 1));
    // 24 jobs total across the fixture org, minus the one sharded job.
    assert_eq!((plain_records, plain_spans), (23, 23));
}

#[test]
fn a_pre_9089_job_record_still_decodes_as_unsharded_with_no_queue_time() {
    let old = serde_json::json!({
        "repo": "o/r", "run_id": 1, "job_id": 2, "workflow": "CI", "job": "build",
        "attempts": 1, "status": "completed", "timed_out": false,
        "started_at": "2026-09-20T10:00:00Z", "completed_at": "2026-09-20T10:01:00Z",
        "duration_ms": 60000,
    });
    let record: crate::telemetry::CiJobRecord = serde_json::from_value(old).unwrap();
    assert_eq!(record.queued_ms, None);
    assert_eq!(record.shard_index, None);
    assert_eq!(record.shard_total, None);
    assert_eq!(record.shard_kind, "none");
    let attrs = record.log_attributes();
    assert!(!attrs.iter().any(|(k, _)| *k == "loom.ci.queued_ms"));
    assert!(!attrs.iter().any(|(k, _)| *k == "loom.ci.shard.index"));
    assert!(!attrs.iter().any(|(k, _)| *k == "loom.ci.shard.total"));
    assert!(attrs.iter().any(|(k, v)| *k == "loom.ci.shard.kind"
        && *v == crate::telemetry::ci::CiAttr::Str("none".to_string())));
}
