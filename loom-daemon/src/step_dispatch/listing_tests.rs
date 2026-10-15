//! Tests for the step engine's all-open listing: mirror first, the shared
//! REST walk otherwise.

use super::*;
use std::cell::Cell;

fn row(number: u32) -> RestIssue {
    crate::forge_listing::parse_rest_issues(&format!(
        r#"[{{"number": {number}, "state": "open", "labels": []}}]"#
    ))
    .unwrap()
    .remove(0)
}

#[test]
fn a_mirror_answer_is_served_and_the_forge_is_not_read() {
    let forge_read = Cell::new(false);
    let items = list_all_open_with(
        None,
        Some("acme/widgets"),
        |_, repo| {
            assert_eq!(repo, Some("acme/widgets"));
            Some(vec![row(1), row(2)])
        },
        |_, _| {
            forge_read.set(true);
            Ok(vec![])
        },
    )
    .unwrap();
    assert_eq!(items.source, ListingSource::Mirror);
    assert_eq!(items.rows.iter().map(|r| r.number).collect::<Vec<_>>(), [1, 2]);
    assert!(!forge_read.get());
}

#[test]
fn no_mirror_answer_reads_the_forge_and_keeps_its_error() {
    let items = list_all_open_with(None, None, |_, _| None, |_, _| Ok(vec![row(3)])).unwrap();
    assert_eq!(items.source, ListingSource::Forge);
    assert_eq!(items.rows[0].number, 3);

    let err = list_all_open_with(
        None,
        None,
        |_, _| None,
        |_, _| Err(anyhow::anyhow!("the unfiltered listing changed mid-walk")),
    )
    .expect_err("an incomplete walk is no listing");
    assert!(err.to_string().contains("mid-walk"), "{err}");
}

/// With no mirror live (the default), the live entry point is the shared
/// ETag walk: the unfiltered, oldest-first REST listing, accounted as
/// `step_engine`.
#[test]
fn the_live_listing_falls_back_to_the_shared_etag_walk() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let gh = d.join("gh");
    std::fs::write(
        &gh,
        format!(
            r#"#!/bin/sh
echo "$*" >> '{log}'
printf 'HTTP/2.0 200 OK\r\nEtag: W/"s1"\r\n\r\n'
printf '[{{"number": 4, "state": "open", "labels": []}}, {{"number": 5, "state": "open", "labels": [], "pull_request": {{}}}}]\n'
"#,
            log = d.join("calls.log").display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let repo = format!("acme/step-listing-{}", std::process::id());
    let items = list_all_open(&gh, Some(d), Some(&repo)).unwrap();
    assert_eq!(items.source, ListingSource::Forge);
    assert_eq!(items.rows.iter().map(|r| r.number).collect::<Vec<_>>(), [4, 5]);
    assert!(items.rows[1].is_pull_request, "pull requests are items too");
    let calls = std::fs::read_to_string(d.join("calls.log")).unwrap();
    assert_eq!(calls.lines().count(), 1, "{calls}");
    assert!(
        calls.contains(&format!("repos/{repo}/issues?state=open&sort=created")),
        "{calls}"
    );
}

/// #12205: a second gather of an unchanged repo revalidates instead of
/// re-reading. Both of the step gather's REST listings (the all-open items
/// and the open PRs) present the ETag their last `200` stored, so an idle
/// repo costs `304`s only.
#[test]
fn a_second_gather_of_an_unchanged_repo_is_all_304s() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let gh = d.join("gh");
    std::fs::write(
        &gh,
        format!(
            r#"#!/bin/sh
case "$*" in *'/pulls?'*) kind=pulls ;; *) kind=issues ;; esac
case "$*" in
  *'If-None-Match: W/"'$kind'1"'*)
    echo "$kind 304" >> '{log}'
    printf 'HTTP/2.0 304 Not Modified\r\n\r\n'
    echo 'gh: Not Modified (HTTP 304)' 1>&2
    exit 1 ;;
esac
echo "$kind 200" >> '{log}'
printf 'HTTP/2.0 200 OK\r\nEtag: W/"%s1"\r\n\r\n' "$kind"
if [ "$kind" = pulls ]; then
  printf '[{{"number": 5, "state": "open", "head": {{"ref": "feature/issue-4", "sha": "abc"}}, "base": {{"ref": "main"}}, "user": {{"login": "a"}}, "labels": []}}]\n'
else
  printf '[{{"number": 4, "state": "open", "labels": []}}, {{"number": 5, "state": "open", "labels": [], "pull_request": {{}}}}]\n'
fi
"#,
            log = d.join("calls.log").display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let repo = format!("acme/step-idle-{}", std::process::id());
    let gather = || {
        let items = list_all_open(&gh, Some(d), Some(&repo)).unwrap();
        let pulls = crate::forge_pull_listing::list_open_pulls_cached_as(
            CALLER,
            &gh,
            Some(d),
            Some(&repo),
            super::super::gather::PULL_PAGES,
        )
        .unwrap();
        (items.rows, pulls)
    };
    let drain = || {
        let log = d.join("calls.log");
        let calls: Vec<String> = std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect();
        let _ = std::fs::remove_file(&log);
        calls
    };
    let (rows, pulls) = gather();
    assert_eq!(drain(), ["issues 200", "pulls 200"]);
    let (again_rows, again_pulls) = gather();
    assert_eq!(drain(), ["issues 304", "pulls 304"], "an idle repo costs 304s only");
    assert_eq!(again_rows, rows);
    assert_eq!(again_pulls, pulls);
}
