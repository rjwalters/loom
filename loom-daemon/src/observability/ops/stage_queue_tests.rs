//! Queue-state classification (Issue #11847). Pure: rows are parsed from the
//! REST listing shape, no forge.

use std::collections::BTreeMap;
use std::path::PathBuf;

use super::{queue_counts, QUEUE_STATES};
use crate::forge_listing::{parse_rest_issues, RestIssue};
use crate::observability::ops::stage_dwell::{stage_batches, stage_points, DwellSample, RepoInput};
use crate::telemetry::ops::{
    bounded_labels, MetricName, MetricPoint, MetricPointsRecord, MAX_POINTS_PER_RECORD,
};

fn row(number: u32, labels: &[&str], pr_body: Option<&str>) -> RestIssue {
    let labels: Vec<_> = labels
        .iter()
        .map(|l| serde_json::json!({"name": l}))
        .collect();
    let mut item = serde_json::json!({"number": number, "state": "open", "labels": labels});
    if let Some(body) = pr_body {
        item["pull_request"] = serde_json::json!({"url": "u"});
        item["body"] = serde_json::json!(body);
    }
    parse_rest_issues(&serde_json::json!([item]).to_string())
        .unwrap()
        .remove(0)
}

fn counts(rows: &[RestIssue]) -> BTreeMap<&'static str, usize> {
    queue_counts(rows)
}

#[test]
fn every_state_is_measured_zero_included() {
    let empty = counts(&[]);
    assert_eq!(empty.len(), QUEUE_STATES.len());
    assert!(empty.values().all(|n| *n == 0));
}

#[test]
fn triage_and_unlabeled_issues() {
    let rows = [
        row(1, &["loom:triage"], None),
        row(2, &["loom:triage", "bug"], None),
        row(3, &[], None),
        row(4, &["bug", "points:3"], None),
        row(5, &["loom:issue"], None),
        // A PR with no Loom label is not an unlabeled issue.
        row(6, &[], Some("")),
    ];
    let c = counts(&rows);
    assert_eq!(c["triage"], 2);
    assert_eq!(c["unlabeled"], 2, "issues 3 and 4: no `loom:` label at all");
}

#[test]
fn curated_not_promoted_excludes_the_ready_queue_and_builds() {
    let rows = [
        row(1, &["loom:curated"], None),
        row(2, &["loom:curated", "loom:issue"], None),
        row(3, &["loom:curated", "loom:building"], None),
        row(4, &["loom:curated", "loom:operator"], None),
    ];
    let c = counts(&rows);
    assert_eq!(c["curated_not_promoted"], 2, "issues 1 and 4");
}

#[test]
fn the_ready_queue_splits_on_an_open_pr_closing_the_issue() {
    let rows = [
        row(10, &["loom:issue"], None),
        row(11, &["loom:issue"], None),
        row(12, &["loom:issue"], None),
        row(20, &["loom:review-requested"], Some("Closes #10\n\nPart of #12")),
        // A PR that closes a non-ready issue changes nothing.
        row(21, &[], Some("Fixes #99")),
    ];
    let c = counts(&rows);
    assert_eq!(c["issue_open_pr"], 1, "#10 is closed by PR #20");
    assert_eq!(c["issue_no_pr"], 2, "`Part of` is not a closing reference");
    assert_eq!(c["issue_open_pr"] + c["issue_no_pr"], 3, "the split covers loom:issue");
}

#[test]
fn parked_issues_and_held_prs_follow_the_registry() {
    let rows = [
        row(1, &["loom:blocked"], None),
        row(2, &["loom:operator", "loom:issue"], None),
        row(3, &["loom:frozen"], None),
        // A retired name folds into its replacement (#11795).
        row(4, &["loom:operator-only"], None),
        row(5, &["loom:triage"], None),
        row(30, &["loom:pr", "loom:operator"], Some("Closes #2")),
        row(31, &["loom:pr"], Some("")),
        row(32, &["loom:review-requested", "loom:blocked"], Some("")),
    ];
    let c = counts(&rows);
    assert_eq!(c["parked"], 4);
    assert_eq!(c["pr_held"], 2, "PRs 30 and 32; an approved PR without a hold is not held");
    assert_eq!(c["issue_open_pr"], 1, "states overlap: #2 is parked and has an open PR");
}

#[test]
fn queue_states_export_per_repo_only_when_the_walk_completed() {
    let measured = RepoInput {
        slug: "Acme/App".into(),
        root: PathBuf::from("/w/app"),
        listings: BTreeMap::new(),
        queue: Some(counts(&[row(1, &["loom:triage"], None)])),
    };
    let unmeasured = RepoInput {
        slug: "acme/other".into(),
        queue: None,
        ..measured.clone()
    };
    let points = stage_points(&[], &[measured, unmeasured]);
    let item = |n, state: &str, repo: &str| {
        MetricPoint::int(MetricName::ForgeStageItems, n)
            .label("state", state)
            .label("repo", repo)
    };
    assert!(points.contains(&item(1, "triage", "acme/app")));
    assert!(points.contains(&item(0, "unlabeled", "acme/app")), "measured zero");
    assert!(points.contains(&item(0, "curated", "acme/other")), "label states unaffected");
    assert!(
        !points
            .iter()
            .any(|p| p.labels.get("repo").is_some_and(|r| r == "acme/other")
                && p.labels.get("state").is_some_and(|s| s == "triage")),
        "no completed walk: absent, not 0"
    );
    for point in &points {
        assert_eq!(bounded_labels(&point.labels), point.labels);
    }
}

/// The fleet captain samples dozens of repos: one record would be cut at
/// `MAX_POINTS_PER_RECORD` by `bounded_points`, silently dropping every repo
/// past the cut. Batched, every repo's every state survives the bound.
#[test]
fn a_many_repo_sample_survives_the_per_record_bound() {
    let repos = 62;
    let inputs: Vec<RepoInput> = (0..repos)
        .map(|n| RepoInput {
            slug: format!("acme/r{n}"),
            root: PathBuf::from("/w"),
            listings: BTreeMap::new(),
            queue: Some(counts(&[])),
        })
        .collect();
    let samples = [DwellSample {
        stage: "created_to_curated",
        seconds: 5,
    }];
    let batches = stage_batches(&samples, &inputs);
    assert!(batches.len() > 1, "{repos} repos must not fit one record");
    let mut kept = Vec::new();
    for batch in batches {
        assert!(batch.len() <= MAX_POINTS_PER_RECORD);
        let record = MetricPointsRecord {
            captured_at: chrono::Utc::now(),
            interval_start: None,
            points: batch,
        };
        kept.extend(record.bounded_points());
    }
    assert_eq!(kept.len(), stage_points(&samples, &inputs).len(), "nothing truncated");
    for n in 0..repos {
        let repo = format!("acme/r{n}");
        let states = kept
            .iter()
            .filter(|p| p.name == MetricName::ForgeStageItems)
            .filter(|p| p.labels.get("repo") == Some(&repo))
            .count();
        assert_eq!(states, 6 + QUEUE_STATES.len(), "{repo} lost states");
    }
    assert!(
        kept.iter()
            .take(2)
            .all(|p| p.name != MetricName::ForgeStageItems),
        "dwell pairs lead the first batch"
    );
}

/// #11905: held approvals (`loom:pr` + `loom:blocked`, by label) are their own
/// state, so they stay visible once they leave `loom:operator`; an
/// operator-held approval is not counted twice.
#[test]
fn champion_held_prs_are_a_sibling_state() {
    let rows = [
        row(40, &["loom:pr", "loom:blocked"], Some("")),
        row(41, &["loom:pr", "loom:blocked", "loom:operator"], Some("")),
        row(42, &["loom:pr", "loom:operator"], Some("")),
        row(43, &["loom:changes-requested", "loom:blocked"], Some("")),
        row(44, &["loom:pr", "loom:blocked"], None),
    ];
    let c = counts(&rows);
    assert_eq!(c["pr_champion_held"], 1, "PR 40 only; 44 is an issue");
    assert_eq!(c["pr_held"], 4, "pr_held is unchanged and a superset");
}

/// #12027: `pr_refused` counts this host's standing refusals on open
/// `loom:pr` PRs, by reason, with the oldest refusal's age.
#[test]
fn pr_refused_counts_live_refusals_by_reason_with_the_oldest_age() {
    use crate::observability::step_verdict::refusals::{Observed, RefusalStore};
    use chrono::TimeZone;
    let t0 = chrono::Utc.with_ymd_and_hms(2026, 10, 15, 3, 0, 0).unwrap();
    let mut store = RefusalStore::default();
    let refuse =
        |store: &mut RefusalStore, pr: u64, criterion: &str, reason: &str, minutes: i64| {
            store.observe(
                "acme/app",
                pr,
                &Observed {
                    head_sha: Some("abc1234"),
                    criterion_key: criterion,
                    reason_key: "k",
                    reason,
                    at: t0 + chrono::Duration::minutes(minutes),
                },
            );
        };
    refuse(&mut store, 11277, "ci-status", "ci_failing", 0);
    refuse(&mut store, 11313, "ci-status", "ci_failing", 30);
    refuse(&mut store, 50, "merge-conflict", "conflict", 10);
    refuse(&mut store, 51, "ci-status", "ci_failing", 40);
    // The walk: #51 lost `loom:pr`, #50 was merged (gone from the open walk).
    let rows = [
        row(11277, &["loom:pr"], Some("")),
        row(11313, &["loom:pr"], Some("")),
        row(51, &["loom:review-requested"], Some("")),
        row(52, &["loom:pr"], None),
    ];
    let open = super::approved_open(&rows);
    assert_eq!(open.into_iter().collect::<Vec<_>>(), vec![11277, 11313], "52 is an issue");
    store.reconcile("acme/app", &super::approved_open(&rows));
    let now = t0 + chrono::Duration::hours(2);
    let points = super::refused_points(&store, "acme/app", now);
    let get = |name: MetricName, reason: &str| {
        points
            .iter()
            .find(|p| {
                p.name == name
                    && p.labels.get("reason").map(String::as_str) == Some(reason)
                    && p.labels.get("state").map(String::as_str) == Some("pr_refused")
                    && p.labels.get("repo").map(String::as_str) == Some("acme/app")
            })
            .map(|p| p.value)
    };
    use crate::telemetry::ops::MetricValue;
    assert_eq!(get(MetricName::ForgeStageItems, "ci_failing"), Some(MetricValue::Int(2)));
    assert_eq!(get(MetricName::ForgeStageItems, "conflict"), Some(MetricValue::Int(0)));
    assert_eq!(
        get(MetricName::ForgeStageOldestAgeSeconds, "ci_failing"),
        Some(MetricValue::Int(7200)),
        "now - min(first_refused_at)"
    );
    assert_eq!(
        get(MetricName::ForgeStageOldestAgeSeconds, "conflict"),
        None,
        "no refusal, no age"
    );
    // A moved head: the standing refusal no longer applies.
    assert!(store
        .touch("acme/app", 11277, Some("fffffff"), now)
        .is_none());
    assert!(super::refused_points(&store, "acme/app", now)
        .iter()
        .any(|p| p.name == MetricName::ForgeStageItems
            && p.labels.get("reason").map(String::as_str) == Some("ci_failing")
            && p.value == MetricValue::Int(1)));
    // A repo this host never refused on (or whose walk was shed): absent.
    assert!(super::refused_points(&store, "acme/other", now).is_empty());
    for point in &points {
        assert_eq!(bounded_labels(&point.labels), point.labels);
    }
}
