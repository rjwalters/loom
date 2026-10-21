//! Tests for the step engine's all-open listing: the shared REST walk.

use super::*;

/// The entry point is the shared ETag walk: the unfiltered, oldest-first REST listing, accounted as
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
    assert_eq!(items.iter().map(|r| r.number).collect::<Vec<_>>(), [4, 5]);
    assert!(items[1].is_pull_request, "pull requests are items too");
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
        (items, pulls)
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
