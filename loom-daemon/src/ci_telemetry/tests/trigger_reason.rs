//! CI trigger attribution (#9337): every `loom.ci.run` span and `ci.run`
//! record carries `loom.ci.trigger_reason`, a pure function of one
//! `/actions/runs` row — `flaky_retry` (attempt > 1), `stale_main_bump`
//! (the #8508 re-date commit), `new_commit` (push / PR / merge-queue head),
//! else `unknown`. The run span additionally carries `loom.ci.run_attempt`.

use super::*;
use crate::ci_telemetry::records::{RunJson, TriggerReason};
use crate::merge_pr::redate::commit_message;

fn row(event: &str, attempt: u32, message: Option<&str>) -> RunJson {
    let mut value = serde_json::json!({
        "id": 7, "head_sha": "abc", "event": event, "status": "completed",
        "run_attempt": attempt, "head_branch": "feature/issue-9337",
        "created_at": "2026-09-20T10:00:00Z", "updated_at": "2026-09-20T10:05:00Z",
    });
    if let Some(message) = message {
        value["head_commit"] =
            serde_json::json!({"id": "abc", "tree_id": "def", "message": message});
    }
    serde_json::from_value(value).unwrap()
}

#[test]
fn trigger_reason_follows_the_documented_rule_order() {
    use TriggerReason::{FlakyRetry as Retry, NewCommit as New, StaleMainBump as Bump, Unknown};
    let redate = commit_message("123");
    let ordinary = "fix(daemon): a real change (#9337)\n\nBody.";
    let rd = Some(redate.as_str());
    let ok = Some(ordinary);
    let cases: &[(&str, &str, u32, Option<&str>, TriggerReason)] = &[
        ("re-run in place", "pull_request", 2, ok, Retry),
        // Rule order: a re-run OF a re-date head is still a re-run.
        ("re-run of a re-date head", "pull_request", 2, rd, Retry),
        ("re-run without head_commit", "push", 3, None, Retry),
        ("re-date remedy commit", "pull_request", 1, rd, Bump),
        ("re-date pushed to branch", "push", 1, rd, Bump),
        ("ordinary PR head", "pull_request", 1, ok, New),
        ("branch push before a PR", "push", 1, ok, New),
        ("pull_request_target", "pull_request_target", 1, ok, New),
        ("merge queue", "merge_group", 1, ok, New),
        // Documented under-count: a merge-from-main head is a freshness bump in
        // spirit but can be real conflict work, so v1 calls it new_commit.
        ("merge from main", "push", 1, Some("Merge branch 'main' into feature/x"), New),
        // Near-miss of the re-date subject is not the remedy.
        (
            "near-miss subject",
            "pull_request",
            1,
            Some("chore: re-date required checks for PR #123"),
            New,
        ),
        ("manual dispatch", "workflow_dispatch", 1, ok, Unknown),
        ("schedule", "schedule", 1, ok, Unknown),
        ("no head_commit", "push", 1, None, Unknown),
        ("empty message", "push", 1, Some(""), Unknown),
    ];
    for (label, event, attempt, message, want) in cases {
        assert_eq!(row(event, *attempt, *message).trigger_reason(), *want, "{label}");
    }
}

#[test]
fn a_null_head_commit_decodes_and_attributes_unknown() {
    let run: RunJson = serde_json::from_value(serde_json::json!({
        "id": 7, "event": "push", "head_commit": null,
        "created_at": "2026-09-20T10:00:00Z", "updated_at": "2026-09-20T10:05:00Z",
    }))
    .unwrap();
    assert!(run.head_commit.is_none());
    assert_eq!(run.trigger_reason(), TriggerReason::Unknown);
}

#[test]
fn the_four_values_are_the_documented_vocabulary() {
    let values: Vec<_> = [
        TriggerReason::NewCommit,
        TriggerReason::StaleMainBump,
        TriggerReason::FlakyRetry,
        TriggerReason::Unknown,
    ]
    .iter()
    .map(|r| r.as_str())
    .collect();
    assert_eq!(values, ["new_commit", "stale_main_bump", "flaky_retry", "unknown"]);
}

/// The fixture's beta runs carry a head commit (2003 ordinary, 2002 the
/// re-date subject, 2001 a merge from main); alpha's carry none.
#[test]
fn every_fixture_run_carries_its_trigger_reason_on_record_and_run_span() {
    let dir = TempDir::new().unwrap();
    run_cycle(&ctx(dir.path()), &FixtureApi::new()).unwrap();
    let want = |run_id: u64| match run_id {
        2003 | 2001 => "new_commit",
        2002 => "stale_main_bump",
        _ => "unknown",
    };
    let (mut records, mut run_spans) = (0, 0);
    for env in journal(dir.path()) {
        match env.record {
            TelemetryRecord::CiRun(r) => {
                assert_eq!(r.trigger_reason.as_deref(), Some(want(r.run_id)), "run {}", r.run_id);
                assert!(r
                    .log_attributes()
                    .iter()
                    .any(|(k, v)| *k == "loom.ci.trigger_reason"
                        && *v == crate::telemetry::ci::CiAttr::Str(want(r.run_id).to_string())));
                records += 1;
            }
            TelemetryRecord::Span(s) if s.parent_span_id.is_none() => {
                let run_id: u64 = s.attributes["loom.ci.run_id"].parse().unwrap();
                assert_eq!(s.attributes["loom.ci.trigger_reason"], want(run_id), "run {run_id}");
                assert_eq!(s.attributes["loom.ci.run_attempt"], "1");
                run_spans += 1;
            }
            TelemetryRecord::Span(s) => {
                assert!(
                    !s.attributes.contains_key("loom.ci.trigger_reason"),
                    "job spans carry none"
                );
                assert!(
                    !s.attributes.contains_key("loom.ci.run_attempt"),
                    "job spans use attempts"
                );
            }
            _ => {}
        }
    }
    assert_eq!((records, run_spans), (6, 6));
}

#[test]
fn a_pre_9337_run_record_still_decodes_without_a_trigger_reason() {
    let old = serde_json::json!({
        "repo": "o/r", "run_id": 1, "run_attempt": 1, "workflow": "CI", "head_sha": "a",
        "event": "push", "status": "completed", "started_at": "2026-09-20T10:00:00Z",
        "completed_at": "2026-09-20T10:01:00Z", "duration_ms": 60000,
    });
    let record: crate::telemetry::CiRunRecord = serde_json::from_value(old).unwrap();
    assert_eq!(record.trigger_reason, None);
    assert!(!record
        .log_attributes()
        .iter()
        .any(|(k, _)| *k == "loom.ci.trigger_reason"));
}
