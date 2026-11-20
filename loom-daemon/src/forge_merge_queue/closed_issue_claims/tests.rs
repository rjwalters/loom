use std::cell::Cell;

use serde_json::json;

use super::*;

const NWO: &str = "example-org/work";
const PR: u32 = 52;
const MERGED: &str = "2026-11-20T01:00:00Z";
const CLOSED: &str = "2026-11-20T01:00:02Z";

fn report(nodes: Vec<Value>) -> Value {
    json!({"data": {"repository": {
        "nameWithOwner": NWO,
        "pullRequest": {
            "number": PR, "mergedAt": MERGED, "mergeCommit": {"oid": "merged"},
            "commits": {"nodes": [{"commit": {"oid": "pr-commit"}}]},
            "closingIssuesReferences": {"pageInfo": {"hasNextPage": false}, "nodes": nodes},
        }
    }}})
}

fn pr_closer(number: u32, repo: &str) -> Value {
    json!({"__typename": "PullRequest", "number": number, "repository": {"nameWithOwner": repo}})
}

fn issue(number: u32, closer: Value) -> Value {
    json!({
        "number": number, "repository": {"nameWithOwner": NWO}, "state": "CLOSED",
        "closedAt": CLOSED, "timelineItems": {"nodes": [{"closer": closer}]},
    })
}

fn open_issue(number: u32) -> Value {
    json!({"number": number, "repository": {"nameWithOwner": NWO}, "state": "OPEN"})
}

#[test]
fn selects_only_attributable_local_closes() {
    let mut earlier = issue(6, pr_closer(PR, NWO));
    earlier["closedAt"] = json!("2026-11-20T00:59:59Z");
    let mut unrelated_repo = issue(7, pr_closer(PR, NWO));
    unrelated_repo["repository"]["nameWithOwner"] = json!("example-org/other");
    let mut empty_timeline = issue(8, Value::Null);
    empty_timeline["timelineItems"]["nodes"] = json!([]);
    let doc = report(vec![
        issue(1, pr_closer(PR, NWO)),
        issue(2, json!({"__typename": "Commit", "oid": "pr-commit"})),
        issue(3, pr_closer(PR + 1, NWO)),
        issue(4, pr_closer(PR, "example-org/other")),
        issue(5, Value::Null),
        earlier,
        unrelated_repo,
        empty_timeline,
        open_issue(9),
    ]);
    let targets = close_targets(NWO, PR, &doc).unwrap();
    assert!(targets.has_open);
    assert_eq!(
        targets
            .closed
            .iter()
            .map(|target| target.number)
            .collect::<Vec<_>>(),
        vec![1, 2],
    );
}

#[test]
fn refuses_ambiguous_or_incomplete_forge_reads() {
    let good = report(vec![issue(1, pr_closer(PR, NWO))]);
    assert!(close_targets(NWO, PR + 1, &good).is_err());
    assert!(close_targets("example-org/other", PR, &good).is_err());
    let mut incomplete = good.clone();
    incomplete["data"]["repository"]["pullRequest"]["closingIssuesReferences"]["pageInfo"]
        ["hasNextPage"] = json!(true);
    assert!(close_targets(NWO, PR, &incomplete).is_err());
    let mut missing_time = good.clone();
    missing_time["data"]["repository"]["pullRequest"]["mergedAt"] = Value::Null;
    assert!(close_targets(NWO, PR, &missing_time).is_err());
    let mut partial = good;
    partial["errors"] = json!([{"message": "unreadable node"}]);
    assert!(close_targets(NWO, PR, &partial).is_err());
}

#[test]
fn closes_before_first_read_or_during_one_bounded_retry() {
    let waiting = report(vec![open_issue(1)]);
    let complete = report(vec![issue(1, pr_closer(PR, NWO))]);
    let outcome =
        after_close(|| close_targets(NWO, PR, &complete), || panic!("already closed; no retry"))
            .unwrap();
    assert_eq!(outcome.first().map(|target| target.number), Some(1));

    let waits = Cell::new(0);
    let outcome = after_close(
        || {
            let doc = if waits.get() == 0 {
                &waiting
            } else {
                &complete
            };
            close_targets(NWO, PR, doc)
        },
        || waits.set(waits.get() + 1),
    )
    .unwrap();
    assert_eq!(waits.get(), 1);
    assert_eq!(outcome.first().map(|target| target.number), Some(1));
}

#[test]
fn caps_wait_when_issue_stays_open_and_never_reuses_old_close_evidence() {
    let waiting = report(vec![open_issue(1)]);
    let reads = Cell::new(0);
    let outcome = after_close(
        || {
            reads.set(reads.get() + 1);
            close_targets(NWO, PR, &waiting)
        },
        || {},
    )
    .unwrap();
    assert_eq!(reads.get(), 2);
    assert!(outcome.is_empty());

    let initial = CloseTargets {
        has_open: true,
        closed: vec![ClosedTarget {
            number: 2,
            closed_at: CLOSED.into(),
        }],
    };
    let mut snapshots = [Ok(initial), Err("second read failed".into())].into_iter();
    assert!(after_close(|| snapshots.next().unwrap(), || {}).is_err());
}

#[test]
fn strips_only_workflow_claims_on_fresh_matching_closed_issue() {
    let target = ClosedTarget {
        number: 1,
        closed_at: CLOSED.into(),
    };
    let body = json!({
        "number": 1,
        "repository_url": "https://api.github.com/repos/example-org/work",
        "state": "closed",
        "closed_at": CLOSED,
        "labels": [{"name": "loom:building"}, {"name": "loom:issue"}, {"name": "loom:curated"}],
    });
    assert_eq!(
        labels_to_strip(NWO, &target, &body.to_string()),
        &["loom:building", "loom:issue"]
    );

    let mut already_clean = body.clone();
    already_clean["labels"] = json!([{"name": "loom:curated"}]);
    assert!(labels_to_strip(NWO, &target, &already_clean.to_string()).is_empty());

    for (field, value) in [
        ("state", json!("open")),
        ("number", json!(2)),
        ("closed_at", json!("2026-11-20T01:00:03Z")),
        ("repository_url", json!("https://api.github.com/repos/example-org/other")),
        ("pull_request", json!({})),
    ] {
        let mut changed = body.clone();
        changed[field] = value;
        assert!(labels_to_strip(NWO, &target, &changed.to_string()).is_empty(), "{field}");
    }
    assert!(labels_to_strip(NWO, &target, "broken JSON").is_empty());
}
