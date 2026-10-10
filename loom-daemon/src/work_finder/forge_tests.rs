//! `GhWorkSource`'s listing merge (#9244 §4, #10118) against a scripted fake `gh`.

use super::*;
use std::os::unix::fs::PermissionsExt;

const ISSUES: &str = r#"[
  {"number":1,"labels":[{"name":"loom:issue"},{"name":"loom:operator-priority"}],"created_at":"2026-01-01T00:00:00Z"},
  {"number":2,"labels":[{"name":"loom:issue"}],"created_at":"2026-01-02T00:00:00Z"}
]"#;

const STARRED: &str = r#"[
  {"number":1,"labels":[{"name":"loom:issue"},{"name":"loom:operator-priority"}]},
  {"number":3,"labels":[{"name":"loom:triage"},{"name":"loom:operator-priority"}]},
  {"number":4,"labels":[{"name":"loom:operator-priority"}],"pull_request":{}},
  {"number":5,"labels":[{"name":"loom:building"},{"name":"loom:operator-priority"}]}
]"#;

/// Pre-promotion rows (#10118): only marker-bearing, unclaimed ones filed by
/// a trusted author (#9548) are kept. #10 is an outsider's marker.
const TRIAGE: &str = r#"[
  {"number":3,"labels":[{"name":"loom:triage"},{"name":"loom:operator-priority"}],"body":"<!-- loom:main-red-fix -->","user":{"login":"rjwalters"},"author_association":"OWNER"},
  {"number":6,"labels":[{"name":"loom:triage"}],"body":"Fix CI.\n<!-- loom:main-red-fix -->\n","user":{"login":"rjwalters"},"author_association":"OWNER"},
  {"number":7,"labels":[{"name":"loom:triage"}],"body":"No marker here.","user":{"login":"rjwalters"},"author_association":"OWNER"},
  {"number":8,"labels":[{"name":"loom:triage"},{"name":"loom:building"}],"body":"<!-- loom:main-red-fix -->","user":{"login":"rjwalters"},"author_association":"OWNER"},
  {"number":10,"labels":[{"name":"loom:triage"}],"body":"<!-- loom:main-red-fix -->","user":{"login":"mallory","type":"User"},"author_association":"NONE"}
]"#;

const CURATED: &str = r#"[
  {"number":6,"labels":[{"name":"loom:curated"}],"body":"<!-- loom:main-red-fix -->","user":{"login":"rjwalters"},"author_association":"OWNER"},
  {"number":9,"labels":[{"name":"loom:curated"}],"body":"<!-- loom:main-red-fix -->","user":{"login":"teammate"},"author_association":"MEMBER"}
]"#;

/// A fake `gh` answering the four listings and the timeline read, logging
/// every timeline call. The starred listing fails while `fail-starred` exists.
fn fake_gh(dir: &Path) -> PathBuf {
    let d = dir.display();
    std::fs::write(dir.join("issues.json"), ISSUES).unwrap();
    std::fs::write(dir.join("starred.json"), STARRED).unwrap();
    std::fs::write(dir.join("triage.json"), TRIAGE).unwrap();
    std::fs::write(dir.join("curated.json"), CURATED).unwrap();
    let script = format!(
        r#"#!/bin/bash
case "$*" in
  *labels=loom:issue\&*) printf 'HTTP/2.0 200 OK\r\n\r\n'; cat {d}/issues.json ;;
  *labels=loom:operator-priority\&*)
    if [ -f {d}/fail-starred ]; then echo 'HTTP 502' >&2; exit 1; fi
    printf 'HTTP/2.0 200 OK\r\n\r\n'; cat {d}/starred.json ;;
  *labels=loom:triage\&*)
    if [ -f {d}/fail-triage ]; then echo 'HTTP 502' >&2; exit 1; fi
    printf 'HTTP/2.0 200 OK\r\n\r\n'; cat {d}/triage.json ;;
  *labels=loom:curated\&*) printf 'HTTP/2.0 200 OK\r\n\r\n'; cat {d}/curated.json ;;
  *timeline*) echo "$*" >> {d}/timeline.log; echo '2026-09-01T00:00:00Z' ;;
  *labels=*) printf 'HTTP/2.0 200 OK\r\n\r\n'; echo '[]' ;;
  *) exit 1 ;;
esac
"#
    );
    let path = dir.join("gh");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn source(dir: &Path) -> GhWorkSource {
    GhWorkSource {
        gh_bin: fake_gh(dir),
        repo: Some("acme/app".into()),
        cwd: Some(dir.to_path_buf()),
        complete: true,
    }
}

fn timeline_calls(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("timeline.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn starred_rows_are_merged_deduped_and_stamped_with_starred_at() {
    let dir = tempfile::tempdir().unwrap();
    let mut src = source(dir.path());
    let items = src.list_ready_issues().unwrap();
    let numbers: Vec<u32> = items.iter().map(|i| i.number).collect();
    // #1 deduped, #4 (a PR) and #5 (already claimed) dropped. Then the
    // unpromoted red-main fixes (#10118): #3 already listed as starred, #7
    // has no marker, #8 is claimed, #6 is listed once, and #10's marker is
    // an outsider's (#9548): not admitted.
    assert_eq!(numbers, vec![1, 2, 3, 6, 9]);
    let owner = items[3]
        .author
        .as_ref()
        .expect("the listing carries the author");
    assert_eq!(
        (owner.login.as_deref(), owner.association.as_deref()),
        (Some("rjwalters"), Some("OWNER"))
    );
    assert!(items[3].is_unpromoted_red_fix() && items[4].is_unpromoted_red_fix());
    assert!(!items[2].is_unpromoted_red_fix(), "a starred fix is a candidate anyway");
    assert_eq!(items[0].operator_priority_at.as_deref(), Some("2026-09-01T00:00:00Z"));
    assert_eq!(items[1].operator_priority_at, None, "unstarred");
    assert_eq!(items[2].operator_priority_at.as_deref(), Some("2026-09-01T00:00:00Z"));
    let calls = timeline_calls(dir.path());
    assert_eq!(calls.len(), 2, "one timeline read per starred issue: {calls:?}");
    assert!(calls.iter().all(|c| c.contains("repos/acme/app/issues/")));

    // The next tick reads no timeline at all: starred-at is cached.
    src.list_ready_issues().unwrap();
    assert_eq!(timeline_calls(dir.path()).len(), 2);
}

#[test]
fn a_failed_starred_listing_keeps_the_loom_issue_rows() {
    let dir = tempfile::tempdir().unwrap();
    let mut src = source(dir.path());
    std::fs::write(dir.path().join("fail-starred"), "").unwrap();
    let items = src.list_ready_issues().expect("loom:issue rows still used");
    let numbers: Vec<u32> = items.iter().map(|i| i.number).collect();
    assert_eq!(numbers, vec![1, 2, 3, 6, 9]);
}

#[test]
fn a_failed_unpromoted_listing_keeps_every_other_row() {
    let dir = tempfile::tempdir().unwrap();
    let mut src = source(dir.path());
    std::fs::write(dir.path().join("fail-triage"), "").unwrap();
    let items = src.list_ready_issues().expect("other listings still used");
    let numbers: Vec<u32> = items.iter().map(|i| i.number).collect();
    assert_eq!(numbers, vec![1, 2, 3, 6, 9], "#6 still arrives via loom:curated");
}

// ===== paged ready queue (#11139) =====

/// `n` ready rows numbered `from` down to `from - n + 1`, newest first (the
/// REST default sort): issue `k` was created `k` minutes into the year.
fn ready_page(from: u32, n: u32) -> String {
    let rows: Vec<String> = (0..n)
        .map(|i| {
            let k = from - i;
            let at = format!("2026-01-01T{:02}:{:02}:00Z", k / 60, k % 60);
            format!(
                r#"{{"number":{k},"labels":[{{"name":"loom:issue"}}],"created_at":"{at}","updated_at":"{at}"}}"#
            )
        })
        .collect();
    format!("[{}]", rows.join(","))
}

/// A fake `gh` serving the `loom:issue` listing page by page from
/// `ready-p<N>.json` (page 1 is the URL without `&page=`), logging each of
/// those requests to `ready.log`. A missing page file is an HTTP 502. Every
/// other listing is empty and every timeline read answers nothing.
fn paged_gh(dir: &Path) -> PathBuf {
    let d = dir.display();
    let script = format!(
        r#"#!/bin/bash
case "$*" in
  *labels=loom:issue\&*)
    echo "$*" >> {d}/ready.log
    n=1
    case "$*" in *'&page='*) n=$(echo "$*" | sed 's/.*&page=\([0-9]*\).*/\1/') ;; esac
    if [ -f {d}/ready-all.json ]; then f={d}/ready-all.json; else f={d}/ready-p$n.json; fi
    if [ ! -f "$f" ]; then echo 'gh: Server Error (HTTP 502)' >&2; exit 1; fi
    printf 'HTTP/2.0 200 OK\r\n'
    [ "$(grep -o '"number"' "$f" | wc -l)" -ge 100 ] && [ ! -f {d}/last$n ] && printf 'Link: <https://api.github.com/next>; rel="next"\r\n'
    printf '\r\n'; cat "$f" ;;
  *labels=*) printf 'HTTP/2.0 200 OK\r\n\r\n'; echo '[]' ;;
  *) exit 1 ;;
esac
"#
    );
    let path = dir.join("gh");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn paged_source(dir: &Path, repo: &str) -> GhWorkSource {
    GhWorkSource {
        gh_bin: paged_gh(dir),
        repo: Some(repo.into()),
        cwd: Some(dir.to_path_buf()),
        complete: true,
    }
}

fn ready_requests(dir: &Path) -> usize {
    std::fs::read_to_string(dir.join("ready.log"))
        .unwrap_or_default()
        .lines()
        .count()
}

/// The multi-workspace planner's order over `items`: [`candidate_cmp`]
/// (createdAt oldest first, then number, for unstarred work).
fn planner_order(items: &[WorkItem]) -> Vec<u32> {
    let mut keys: Vec<_> = items
        .iter()
        .map(|i| super::super::ready_queue::key_of(0, 100, i, false))
        .collect();
    keys.sort_by(super::super::candidate_cmp);
    keys.iter().map(|k| k.number).collect()
}

/// 250 ready issues over three pages: every one reaches the tick, the
/// listing marks the queue complete, and the planner ranks the oldest (#1,
/// on the last page, which a one-page read never saw) first.
#[test]
fn a_repo_with_more_than_a_page_of_ready_issues_yields_every_one() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("ready-p1.json"), ready_page(250, 100)).unwrap();
    std::fs::write(dir.path().join("ready-p2.json"), ready_page(150, 100)).unwrap();
    std::fs::write(dir.path().join("ready-p3.json"), ready_page(50, 50)).unwrap();
    let repo = format!("acme/paged-250-{}", std::process::id());
    let mut src = paged_source(dir.path(), &repo);
    let items = src.list_ready_issues().unwrap();
    let numbers: Vec<u32> = items.iter().map(|i| i.number).collect();
    assert_eq!(numbers, (1..=250).rev().collect::<Vec<_>>(), "listing order kept");
    assert!(src.listing_complete());
    assert_eq!(planner_order(&items), (1..=250).collect::<Vec<_>>());
    // Single-workspace order: unstarred work keeps its listing order.
    let mut lanes = items.clone();
    super::super::ready_queue::sort_lanes(&mut lanes, false);
    assert_eq!(lanes.iter().map(|i| i.number).collect::<Vec<_>>(), numbers);
    // Three pages, then the conditional re-reads of pages 1 and 2.
    assert_eq!(ready_requests(dir.path()), 5);
}

/// A repo with at most one page of ready work makes exactly one request for
/// it, as before #11139, and its rows arrive in the same order.
#[test]
fn a_single_page_ready_queue_makes_exactly_one_request() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("ready-p1.json"), ready_page(100, 99)).unwrap();
    let repo = format!("acme/paged-single-{}", std::process::id());
    let mut src = paged_source(dir.path(), &repo);
    let items = src.list_ready_issues().unwrap();
    let numbers: Vec<u32> = items.iter().map(|i| i.number).collect();
    assert_eq!(numbers, (2..=100).rev().collect::<Vec<_>>());
    assert!(src.listing_complete());
    assert_eq!(ready_requests(dir.path()), 1);
}

/// Exactly one full page (100 raw rows) whose `Link` names no next page is
/// the whole queue in one request, as before #11139.
#[test]
fn exactly_one_full_page_without_a_next_link_is_one_request() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("ready-p1.json"), ready_page(100, 100)).unwrap();
    std::fs::write(dir.path().join("last1"), "").unwrap();
    let repo = format!("acme/paged-100-{}", std::process::id());
    let mut src = paged_source(dir.path(), &repo);
    let items = src.list_ready_issues().unwrap();
    let numbers: Vec<u32> = items.iter().map(|i| i.number).collect();
    assert_eq!(numbers, (1..=100).rev().collect::<Vec<_>>());
    assert!(src.listing_complete());
    assert_eq!(ready_requests(dir.path()), 1);
}

/// A later page failing keeps the rows read and marks the queue incomplete;
/// it is never a silent partial list. The next whole read clears the mark.
#[test]
fn a_later_page_failing_marks_the_queue_incomplete_never_silently_partial() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("ready-p1.json"), ready_page(250, 100)).unwrap();
    let repo = format!("acme/paged-fail-{}", std::process::id());
    let mut src = paged_source(dir.path(), &repo);
    let items = src.list_ready_issues().expect("page 1 read: rows are kept");
    assert_eq!(items.len(), 100);
    assert!(!src.listing_complete(), "page 2 failed: the queue is partial");

    std::fs::write(dir.path().join("ready-p2.json"), ready_page(150, 20)).unwrap();
    assert_eq!(src.list_ready_issues().unwrap().len(), 120);
    assert!(src.listing_complete());

    // Page 1 failing is an error: there is nothing to hand back.
    std::fs::remove_file(dir.path().join("ready-p1.json")).unwrap();
    assert!(src.list_ready_issues().is_err());
}

/// Every page full up to the cap: the rows read are kept, the queue is
/// marked incomplete.
#[test]
fn hitting_the_page_cap_marks_the_queue_incomplete() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("ready-all.json"), ready_page(100, 100)).unwrap();
    let repo = format!("acme/paged-cap-{}", std::process::id());
    let mut src = paged_source(dir.path(), &repo);
    let items = src.list_ready_issues().unwrap();
    // Every page served the same rows; each item is listed once.
    assert_eq!(items.len(), 100);
    assert!(!src.listing_complete());
    let cap = crate::forge_listing::MAX_PAGES as usize;
    assert_eq!(ready_requests(dir.path()), cap);
}

/// A failed side listing (the starred one) also leaves the queue partial:
/// its rows are missing from the tick.
#[test]
fn a_failed_side_listing_marks_the_queue_incomplete() {
    let dir = tempfile::tempdir().unwrap();
    let mut src = source(dir.path());
    src.list_ready_issues().unwrap();
    assert!(src.listing_complete());
    std::fs::write(dir.path().join("fail-starred"), "").unwrap();
    src.list_ready_issues().unwrap();
    assert!(!src.listing_complete());
}
