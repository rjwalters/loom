//! The `fleet.state` reads (Issue #10196): the paged review listings and the
//! ready-listing truncation check.

use std::path::{Path, PathBuf};

use crate::types::ReadyQueueRow;

/// A `gh` stub serving the open-item listings. `loom:review-requested`
/// answers `rr1.json` on page 1 and `rr2.json` on `&page=2`; the other review
/// labels answer an empty page. A `&page=N` request fails when `fail<N>`
/// exists. Every call's argv is logged to `calls.log`.
fn review_stub(dir: &Path) -> PathBuf {
    let path = dir.join("fake-gh-review.sh");
    std::fs::write(
        &path,
        format!(
            r#"#!/bin/sh
d={dir}
echo "$*" >> "$d/calls.log"
case "$*" in
  *'&page='*)
    n=$(echo "$*" | sed 's/.*&page=\([0-9]*\).*/\1/')
    if [ -f "$d/fail$n" ]; then echo 'gh: Server Error (HTTP 502)' 1>&2; exit 1; fi ;;
esac
case "$*" in
  *'labels=loom:review-requested&'*'&page=2'*) f=rr2.json ;;
  *'labels=loom:review-requested&'*) f=rr1.json ;;
  *) f=empty.json ;;
esac
printf 'HTTP/2.0 200 OK\r\n\r\n'
cat "$d/$f"
"#,
            dir = dir.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(dir.join("empty.json"), "[]\n").unwrap();
    path
}

/// Open PRs `numbers` under `loom:review-requested`, each closing issue
/// `number + 10_000`.
fn pr_page(numbers: std::ops::Range<u32>) -> String {
    let rows: Vec<String> = numbers
        .map(|n| {
            format!(
                r#"{{"number": {n}, "state": "open", "pull_request": {{}},
                    "labels": [{{"name": "loom:review-requested"}}],
                    "body": "Closes #{}"}}"#,
                n + 10_000
            )
        })
        .collect();
    format!("[{}]\n", rows.join(","))
}

/// More than one page of PRs under one review label: every one of them is a
/// listed PR, so none can vanish from the rows or the census.
#[test]
fn a_review_label_with_more_than_a_page_of_prs_lists_every_pr() {
    let dir = tempfile::tempdir().unwrap();
    let repo = format!("test-owner/fleet-review-{}", std::process::id());
    std::fs::write(dir.path().join("rr1.json"), pr_page(1..101)).unwrap();
    std::fs::write(dir.path().join("rr2.json"), pr_page(101..131)).unwrap();
    let gh = review_stub(dir.path());

    let listing = super::review_listing_with(&gh, dir.path(), Some(&repo), "acme/app").unwrap();
    assert_eq!(listing.repo, "acme/app");
    let numbers: Vec<u32> = listing.prs.iter().map(|pr| pr.number).collect();
    assert_eq!(numbers, (1..131).collect::<Vec<_>>());
    assert!(listing
        .prs
        .iter()
        .all(|pr| pr.issue == Some(pr.number + 10_000)));
    let calls = std::fs::read_to_string(dir.path().join("calls.log")).unwrap();
    assert!(calls.contains("&page=2"), "{calls}");

    // Fed to the view, the census counts all of them.
    let input = super::super::FleetInput {
        managed: ["acme/app".to_string()].into_iter().collect(),
        listings: vec![listing],
        ..Default::default()
    };
    let view = super::super::build_view(&input, None, chrono::Utc::now());
    assert_eq!(view.repos["acme/app"].census.as_ref().unwrap().open, 130);
    assert_eq!(view.repos["acme/app"].rows.len(), 130);
}

/// A walk that cannot finish (page 2 fails) is an error, so the repo is
/// unobserved this pass rather than listed with the first page only.
#[test]
fn an_unfinished_review_walk_is_an_error_not_the_first_page() {
    let dir = tempfile::tempdir().unwrap();
    let repo = format!("test-owner/fleet-review-fail-{}", std::process::id());
    std::fs::write(dir.path().join("rr1.json"), pr_page(1..101)).unwrap();
    std::fs::write(dir.path().join("rr2.json"), pr_page(101..131)).unwrap();
    std::fs::write(dir.path().join("fail2"), "").unwrap();
    let gh = review_stub(dir.path());

    let err = super::review_listing_with(&gh, dir.path(), Some(&repo), "acme/app").unwrap_err();
    assert!(format!("{err:#}").contains("HTTP 502"), "{err:#}");
}

fn row(repo: &str, issue: u32) -> ReadyQueueRow {
    serde_json::from_value(serde_json::json!({
        "rank": issue, "repo": repo, "issue": issue, "workspace_priority": 100,
        "urgent": false, "disposition": "queued"
    }))
    .unwrap()
}

/// A root with a full page's worth of rows in the tick may have been cut at
/// one page; a root with fewer cannot have been.
#[test]
fn a_full_page_of_ready_rows_is_possibly_truncated() {
    let per_page = u32::try_from(crate::forge_listing::PER_PAGE).unwrap();
    let mut queue: Vec<ReadyQueueRow> = (1..=per_page).map(|n| row("/src/full", n)).collect();
    queue.extend((1..per_page).map(|n| row("/src/short", n)));
    let truncated = super::possibly_truncated(&queue);
    assert!(truncated.contains("/src/full"));
    assert!(!truncated.contains("/src/short"));
    assert!(super::possibly_truncated(&[]).is_empty());
}
