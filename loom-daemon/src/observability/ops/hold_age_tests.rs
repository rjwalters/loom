//! Hold-age points (#12038, #12369). Pure: rows come from a fake snapshot.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};

use super::{hold_labels, nearest_rank, points_from_rows, points_from_snapshot};
use crate::discovery::{RepoSnapshot, RepoStatus, Source, Tier};
use crate::forge_listing::RestIssue;
use crate::telemetry::ops::{MetricName, MetricPoint, MetricValue};

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-15T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

fn ago(secs: i64) -> String {
    (now() - Duration::seconds(secs)).to_rfc3339()
}

/// One open row as the mirror serves it; `labels` are `(label, applied-at)`
/// (`None`: no time sent).
fn row(number: u32, pr: bool, labels: &[(&str, Option<String>)], updated_at: &str) -> RestIssue {
    let at: BTreeMap<String, String> = labels
        .iter()
        .filter_map(|(l, t)| t.as_ref().map(|t| ((*l).to_string(), t.clone())))
        .collect();
    RestIssue {
        number,
        title: None,
        labels: labels.iter().map(|(l, _)| (*l).to_string()).collect(),
        created_at: None,
        updated_at: Some(updated_at.to_string()),
        closed_at: None,
        state: "open".to_string(),
        body: None,
        author: None,
        author_association: None,
        is_pull_request: pr,
        comments: 0,
        draft: false,
        open_sub_issues: None,
        labels_at: (!at.is_empty()).then_some(at),
    }
}

fn blocked(number: u32, secs: i64) -> RestIssue {
    row(number, false, &[("loom:blocked", Some(ago(secs)))], "2026-10-15T11:59:00Z")
}

fn get(points: &[MetricPoint], name: MetricName, want: &[(&str, &str)]) -> Option<i64> {
    points
        .iter()
        .find(|p| {
            p.name == name
                && want
                    .iter()
                    .all(|(k, v)| p.labels.get(*k).map(String::as_str) == Some(*v))
        })
        .map(|p| match p.value {
            MetricValue::Int(n) => n,
            MetricValue::Double(_) => panic!("hold points are integers"),
        })
}

#[test]
fn nearest_rank_for_odd_even_and_single() {
    assert_eq!(nearest_rank(&[7], 90), 7);
    assert_eq!(nearest_rank(&[7], 50), 7);
    let odd = [10, 20, 30, 40, 50];
    assert_eq!(nearest_rank(&odd, 50), 30);
    assert_eq!(nearest_rank(&odd, 90), 50);
    assert_eq!(nearest_rank(&odd, 100), 50);
    let even = [10, 20, 30, 40];
    assert_eq!(nearest_rank(&even, 50), 20);
    assert_eq!(nearest_rank(&even, 90), 40);
}

#[test]
fn counts_and_ages_per_artifact_and_hold() {
    let rows = vec![
        blocked(1, 100),
        blocked(2, 300),
        blocked(3, 200),
        row(4, true, &[("loom:blocked", Some(ago(50)))], "2026-10-15T11:00:00Z"),
        row(5, false, &[("loom:frozen", Some(ago(9)))], "2026-10-15T11:00:00Z"),
    ];
    let p = points_from_rows("Owner/Repo", &rows, now());
    let b = [
        ("repo", "owner/repo"),
        ("artifact", "issue"),
        ("hold", "blocked"),
    ];
    let with = |extra: (&'static str, &'static str)| {
        let mut v = b.to_vec();
        v.push(extra);
        v
    };
    assert_eq!(get(&p, MetricName::ForgeHoldItems, &with(("basis", "labels_at"))), Some(3));
    assert_eq!(get(&p, MetricName::ForgeHoldAgeSeconds, &with(("stat", "oldest"))), Some(300));
    assert_eq!(get(&p, MetricName::ForgeHoldAgeSeconds, &with(("stat", "p50"))), Some(200));
    assert_eq!(get(&p, MetricName::ForgeHoldAgeSeconds, &with(("stat", "p90"))), Some(300));
    let pr = [("artifact", "pr"), ("hold", "blocked")];
    assert_eq!(get(&p, MetricName::ForgeHoldItems, &pr), Some(1));
    assert_eq!(
        get(&p, MetricName::ForgeHoldAgeSeconds, &[("artifact", "pr"), ("stat", "oldest")]),
        Some(50)
    );
    assert_eq!(get(&p, MetricName::ForgeHoldItems, &[("hold", "frozen")]), Some(1));
}

#[test]
fn age_is_labels_at_never_updated_at() {
    // updated_at is "now" (a fresh notice); the label went on 1000s ago.
    let r = row(1, false, &[("loom:blocked", Some(ago(1000)))], &now().to_rfc3339());
    let p = points_from_rows("o/r", &[r], now());
    assert_eq!(get(&p, MetricName::ForgeHoldAgeSeconds, &[("stat", "oldest")]), Some(1000));
}

#[test]
fn a_row_without_a_time_is_unknown_and_outside_the_percentiles() {
    let rows = vec![
        blocked(1, 40),
        row(2, false, &[("loom:blocked", None)], "2026-10-15T11:00:00Z"),
        row(3, false, &[("loom:blocked", Some("not a time".to_string()))], "x"),
    ];
    let p = points_from_rows("o/r", &rows, now());
    assert_eq!(get(&p, MetricName::ForgeHoldItems, &[("basis", "unknown")]), Some(2));
    assert_eq!(get(&p, MetricName::ForgeHoldItems, &[("basis", "labels_at")]), Some(1));
    assert_eq!(get(&p, MetricName::ForgeHoldAgeSeconds, &[("stat", "p50")]), Some(40));
}

#[test]
fn all_unknown_emits_a_count_and_no_ages() {
    let r = row(1, false, &[("loom:blocked", None)], "2026-10-15T11:00:00Z");
    let p = points_from_rows("o/r", &[r], now());
    assert_eq!(get(&p, MetricName::ForgeHoldItems, &[("basis", "unknown")]), Some(1));
    assert!(p.iter().all(|p| p.name != MetricName::ForgeHoldAgeSeconds));
}

#[test]
fn two_hold_labels_count_under_each_and_a_repeated_row_once() {
    let both = row(
        1,
        false,
        &[
            ("loom:blocked", Some(ago(10))),
            ("loom:operator", Some(ago(20))),
        ],
        "2026-10-15T11:00:00Z",
    );
    let p = points_from_rows("o/r", &[both.clone(), both], now());
    assert_eq!(get(&p, MetricName::ForgeHoldItems, &[("hold", "blocked")]), Some(1));
    assert_eq!(get(&p, MetricName::ForgeHoldItems, &[("hold", "operator")]), Some(1));
}

#[test]
fn rows_without_a_hold_label_and_empty_input_emit_nothing() {
    let plain = row(1, false, &[("loom:issue", Some(ago(10)))], "2026-10-15T11:00:00Z");
    assert!(points_from_rows("o/r", &[plain], now()).is_empty());
    assert!(points_from_rows("o/r", &[], now()).is_empty());
}

#[test]
fn a_future_label_time_clamps_to_zero() {
    let r = row(1, false, &[("loom:blocked", Some(ago(-500)))], "2026-10-15T11:00:00Z");
    let p = points_from_rows("o/r", &[r], now());
    assert_eq!(get(&p, MetricName::ForgeHoldAgeSeconds, &[("stat", "oldest")]), Some(0));
}

#[test]
fn hold_labels_cover_blocked_operator_and_frozen_once_each() {
    let labels = hold_labels();
    for want in ["loom:blocked", "loom:operator", "loom:frozen"] {
        assert_eq!(labels.iter().filter(|l| **l == want).count(), 1, "{want}");
    }
}

/// No new GitHub calls: the module neither shells out nor lists through REST.
#[test]
fn the_module_reads_no_forge() {
    let src = include_str!("hold_age.rs");
    let code = src.split("#[cfg(test)]").next().unwrap();
    for banned in ["gh_invocation", "queue_blocked", "std::process", "Command"] {
        assert!(!code.contains(banned), "hold_age.rs must not use {banned}");
    }
    for (i, _) in code.match_indices("forge_listing::") {
        assert!(
            code[i..].starts_with("forge_listing::RestIssue"),
            "hold_age.rs may use only the RestIssue type from forge_listing"
        );
    }
}

fn repo(slug: &str, tier: Tier, status: RepoStatus, rows: Vec<RestIssue>) -> RepoSnapshot {
    RepoSnapshot {
        slug: slug.to_string(),
        source: Source::Worklist,
        tier,
        status,
        rows,
        pulls: Some(Vec::new()),
        extras: BTreeMap::new(),
        watermark: None,
    }
}

#[test]
fn a_snapshot_emits_per_repo_and_skips_a_tier_none_repo() {
    let read = repo("Owner/A", Tier::Mirror, RepoStatus::Ok, vec![blocked(1, 60)]);
    let fallback = repo("owner/b", Tier::Gateway, RepoStatus::Unknown, vec![blocked(2, 90)]);
    let unread = repo("owner/c", Tier::None, RepoStatus::Error, vec![blocked(3, 500)]);
    let p = points_from_snapshot([&read, &fallback, &unread], now());
    let of = |r: &'static str| [("repo", r), ("stat", "oldest")];
    assert_eq!(get(&p, MetricName::ForgeHoldAgeSeconds, &of("owner/a")), Some(60));
    assert_eq!(get(&p, MetricName::ForgeHoldAgeSeconds, &of("owner/b")), Some(90));
    assert_eq!(get(&p, MetricName::ForgeHoldAgeSeconds, &of("owner/c")), None);
    assert!(p
        .iter()
        .all(|p| p.labels.get("repo").map(String::as_str) != Some("owner/c")));
}

#[test]
fn no_repos_and_an_empty_repo_emit_nothing() {
    assert!(points_from_snapshot([], now()).is_empty());
    let empty = repo("o/r", Tier::Mirror, RepoStatus::Ok, Vec::new());
    assert!(points_from_snapshot([&empty], now()).is_empty());
}

#[test]
fn an_item_missing_labels_at_counts_as_unknown_in_a_snapshot_repo() {
    let r = repo(
        "o/r",
        Tier::Mirror,
        RepoStatus::Ok,
        vec![row(
            1,
            false,
            &[("loom:blocked", None)],
            "2026-10-15T11:00:00Z",
        )],
    );
    let p = points_from_snapshot([&r], now());
    assert_eq!(get(&p, MetricName::ForgeHoldItems, &[("basis", "unknown")]), Some(1));
}
