//! The captain's forge facts (W12 part 2): the pure reducers, and that a
//! dispatcher's rows from published facts are the rows of its own listing.

use chrono::{DateTime, Duration, TimeZone, Utc};

use super::{
    as_listing, blocked_facts, blocked_job, star_count, star_free, star_job, RepoListings,
};
use crate::forge_listing::RestIssue;
use crate::observability::queue_blocked::blocked_rows;
use crate::telemetry::queue_snapshot::QueueRepoRef;
use crate::telemetry::RepoVisibility;

const STAR: &str = "loom:operator-priority";
const HIGH: &str = "loom:operator-high-priority";

fn at(mins: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 6, 12, 0, 0).unwrap() + Duration::minutes(mins)
}

fn item(number: u32, labels: &[&str]) -> RestIssue {
    RestIssue {
        number,
        title: Some("a private title".into()),
        labels: labels.iter().map(|l| (*l).to_string()).collect(),
        created_at: Some("2026-09-01T00:00:00Z".into()),
        updated_at: Some("2026-09-02T00:00:00Z".into()),
        closed_at: None,
        state: "open".into(),
        body: Some("Blocked on the vendor contract".into()),
        author: Some("someone".into()),
        is_pull_request: false,
        comments: 3,
    }
}

fn pull(number: u32, labels: &[&str]) -> RestIssue {
    RestIssue {
        is_pull_request: true,
        ..item(number, labels)
    }
}

fn listings(slug: &str, star: Option<Vec<Vec<RestIssue>>>) -> RepoListings {
    RepoListings {
        slug: slug.into(),
        star,
        blocked: None,
    }
}

#[test]
fn starred_issues_are_counted_once_and_starred_prs_are_not() {
    assert_eq!(star_count(&[Vec::new(), Vec::new()]), 0);
    let both_levels = vec![
        vec![item(1, &[STAR]), item(2, &[STAR, HIGH]), pull(3, &[STAR])],
        vec![item(2, &[STAR, HIGH]), item(4, &[HIGH])],
    ];
    assert_eq!(star_count(&both_levels), 3, "#2 carries both labels; #3 is Builder's PR copy");
    // The liveness pass evaluates nothing for a repo whose only starred item
    // is a PR, so that repo counts as star-free.
    assert_eq!(star_count(&[vec![pull(3, &[STAR])]]), 0);
}

#[test]
fn a_repo_is_covered_only_when_every_label_listing_succeeded() {
    let repos = [
        listings("acme/quiet", Some(vec![Vec::new(), Vec::new()])),
        listings("acme/starred", Some(vec![vec![item(1, &[STAR])], Vec::new()])),
        // A listing failed (rate-limited reader, forge down): not covered.
        listings("acme/unread", None),
    ];
    let job = star_job(&repos, &[STAR, HIGH], at(0));
    assert_eq!(job.as_of, at(0));
    assert!(job.repos.contains("acme/quiet") && job.repos.contains("acme/starred"));
    assert!(!job.repos.contains("acme/unread"));
    assert_eq!(job.counts.get("acme/starred"), Some(&1));
    assert!(!job.counts.contains_key("acme/quiet"), "zero is the absent entry");

    let free = star_free(&job, &[STAR, HIGH]);
    assert_eq!(free.len(), 1, "only the covered repo with no star: {free:?}");
    assert_eq!(free.get("acme/quiet"), Some(&at(0)));
}

#[test]
fn a_label_the_captain_did_not_list_means_nothing_is_believed() {
    let repos = [listings("acme/quiet", Some(vec![Vec::new()]))];
    // The captain's level table has one level; this host's has two. A star at
    // the level the captain never listed would be invisible to it.
    let job = star_job(&repos, &[STAR], at(0));
    assert_eq!(star_free(&job, &[STAR]).len(), 1);
    assert!(star_free(&job, &[STAR, HIGH]).is_empty());
    assert!(star_free(&job, &[]).is_empty());
    // A heartbeat from before the label set was published.
    let mut unlabelled = job;
    unlabelled.labels.clear();
    assert!(star_free(&unlabelled, &[STAR]).is_empty());
}

#[test]
fn rows_from_the_captains_facts_are_the_rows_of_the_listing() {
    let listing = vec![
        item(5, &["loom:blocked", "tier:goal-advancing", STAR, "loom:operator"]),
        item(6, &["loom:blocked", "loom:issue"]),
        pull(7, &["loom:blocked"]),
        item(
            8,
            &[
                "loom:blocked",
                "customer:acme-corp",
                "loom:needs-capability",
            ],
        ),
    ];
    let facts = blocked_facts(&listing);
    assert_eq!(facts.iter().map(|f| f.number).collect::<Vec<_>>(), [5, 8]);
    for visibility in [RepoVisibility::Public, RepoVisibility::Private] {
        let repo = QueueRepoRef {
            repo: "acme/app".into(),
            visibility,
        };
        assert_eq!(
            blocked_rows(&repo, &as_listing(&facts)),
            blocked_rows(&repo, &listing),
            "a dispatcher's rows do not depend on who listed"
        );
    }
}

#[test]
fn published_facts_carry_label_names_and_nothing_else() {
    let listing = vec![item(8, &["loom:blocked", "customer:acme-corp", "tier:2"])];
    let repos = [RepoListings {
        slug: "acme/app".into(),
        star: None,
        blocked: Some(listing),
    }];
    let job = blocked_job(&repos, at(0));
    let json = serde_json::to_string(&job).unwrap();
    for leak in [
        "private title",
        "vendor contract",
        "someone",
        "customer:acme-corp",
    ] {
        assert!(!json.contains(leak), "{leak} must not be published: {json}");
    }
    assert!(json.contains(r#"{"n":8,"c":"2026-09-01T00:00:00Z","l":["loom:blocked","tier:2"]}"#));
}

#[test]
fn blocked_coverage_distinguishes_none_blocked_from_not_listed() {
    let repos = [
        RepoListings {
            slug: "acme/clear".into(),
            star: None,
            blocked: Some(Vec::new()),
        },
        RepoListings {
            slug: "acme/unread".into(),
            star: None,
            blocked: None,
        },
    ];
    let job = blocked_job(&repos, at(0));
    assert!(job.repos.contains("acme/clear"), "covered: a dispatcher appends no row");
    assert!(!job.repos.contains("acme/unread"), "not covered: a dispatcher lists it itself");
    assert!(job.blocked.is_empty());
}
