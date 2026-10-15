//! The captain's forge facts (W12 part 2): the pure reducer, and that a
//! dispatcher's rows from published facts are the rows of its own listing.

use chrono::{DateTime, Duration, TimeZone, Utc};

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use super::super::store::JobFacts;
use super::{as_listing, blocked_facts, blocked_job, gather, RepoListings, BLOCKED_CALLER};
use crate::forge_listing::RestIssue;
use crate::observability::queue_blocked::blocked_rows;
use crate::telemetry::queue_snapshot::QueueRepoRef;
use crate::telemetry::RepoVisibility;

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
        author_association: None,
        is_pull_request: false,
        comments: 3,
        open_sub_issues: None,
        labels_at: None,
    }
}

fn pull(number: u32, labels: &[&str]) -> RestIssue {
    RestIssue {
        is_pull_request: true,
        ..item(number, labels)
    }
}

#[test]
fn rows_from_the_captains_facts_are_the_rows_of_the_listing() {
    let listing = vec![
        item(
            5,
            &[
                "loom:blocked",
                "points:3",
                "loom:important",
                "loom:operator",
            ],
        ),
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
        blocked: Some(listing),
    }];
    let job = blocked_job(&repos, at(0));
    let json = serde_json::to_string(&job).unwrap();
    for leak in [
        "private title",
        "vendor contract",
        "someone",
        "customer:acme-corp",
        "tier:2",
    ] {
        assert!(!json.contains(leak), "{leak} must not be published: {json}");
    }
    assert!(json.contains(r#"{"n":8,"c":"2026-09-01T00:00:00Z","l":["loom:blocked"]}"#));
}

#[test]
fn blocked_coverage_distinguishes_none_blocked_from_not_listed() {
    let repos = [
        RepoListings {
            slug: "acme/clear".into(),
            blocked: Some(Vec::new()),
        },
        RepoListings {
            slug: "acme/unread".into(),
            blocked: None,
        },
    ];
    let job = blocked_job(&repos, at(0));
    assert!(job.repos.contains("acme/clear"), "covered: a dispatcher appends no row");
    assert!(!job.repos.contains("acme/unread"), "not covered: a dispatcher lists it itself");
    assert!(job.blocked.is_empty());
}

// ---- the pass: what is listed, and when (B1, S2) ---------------------------

/// One listing the pass asked for: `(root, repo, label, caller, at)`.
type Call = (PathBuf, String, &'static str, &'static str, DateTime<Utc>);

/// Run one pass over `targets` against a lister that records every call,
/// answers an empty listing, and takes a few milliseconds per call.
async fn run(targets: &[(&str, &str)]) -> (JobFacts, Vec<Call>) {
    let calls: Arc<Mutex<Vec<Call>>> = Arc::default();
    let log = Arc::clone(&calls);
    let targets = targets
        .iter()
        .map(|(root, slug)| (PathBuf::from(root), (*slug).to_string()))
        .collect();
    let facts = gather(targets, move |root, repo, label, caller| {
        let log = Arc::clone(&log);
        async move {
            log.lock()
                .unwrap()
                .push((root, repo, label, caller, Utc::now()));
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            Some(Vec::new())
        }
    })
    .await;
    let calls = calls.lock().unwrap().clone();
    (facts, calls)
}

#[tokio::test]
async fn every_listing_names_the_slug_the_facts_are_published_under() {
    // A fork checkout: the slug resolves to the upstream (`gh` prefers an
    // `upstream` remote) while `origin` is the fork. The listing must be of
    // the upstream, or the fork's rows are published under its name.
    let (job, calls) = run(&[("/w/fork-checkout", "Acme/App")]).await;
    assert_eq!(calls.len(), 1, "one loom:blocked listing: {calls:?}");
    assert!(calls.iter().all(|(_, repo, ..)| repo == "Acme/App"), "{calls:?}");
    assert!(calls
        .iter()
        .any(|c| c.2 == "loom:blocked" && c.3 == BLOCKED_CALLER));
    assert!(job.repos.contains("acme/app"), "published lowercased");
}

#[tokio::test]
async fn roots_that_resolve_to_one_slug_are_listed_once_and_as_that_slug() {
    let (job, calls) = run(&[
        ("/w/b-clone", "acme/app"),
        ("/w/a-fork", "ACME/app"),
        ("/w/lib", "acme/lib"),
    ])
    .await;
    let app: Vec<_> = calls
        .iter()
        .filter(|c| c.1.eq_ignore_ascii_case("acme/app"))
        .collect();
    assert_eq!(app.len(), 1, "one listing for the repo, not per root: {calls:?}");
    assert!(
        app.iter()
            .all(|c| c.0.as_path() == std::path::Path::new("/w/b-clone")),
        "the first root wins"
    );
    assert_eq!(job.repos.len(), 2);
}

#[tokio::test]
async fn as_of_is_no_later_than_the_first_listing_of_the_pass() {
    let (job, calls) = run(&[("/w/a", "acme/a"), ("/w/b", "acme/b")]).await;
    let first = calls.iter().map(|c| c.4).min().unwrap();
    let last = calls.iter().map(|c| c.4).max().unwrap();
    assert!(last > first, "the pass took time");
    assert!(job.as_of <= first, "{} > {first}", job.as_of);
}
