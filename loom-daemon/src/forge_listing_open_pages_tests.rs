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
printf 'HTTP/2.0 200 OK\r\nEtag: W/"%s"\r\n\r\n' "$e"
cat "$d/page$n.json"
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
