//! `sweep.started` / `sweep.outcome` story-point field tests (Issue #9432,
//! epic #9429).
//!
//! A sibling module rather than more lines in `telemetry/tests.rs`, which is at
//! the `scripts/check-file-size-budget.sh` threshold — the same reason
//! `complexity.rs` and `role_tick.rs` are split out beside this one.
//!
//! The wire contract asserted here: `story_points` is additive (no
//! `schema_version` bump — it is not a new record kind), carried on BOTH sweep
//! records when the issue was sized, **omitted** (never `0`) when it was not,
//! and a record emitted by a pre-#9432 daemon must still decode.

use super::super::*;
use super::{sweep_outcome, ts};

fn sized_outcome(points: u32) -> SweepOutcomeRecord {
    let TelemetryRecord::SweepOutcome(mut record) = sweep_outcome() else {
        panic!("fixture is a sweep.outcome record")
    };
    record.story_points = Some(points);
    record
}

fn started(story_points: Option<u32>) -> SweepStartedRecord {
    SweepStartedRecord {
        repo: "rjwalters/loom".to_string(),
        visibility: RepoVisibility::Public,
        issue: 9432,
        sweep_id: "sweep-issue-9432-0".to_string(),
        started_at: ts(),
        facts: crate::telemetry::SweepStartFacts {
            runtime: Some("claude".to_string()),
            ..Default::default()
        },
        story_points,
    }
}

/// AC1: a sweep of an issue carrying one `points:*` label produces records
/// carrying the size — as a JSON **number**, on both record kinds, for every
/// value in the closed vocabulary.
#[test]
fn both_sweep_records_carry_story_points_when_the_issue_was_sized() {
    for points in [1_u32, 2, 3, 5, 8, 13] {
        let outcome = sized_outcome(points);
        let value = serde_json::to_value(&outcome).unwrap();
        assert_eq!(
            value
                .get("story_points")
                .and_then(serde_json::Value::as_u64),
            Some(u64::from(points)),
            "sweep.outcome must carry the size as a number: {value}"
        );
        assert_eq!(serde_json::from_value::<SweepOutcomeRecord>(value).unwrap(), outcome);

        let start = started(Some(points));
        let value = serde_json::to_value(&start).unwrap();
        assert_eq!(
            value
                .get("story_points")
                .and_then(serde_json::Value::as_u64),
            Some(u64::from(points)),
            "sweep.started must carry the size as a number: {value}"
        );
        assert_eq!(serde_json::from_value::<SweepStartedRecord>(value).unwrap(), start);
    }
}

/// AC2: a sweep of an issue without a points label produces records with **no
/// such attribute** — absent, not `0`, and not `null`.
#[test]
fn an_unsized_issue_omits_story_points_entirely() {
    let TelemetryRecord::SweepOutcome(mut outcome) = sweep_outcome() else {
        panic!("fixture is a sweep.outcome record")
    };
    outcome.story_points = None;
    for value in [
        serde_json::to_value(&outcome).unwrap(),
        serde_json::to_value(started(None)).unwrap(),
    ] {
        assert!(
            value.get("story_points").is_none(),
            "an unsized issue must omit story_points, not report 0/null: {value}"
        );
    }
}

/// Backward compatibility: a record emitted by a daemon that predates #9432
/// decodes with `story_points` reading as "nobody sized this issue", which is
/// distinguishable from a sized record and is NOT a zero.
#[test]
fn records_from_a_pre_9432_daemon_still_decode() {
    let outcome: TelemetryRecord = serde_json::from_str(
        r#"{
            "kind": "sweep.outcome",
            "repo": "rjwalters/loom",
            "visibility": "public",
            "issue": 9432,
            "sweep_id": "sweep-issue-9432-0",
            "total_duration_sec": 512,
            "result": "success",
            "pr_number": 9500
        }"#,
    )
    .unwrap();
    match outcome {
        TelemetryRecord::SweepOutcome(r) => {
            assert_eq!(r.pr_number, Some(9500));
            assert_eq!(r.story_points, None);
        }
        other => panic!("expected SweepOutcome, got {other:?}"),
    }

    let start: TelemetryRecord = serde_json::from_str(
        r#"{
            "kind": "sweep.started",
            "repo": "rjwalters/loom",
            "visibility": "public",
            "issue": 9432,
            "sweep_id": "sweep-issue-9432-0",
            "started_at": "2026-09-29T12:00:00Z",
            "runtime": "claude"
        }"#,
    )
    .unwrap();
    match start {
        TelemetryRecord::SweepStarted(r) => {
            assert_eq!(r.facts.runtime.as_deref(), Some("claude"));
            assert_eq!(r.story_points, None);
        }
        other => panic!("expected SweepStarted, got {other:?}"),
    }
}
