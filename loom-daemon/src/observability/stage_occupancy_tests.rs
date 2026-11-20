use super::*;
use crate::telemetry::kinds::stage_occupancy::AGE_BUCKET_EDGES_SEC;

fn now() -> DateTime<Utc> {
    "2026-10-20T12:00:00Z".parse().unwrap()
}

fn item(number: u32, labels: &[&str], created_at: &str) -> RestIssue {
    RestIssue {
        comments: 0,
        number,
        title: Some("secret title".into()),
        labels: labels.iter().map(|l| (*l).to_string()).collect(),
        created_at: Some(created_at.into()),
        updated_at: None,
        closed_at: None,
        state: "open".into(),
        body: Some("secret body".into()),
        author: Some("someone".into()),
        author_association: None,
        is_pull_request: false,
        open_sub_issues: None,
        labels_at: None,
        draft: false,
    }
}

#[test]
fn approval_wait_excludes_an_item_already_approved() {
    let curated = item(1, &["loom:curated"], "2026-10-20T00:00:00Z");
    let both = item(2, &["loom:curated", "loom:issue"], "2026-10-20T00:00:00Z");
    assert!(in_stage(STAGE_APPROVAL_WAIT, &curated));
    assert!(!in_stage(STAGE_APPROVAL_WAIT, &both));
    // The other stages do not special-case `loom:issue`.
    assert!(in_stage(
        STAGE_TRIAGE_WAIT,
        &item(3, &["loom:triage", "loom:issue"], "2026-10-20T00:00:00Z")
    ));
}

#[test]
fn pull_requests_and_closed_items_are_not_counted() {
    let pr = RestIssue {
        is_pull_request: true,
        ..item(1, &["loom:triage"], "2026-10-20T00:00:00Z")
    };
    let closed = RestIssue {
        state: "closed".into(),
        ..item(2, &["loom:triage"], "2026-10-20T00:00:00Z")
    };
    assert!(!in_stage(STAGE_TRIAGE_WAIT, &pr));
    assert!(!in_stage(STAGE_TRIAGE_WAIT, &closed));
    let stats = stage_stats(STAGE_TRIAGE_WAIT, &[pr, closed], now());
    assert_eq!(stats, StageStats::default());
}

#[test]
fn age_prefers_the_label_time_and_falls_back_to_creation() {
    let mut curated = item(1, &["loom:curated"], "2026-10-10T12:00:00Z");
    assert_eq!(age_sec(STAGE_APPROVAL_WAIT, &curated, now()), Some(10 * 86_400));
    curated.labels_at =
        Some([("loom:curated".to_string(), "2026-10-20T11:00:00Z".to_string())].into());
    assert_eq!(age_sec(STAGE_APPROVAL_WAIT, &curated, now()), Some(3_600));
    // A label time for some other label does not apply.
    curated.labels_at =
        Some([("loom:triage".to_string(), "2026-10-20T11:30:00Z".to_string())].into());
    assert_eq!(age_sec(STAGE_APPROVAL_WAIT, &curated, now()), Some(10 * 86_400));
}

#[test]
fn age_is_unknown_without_a_parseable_time_and_clamped_for_skew() {
    let mut bad = item(1, &["loom:triage"], "not a time");
    assert_eq!(age_sec(STAGE_TRIAGE_WAIT, &bad, now()), None);
    bad.created_at = None;
    assert_eq!(age_sec(STAGE_TRIAGE_WAIT, &bad, now()), None);
    let future = item(2, &["loom:triage"], "2026-10-21T00:00:00Z");
    assert_eq!(age_sec(STAGE_TRIAGE_WAIT, &future, now()), Some(0));
}

#[test]
fn stage_stats_bucket_by_age() {
    let listing = [
        item(1, &["loom:triage"], "2026-10-20T11:59:00Z"), // 1 min
        item(2, &["loom:triage"], "2026-10-20T11:00:00Z"), // 1 h exactly
        item(3, &["loom:triage"], "2026-10-13T12:00:00Z"), // 7 d exactly
        item(4, &["loom:curated"], "2026-10-01T12:00:00Z"), // other stage
    ];
    let stats = stage_stats(STAGE_TRIAGE_WAIT, &listing, now());
    assert_eq!(stats.count, 3);
    assert_eq!(stats.oldest_age_sec, Some(AGE_BUCKET_EDGES_SEC[4]));
    assert_eq!(stats.age_buckets, vec![1, 1, 0, 0, 0, 1, 0]);
}

fn repo(slug: &str, visibility: RepoVisibility, stages: [Option<StageStats>; 3]) -> RepoOccupancy {
    RepoOccupancy {
        slug: slug.into(),
        visibility,
        stages,
    }
}

fn find<'a>(
    records: &'a [StageOccupancyRecord],
    scope: OccupancyScope,
    repo: Option<&str>,
    stage: &str,
) -> Option<&'a StageOccupancyRecord> {
    records
        .iter()
        .find(|r| r.scope == scope && r.repo.as_deref() == repo && r.stage == stage)
}

#[test]
fn an_empty_scanned_stage_is_a_zero_row() {
    let empty = Some(StageStats::default());
    let records = build_records(
        "host-a",
        now(),
        &[repo(
            "a/b",
            RepoVisibility::Public,
            [empty.clone(), empty.clone(), empty],
        )],
    );
    // 3 repo rows + 3 global rows.
    assert_eq!(records.len(), 6);
    let row = find(&records, OccupancyScope::Repo, Some("a/b"), STAGE_TRIAGE_WAIT).unwrap();
    assert_eq!(row.count, 0);
    assert_eq!(row.oldest_age_sec, None);
    let global = find(&records, OccupancyScope::Global, None, STAGE_TRIAGE_WAIT).unwrap();
    assert_eq!(
        (global.count, global.repos_counted, global.repos_unknown),
        (0, Some(1), Some(0))
    );
}

#[test]
fn an_unknown_stage_emits_nothing_and_is_not_zero() {
    let known = Some(StageStats::from_ages([Some(10)]));
    let records = build_records(
        "host-a",
        now(),
        &[
            repo("a/b", RepoVisibility::Public, [known.clone(), None, None]),
            repo("c/d", RepoVisibility::Public, [None, None, None]),
        ],
    );
    assert!(find(&records, OccupancyScope::Repo, Some("c/d"), STAGE_TRIAGE_WAIT).is_none());
    assert!(find(&records, OccupancyScope::Repo, Some("a/b"), STAGE_APPROVAL_WAIT).is_none());
    // A stage no repo could count has no global row either.
    assert!(find(&records, OccupancyScope::Global, None, STAGE_OPERATOR_HOLD_NAME).is_none());
    // The counted stage's rollup names the repo it could not count.
    let global = find(&records, OccupancyScope::Global, None, STAGE_TRIAGE_WAIT).unwrap();
    assert_eq!(
        (global.count, global.repos_counted, global.repos_unknown),
        (1, Some(1), Some(1))
    );
}

const STAGE_OPERATOR_HOLD_NAME: &str =
    crate::telemetry::kinds::stage_occupancy::STAGE_OPERATOR_HOLD;

#[test]
fn the_global_rollup_sums_repos_and_is_public_only_when_all_are() {
    let a = Some(StageStats::from_ages([Some(10), Some(5_000)]));
    let b = Some(StageStats::from_ages([Some(100_000)]));
    let none = || None;
    let records = build_records(
        "host-a",
        now(),
        &[
            repo("a/b", RepoVisibility::Public, [a, none(), none()]),
            repo("c/d", RepoVisibility::Private, [b, none(), none()]),
        ],
    );
    let global = find(&records, OccupancyScope::Global, None, STAGE_TRIAGE_WAIT).unwrap();
    assert_eq!(global.count, 3);
    assert_eq!(global.oldest_age_sec, Some(100_000));
    assert_eq!(global.age_buckets, vec![1, 1, 0, 1, 0, 0, 0]);
    assert_eq!(global.visibility, RepoVisibility::Private);
    assert_eq!(global.repo, None);
    let only_public = build_records(
        "host-a",
        now(),
        &[repo(
            "a/b",
            RepoVisibility::Public,
            [Some(StageStats::default()), None, None],
        )],
    );
    let g = find(&only_public, OccupancyScope::Global, None, STAGE_TRIAGE_WAIT).unwrap();
    assert_eq!(g.visibility, RepoVisibility::Public);
}

#[test]
fn records_carry_no_forge_text() {
    // The listing rows have a title, body and author; none may reach a record.
    let listing = [item(1, &["loom:triage"], "2026-10-20T11:00:00Z")];
    let stats = stage_stats(STAGE_TRIAGE_WAIT, &listing, now());
    let records = build_records(
        "host-a",
        now(),
        &[repo(
            "a/b",
            RepoVisibility::Public,
            [Some(stats), None, None],
        )],
    );
    let json = serde_json::to_string(&records).unwrap();
    for leak in ["secret title", "secret body", "someone", "loom:triage"] {
        assert!(!json.contains(leak), "{leak} leaked: {json}");
    }
}

#[test]
fn stage_labels_cover_every_stage() {
    assert_eq!(stage_label(STAGE_TRIAGE_WAIT), "loom:triage");
    assert_eq!(stage_label(STAGE_APPROVAL_WAIT), "loom:curated");
    assert_eq!(stage_label(STAGE_OPERATOR_HOLD_NAME), "loom:operator");
}
