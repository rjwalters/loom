//! Tests for the unfiltered open-issue page walk
//! ([`super::list_open_issues_cached_all_as`]): every page revalidates under
//! its own ETag, so an idle repo costs only `304`s and a change on one page
//! costs one `200`.

use super::*;
use std::path::PathBuf;

/// A `gh` stub serving `page<N>.json` under the ETag in `etag<N>`: a request
/// presenting that page's current ETag gets a `304`, anything else a `200`
/// with it. Every call is logged to `calls.log` as `<page> <status>`.
fn stub(dir: &Path) -> PathBuf {
    let path = dir.join("fake-gh-open-pages.sh");
    std::fs::write(
        &path,
        format!(
            r#"#!/bin/sh
d={dir}
n=1
case "$*" in *'&page='*) n=$(echo "$*" | sed 's/.*&page=\([0-9]*\).*/\1/') ;; esac
e=$(cat "$d/etag$n")
case "$*" in
  *"If-None-Match: W/\"$e\""*)
    echo "$n 304" >> "$d/calls.log"
    printf 'HTTP/2.0 304 Not Modified\r\n\r\n'
    echo 'gh: Not Modified (HTTP 304)' 1>&2
    exit 1 ;;
esac
echo "$n 200" >> "$d/calls.log"
printf 'HTTP/2.0 200 OK\r\nEtag: W/"%s"\r\n' "$e"
[ "$(grep -o '"number"' "$d/page$n.json" | wc -l)" -ge 100 ] && [ ! -f "$d/last$n" ] && printf 'Link: <https://api.github.com/next>; rel="next"\r\n'
printf '\r\n'
cat "$d/page$n.json"
# A scripted change: after page N is served once, its next version lands.
if [ -f "$d/next$n.json" ]; then
  mv "$d/next$n.json" "$d/page$n.json"
  mv "$d/nextetag$n" "$d/etag$n"
fi
"#,
            dir = dir.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

/// Rows `numbers`, with `labelled` carrying `loom:triage`.
fn page(numbers: std::ops::Range<u32>, labelled: &[u32]) -> String {
    let rows: Vec<String> = numbers
        .map(|n| {
            let labels = if labelled.contains(&n) {
                r#"[{"name": "loom:triage"}]"#
            } else {
                "[]"
            };
            format!(r#"{{"number": {n}, "state": "open", "labels": {labels}}}"#)
        })
        .collect();
    format!("[{}]\n", rows.join(","))
}

fn set_page(dir: &Path, n: u32, etag: &str, body: &str) {
    std::fs::write(dir.join(format!("etag{n}")), etag).unwrap();
    std::fs::write(dir.join(format!("page{n}.json")), body).unwrap();
}

/// The calls logged since the last drain.
fn drain_calls(dir: &Path) -> Vec<String> {
    let log = dir.join("calls.log");
    let calls = std::fs::read_to_string(&log)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    let _ = std::fs::remove_file(log);
    calls
}

#[test]
fn the_unfiltered_url_has_no_label_filter_and_sorts_oldest_first() {
    assert_eq!(
        build_issues_url(Some("o/r"), "", "open"),
        "repos/o/r/issues?state=open&sort=created&direction=asc&per_page=100"
    );
    // A labelled listing's URL (and so its cache key) is unchanged.
    assert_eq!(
        build_issues_url(Some("o/r"), "loom:issue", "open"),
        "repos/o/r/issues?labels=loom:issue&state=open&per_page=100"
    );
}

/// The walk the intake pass makes: a first pass pays a `200` per page, an
/// idle pass is `304`s only, and a change confined to page 2 is picked up at
/// the cost of that one page.
#[test]
fn multi_page_revalidation_is_all_304_when_idle_and_picks_up_a_page_two_change() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let repo = format!("test-owner/open-pages-{}", std::process::id());
    set_page(d, 1, "a1", &page(1..101, &[]));
    set_page(d, 2, "b1", &page(101..104, &[]));
    let gh = stub(d);
    let walk = || list_open_issues_cached_all_as("t", &gh, Some(d), Some(&repo)).unwrap();

    // Cold: both pages are read, then page 1 is revalidated (#10401).
    let first = walk();
    assert_eq!(first.len(), 103);
    assert_eq!(drain_calls(d), ["1 200", "2 200", "1 304"]);

    // Idle: every request is a 304, each page under its own validator.
    let idle = walk();
    assert_eq!(idle, first);
    assert_eq!(drain_calls(d), ["1 304", "2 304", "1 304"]);

    // Page 2 changes (an item is labelled, a new one is filed): page 1 stays
    // a 304, page 2 is re-read, and the walk returns the new rows.
    set_page(d, 2, "b2", &page(101..105, &[102]));
    let changed = walk();
    assert_eq!(drain_calls(d), ["1 304", "2 200", "1 304"]);
    assert_eq!(changed.len(), 104);
    let row = |n: u32| changed.iter().find(|i| i.number == n).unwrap();
    assert_eq!(row(102).labels, vec!["loom:triage".to_string()]);
    assert!(row(104).labels.is_empty());

    // And the new page 2 validator is the one presented from then on.
    assert_eq!(walk(), changed);
    assert_eq!(drain_calls(d), ["1 304", "2 304", "1 304"]);
}

/// The unfiltered walk reads past [`MAX_PAGES`], up to its own bound.
#[test]
fn the_unfiltered_walk_is_bounded_by_its_own_page_cap() {
    const { assert!(MAX_OPEN_PAGES > MAX_PAGES) };
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let repo = format!("test-owner/open-pages-cap-{}", std::process::id());
    // Every page is full: the walk never finds the end.
    for n in 1..=MAX_OPEN_PAGES {
        set_page(d, n, &format!("e{n}"), &page(1..101, &[]));
    }
    let gh = stub(d);
    let err = list_open_issues_cached_all_as("t", &gh, Some(d), Some(&repo))
        .expect_err("a listing with no last page is incomplete");
    assert!(err.to_string().contains("incomplete"), "{err}");
    assert_eq!(drain_calls(d).len(), MAX_OPEN_PAGES as usize);
}

/// Page 1 changes to `next` (under ETag `etag`) right after its first read,
/// i.e. between the walk's first read and its revalidation.
fn change_after_first_read(dir: &Path, etag: &str, next: &str) {
    std::fs::write(dir.join("nextetag1"), etag).unwrap();
    std::fs::write(dir.join("next1.json"), next).unwrap();
}

/// Rows `numbers`, each carrying `comments` comments.
fn page_with_comments(numbers: std::ops::Range<u32>, comments: u32) -> String {
    let rows: Vec<String> = numbers
        .map(|n| {
            format!(r#"{{"number": {n}, "state": "open", "labels": [], "comments": {comments}}}"#)
        })
        .collect();
    format!("[{}]\n", rows.join(","))
}

/// The intake walk compares page MEMBERSHIP: a comment on an open issue
/// between the first read and the revalidation does not abort it, and the
/// fresher rows are returned. The strict (row) walk the other callers use
/// still aborts on the same change.
#[test]
fn a_comment_mid_walk_aborts_only_the_strict_walk() {
    for (snapshot, strict) in [(Snapshot::Membership, false), (Snapshot::Rows, true)] {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let repo = format!("test-owner/open-pages-member-{strict}-{}", std::process::id());
        set_page(d, 1, "a1", &page_with_comments(1..101, 0));
        set_page(d, 2, "b1", &page(101..103, &[]));
        change_after_first_read(d, "a2", &page_with_comments(1..101, 1));
        let gh = stub(d);
        let site = issue_list("t");
        let walked =
            walk_pages(site, &gh, Some(d), Some(&repo), ("", "open"), MAX_OPEN_PAGES, snapshot);
        assert_eq!(drain_calls(d), ["1 200", "2 200", "1 200"], "{snapshot:?}");
        if strict {
            let err = walked.expect_err("a row walk aborts on any changed row");
            assert!(err.to_string().contains("changed mid-walk"), "{err}");
        } else {
            let rows = walked.unwrap();
            assert_eq!(rows.len(), 102);
            assert_eq!(rows[0].comments, 1, "the revalidated rows are returned");
        }
    }
}

/// Membership still catches what it exists for: an issue that left page 1
/// mid-walk shifts the page boundary, and the walk aborts.
#[test]
fn a_membership_change_mid_walk_still_aborts() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let repo = format!("test-owner/open-pages-shift-{}", std::process::id());
    set_page(d, 1, "a1", &page(1..101, &[]));
    set_page(d, 2, "b1", &page(101..103, &[]));
    // #1 closes: every later issue moves up one place.
    change_after_first_read(d, "a2", &page(2..102, &[]));
    let gh = stub(d);
    let err = list_open_issues_cached_all_as("t", &gh, Some(d), Some(&repo))
        .expect_err("a shifted page boundary is not a consistent snapshot");
    assert!(err.to_string().contains("changed mid-walk"), "{err}");
}

/// With every reader out of budget the intake walk is shed: not one request
/// (so not one on the writer), and the error is a [`store::ReadShed`].
#[test]
fn the_open_walk_is_shed_when_readers_are_out_of_budget() {
    use crate::forge_identity::{ExhaustCause, ReadClass, RouteDecision};
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let repo = format!("test-owner/open-pages-shed-{}", std::process::id());
    set_page(d, 1, "a1", &page(1..3, &[]));
    let gh = stub(d);
    let classes = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let seen = classes.clone();
    let _route = store::install_test_route(move |req| {
        seen.borrow_mut().push(req.class);
        RouteDecision::Exhausted {
            until: std::time::SystemTime::now() + std::time::Duration::from_secs(600),
            cause: ExhaustCause::Budget,
        }
    });
    let err = list_open_issues_cached_all_as("t", &gh, Some(d), Some(&repo))
        .expect_err("a shed walk is no listing");
    assert!(err.downcast_ref::<store::ReadShed>().is_some(), "{err:#}");
    assert!(drain_calls(d).is_empty(), "no request at all");
    assert_eq!(*classes.borrow(), [ReadClass::Hygiene]);
}
