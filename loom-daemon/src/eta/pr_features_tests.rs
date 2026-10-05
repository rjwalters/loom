#![allow(clippy::unwrap_used)]

use super::*;
use serde_json::json;

const REPO: &str = "o/r";

fn t(secs: i64) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-04T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
        + Duration::seconds(secs)
}

fn pull_body(state: &str, merged: bool, additions: i64, sha: &str, updated: i64) -> Value {
    json!({
        "state": state,
        "merged_at": if merged { json!(t(updated).to_rfc3339()) } else { Value::Null },
        "additions": additions, "deletions": 3, "changed_files": 2, "commits": 1,
        "head": {"sha": sha},
        "updated_at": t(updated).to_rfc3339(),
    })
}

fn issue_body(body: &str, updated: i64) -> Value {
    json!({"body": body, "user": {"login": "octocat"}, "updated_at": t(updated).to_rfc3339()})
}

fn checks_body(runs: &[(&str, Option<&str>)], total: usize) -> Value {
    let runs: Vec<Value> = runs
        .iter()
        .map(|(status, conclusion)| json!({"status": status, "conclusion": conclusion}))
        .collect();
    json!({"total_count": total, "check_runs": runs})
}

fn read(kind: ReadKind, number: u32, sha: Option<&str>) -> FeatureRead {
    FeatureRead {
        repo: REPO.to_string(),
        kind,
        number,
        sha: sha.map(str::to_string),
    }
}

fn wanted(issue: u32, pr: Option<u32>) -> Wanted {
    Wanted {
        repo: REPO.to_string(),
        issue,
        pr,
        readable: true,
    }
}

fn features_at(
    store: &PrFeatureStore,
    pr: Option<u32>,
    as_of: DateTime<Utc>,
) -> (Features, Vec<FeatureOmitted>) {
    let mut features = Features::default();
    let mut omitted = Vec::new();
    store.write_to(REPO, 1, pr.ok_or("no_pr_yet"), as_of, &mut features, &mut omitted);
    (features, omitted)
}

fn reason_of<'a>(omitted: &'a [FeatureOmitted], name: &str) -> Option<&'a str> {
    omitted
        .iter()
        .find(|o| o.name == name)
        .map(|o| o.reason.as_str())
}

/// A store with PR #10 (for issue #1) and the issue read at `read_at`.
fn answered(read_at: DateTime<Utc>, pull: &Value) -> PrFeatureStore {
    let mut store = PrFeatureStore::default();
    store.answer(&read(ReadKind::Pull, 10, None), Some(pull), read_at);
    store.answer(
        &read(ReadKind::Issue, 1, None),
        Some(&issue_body(
            "x\n<!-- loom:complexity=complex -->\n<!-- loom:points=5 -->",
            -9000,
        )),
        read_at,
    );
    store.answer(
        &read(ReadKind::Checks, 10, Some("abc")),
        Some(&checks_body(
            &[
                ("completed", Some("success")),
                ("in_progress", None),
                ("completed", Some("failure")),
            ],
            3,
        )),
        read_at,
    );
    store
}

#[test]
fn every_source_is_populated_from_reads_before_as_of() {
    let store = answered(t(-60), &pull_body("open", false, 40, "abc", -120));
    let (f, omitted) = features_at(&store, Some(10), t(0));
    assert_eq!(
        (f.pr_additions, f.pr_deletions, f.pr_changed_files, f.pr_commits),
        (Some(40), Some(3), Some(2), Some(1))
    );
    assert_eq!((f.checks_pending, f.checks_failed), (Some(1), Some(1)));
    assert_eq!(f.complexity_marker.as_deref(), Some("complex"));
    assert_eq!(f.points_marker, Some(5));
    assert_eq!(f.author.as_deref(), Some("octocat"));
    assert!(omitted.is_empty(), "{omitted:?}");
}

#[test]
fn a_pr_that_grew_or_merged_after_as_of_never_logs_its_later_size() {
    // Read after `as_of`, and the PR changed after `as_of` (it grew): the
    // later size is not the size at `as_of`.
    let grew = answered(t(60), &pull_body("open", false, 900, "abc", 30));
    let (f, omitted) = features_at(&grew, Some(10), t(0));
    assert_eq!(f.pr_additions, None);
    assert_eq!(reason_of(&omitted, "pr_additions"), Some(reason::PR_CHANGED_AFTER_AS_OF));
    assert_eq!(reason_of(&omitted, "checks_pending"), Some(reason::PR_CHANGED_AFTER_AS_OF));

    // Merged: the final size is never logged, whenever it was read.
    for read_at in [t(-60), t(60)] {
        let merged = answered(read_at, &pull_body("closed", true, 900, "abc", -30));
        let (f, omitted) = features_at(&merged, Some(10), t(0));
        assert_eq!(f.pr_additions, None);
        assert_eq!(reason_of(&omitted, "pr_additions"), Some(reason::PR_NOT_OPEN));
    }

    // Read after `as_of` but unchanged since before it: the value held then.
    let unchanged = answered(t(60), &pull_body("open", false, 40, "abc", -30));
    let (f, _) = features_at(&unchanged, Some(10), t(0));
    assert_eq!(f.pr_additions, Some(40));
    // Check runs change without touching `updated_at`: never read after.
    assert_eq!(f.checks_pending, None);
}

#[test]
fn an_answer_older_than_its_max_age_is_stale() {
    let store = answered(t(-PR_MAX_AGE_SEC - 1), &pull_body("open", false, 40, "abc", -9000));
    let (f, omitted) = features_at(&store, Some(10), t(0));
    assert_eq!(f.pr_additions, None);
    assert_eq!(reason_of(&omitted, "pr_additions"), Some(reason::READ_STALE));
    // The issue's max age is longer, so its markers still hold.
    assert_eq!(f.points_marker, Some(5));
    let (f, omitted) = features_at(&store, Some(10), t(ISSUE_MAX_AGE_SEC));
    assert_eq!(f.author, None);
    assert_eq!(reason_of(&omitted, "author"), Some(reason::READ_STALE));
}

#[test]
fn reads_over_the_budget_are_deferred_and_say_so() {
    let mut store = PrFeatureStore::default();
    let items: Vec<Wanted> = (1..=5).map(|i| wanted(i, Some(100 + i))).collect();
    let reads = store.plan(&items, t(-100), 3);
    assert_eq!(reads.len(), 3);
    for r in &reads {
        store.answer(r, None, t(-90)); // and they all fail
    }
    let mut seen_budget = false;
    for item in &items {
        let mut features = Features::default();
        let mut omitted = Vec::new();
        store.write_to(REPO, item.issue, Ok(item.pr.unwrap()), t(0), &mut features, &mut omitted);
        let why = reason_of(&omitted, "author").unwrap();
        assert!([reason::BUDGET_EXHAUSTED, reason::READ_FAILED].contains(&why), "{why}");
        seen_budget |= why == reason::BUDGET_EXHAUSTED;
    }
    assert!(seen_budget);
}

#[test]
fn the_reads_per_pass_never_exceed_the_budget_and_every_item_is_read_in_turn() {
    let mut store = PrFeatureStore::default();
    let items: Vec<Wanted> = (1..=20).map(|i| wanted(i, Some(100 + i))).collect();
    let mut read_issues = std::collections::BTreeSet::new();
    for pass in 0..10 {
        let now = t(pass * 300);
        let reads = store.plan(&items, now, FEATURE_READ_BUDGET);
        assert!(reads.len() <= FEATURE_READ_BUDGET, "pass {pass}: {}", reads.len());
        for r in &reads {
            let body = match r.kind {
                ReadKind::Pull => pull_body("open", false, 1, "sha", -1),
                ReadKind::Issue => {
                    read_issues.insert(r.number);
                    issue_body("", -1)
                }
                ReadKind::Checks => checks_body(&[], 0),
            };
            store.answer(r, Some(&body), now);
        }
    }
    assert_eq!(read_issues.len(), items.len(), "the round robin reaches every item");
}

#[test]
fn reads_that_keep_failing_do_not_starve_healthy_reads() {
    let mut store = PrFeatureStore::default();
    // 12 PRs whose pull reads always fail, plus 3 healthy issues without a PR.
    let mut items: Vec<Wanted> = (1..=12).map(|i| wanted(i, Some(100 + i))).collect();
    items.extend((20..=22).map(|i| wanted(i, None)));
    let mut read_issues = std::collections::BTreeSet::new();
    for pass in 0..4 {
        let now = t(pass * 300);
        let reads = store.plan(&items, now, 12);
        for r in &reads {
            match r.kind {
                ReadKind::Pull => store.answer(r, None, now),
                ReadKind::Issue if r.number >= 20 => {
                    read_issues.insert(r.number);
                    store.answer(r, Some(&issue_body("", -1)), now);
                }
                _ => store.answer(r, None, now),
            }
        }
    }
    assert_eq!(read_issues.len(), 3, "healthy reads are attempted despite failing ones");
}

#[test]
fn unreadable_items_keep_their_answers_but_plan_nothing() {
    let mut store = answered(t(-60), &pull_body("open", false, 40, "abc", -120));
    let mut item = wanted(1, Some(10));
    item.readable = false;
    assert!(store.plan(&[item], t(10_000), 10).is_empty());
    assert!(store.pulls.contains_key(&(REPO.to_string(), 10)));
    // An item no longer wanted is forgotten.
    assert!(store.plan(&[], t(10_000), 10).is_empty());
    assert!(store.pulls.is_empty() && store.issues.is_empty() && store.checks.is_empty());
}

#[test]
fn checks_are_planned_for_the_head_the_pr_read_shows() {
    let mut store = PrFeatureStore::default();
    let reads = store.plan(&[wanted(1, Some(10))], t(0), 10);
    assert!(reads.iter().all(|r| r.kind != ReadKind::Checks), "no head known yet");
    store.answer(
        &read(ReadKind::Pull, 10, None),
        Some(&pull_body("open", false, 1, "new", -5)),
        t(1),
    );
    store.answer(&read(ReadKind::Checks, 10, Some("old")), Some(&checks_body(&[], 0)), t(1));
    let (f, omitted) = features_at(&store, Some(10), t(2));
    assert_eq!(f.checks_pending, None);
    assert_eq!(reason_of(&omitted, "checks_pending"), Some(reason::CHECKS_FOR_OTHER_HEAD));
    let reads = store.plan(&[wanted(1, Some(10))], t(2), 10);
    let checks: Vec<_> = reads
        .iter()
        .filter(|r| r.kind == ReadKind::Checks)
        .collect();
    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0].sha.as_deref(), Some("new"));
}

#[test]
fn a_truncated_check_page_is_not_a_count() {
    let mut store = answered(t(-60), &pull_body("open", false, 40, "abc", -120));
    store.answer(&read(ReadKind::Checks, 10, Some("abc")), Some(&checks_body(&[], 150)), t(-60));
    let (_, omitted) = features_at(&store, Some(10), t(0));
    assert_eq!(reason_of(&omitted, "checks_failed"), Some(reason::CHECKS_TRUNCATED));
}

#[test]
fn a_failed_read_keeps_the_last_answer() {
    let mut store = answered(t(-600), &pull_body("open", false, 40, "abc", -1200));
    store.answer(&read(ReadKind::Pull, 10, None), None, t(-60));
    let (f, _) = features_at(&store, Some(10), t(0));
    assert_eq!(f.pr_additions, Some(40));
}

#[test]
fn markers_absent_or_invalid_carry_their_own_reason() {
    let mut store = PrFeatureStore::default();
    store.answer(
        &read(ReadKind::Issue, 1, None),
        Some(&issue_body("<!-- loom:points=4 -->", -100)),
        t(-60),
    );
    let (f, omitted) = features_at(&store, None, t(0));
    assert_eq!(f.points_marker, None);
    assert_eq!(reason_of(&omitted, "points_marker"), Some(reason::MARKER_INVALID));
    assert_eq!(reason_of(&omitted, "complexity_marker"), Some(reason::MARKER_ABSENT));
    assert_eq!(reason_of(&omitted, "pr_additions"), Some("no_pr_yet"));
    assert_eq!(f.author.as_deref(), Some("octocat"));
}

#[test]
fn every_read_feature_is_a_value_or_has_exactly_one_reason() {
    let names: Vec<&str> = PR_SIZE_FEATURES
        .iter()
        .chain(&CHECK_FEATURES)
        .chain(&ISSUE_FEATURES)
        .copied()
        .collect();
    for name in &names {
        assert!(Features::NAMES.contains(name), "{name} is declared");
    }
    let stores = [
        PrFeatureStore::default(),
        answered(t(-60), &pull_body("open", false, 40, "abc", -120)),
        answered(t(60), &pull_body("open", false, 40, "abc", 30)),
        answered(t(-60), &pull_body("closed", true, 40, "abc", -120)),
    ];
    for (i, store) in stores.iter().enumerate() {
        for pr in [None, Some(10)] {
            let (f, omitted) = features_at(store, pr, t(0));
            let value = serde_json::to_value(&f).unwrap();
            for name in &names {
                let n = omitted.iter().filter(|o| o.name == *name).count();
                assert_eq!(usize::from(value[*name].is_null()), n, "{name} store {i} pr {pr:?}");
            }
        }
    }
}
