//! The CI queue-depth gauge (#11993): `loom.ci.queue_depth{repo}` counts the
//! runs still `queued` in the page `poll_repo` already fetched, so it adds no
//! request and rides the existing cycle. Per-run runner-wait is the separate
//! `queued_ms` ([`super::queue_time`]).

use super::*;
use crate::observability::ops::capture::capture;
use crate::telemetry::ops::{MetricName, MetricValue};

fn run_json(id: u64, status: Option<&str>) -> Value {
    let mut run = serde_json::json!({
        "id": id, "head_sha": "abc", "event": "push",
        "created_at": "2026-09-20T12:00:00Z", "updated_at": "2026-09-20T12:00:00Z",
    });
    if let Some(status) = status {
        run["status"] = serde_json::json!(status);
    }
    run
}

fn depths(captured: &crate::observability::ops::capture::Captured) -> BTreeMap<String, i64> {
    captured
        .metrics
        .iter()
        .filter(|p| p.name == MetricName::CiQueueDepth)
        .map(|p| {
            let MetricValue::Int(v) = p.value else {
                panic!("depth is an int")
            };
            (p.labels["repo"].clone(), v)
        })
        .collect()
}

#[test]
fn depth_counts_only_queued_runs_and_costs_no_request() {
    let alpha = format!("{ORG}/alpha");
    let runs_key = format!("repos/{alpha}/actions/runs?per_page=100");

    let dir = TempDir::new().unwrap();
    let baseline_api = FixtureApi::new();
    let (_, before) = capture(|| run_cycle(&ctx(dir.path()), &baseline_api).unwrap());
    // The recorded fixture holds only completed runs: depth is 0, not absent.
    assert_eq!(depths(&before).get(&alpha), Some(&0));

    let dir = TempDir::new().unwrap();
    let api = FixtureApi::new();
    api.edit(&runs_key, |entry| {
        let runs = entry["body"]["workflow_runs"].as_array_mut().unwrap();
        // N = 3 queued, M = 2 in progress, one with no status at all.
        for (id, status) in [
            (9001, Some("queued")),
            (9002, Some("queued")),
            (9003, Some("queued")),
            (9004, Some("in_progress")),
            (9005, Some("in_progress")),
            (9006, None),
        ] {
            runs.push(run_json(id, status));
        }
    });
    let (_, after) = capture(|| run_cycle(&ctx(dir.path()), &api).unwrap());
    let depths = depths(&after);
    assert_eq!(depths.get(&alpha), Some(&3));
    // Every polled repo reports, not only the one with a queue.
    assert_eq!(depths.get(&format!("{ORG}/beta")), Some(&0));
    // The gauge reads the page already fetched: no extra GitHub request.
    let baseline_requests = baseline_api.requests().len();
    assert_eq!(api.requests().len(), baseline_requests);
}
