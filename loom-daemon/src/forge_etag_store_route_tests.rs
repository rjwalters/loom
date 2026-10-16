//! `fetch_conditional` under W4-B routing: a URL served by a reader other
//! than the one whose ETag the caller holds (a spill, or a split rolling
//! out) costs one 200, never a stale 304.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::forge_identity::{Placement, RouteDecision, RouteRequest};
use std::cell::RefCell;
use std::os::unix::fs::PermissionsExt;

/// A `gh` stub whose ETag is per credential: `"<basename of
/// GH_CONFIG_DIR>"`. It answers 304 only when the request's If-None-Match
/// is exactly its own ETag, else 200 with its own ETag, and logs every
/// If-None-Match it was sent.
fn stub(dir: &Path) -> (PathBuf, PathBuf) {
    let log = dir.join("sent.log");
    let gh = dir.join("gh-etag-stub");
    let body = format!(
        r#"#!/bin/sh
own="\"$(basename "${{GH_CONFIG_DIR:-writer}}")\""
sent=""
prev=""
for a in "$@"; do
  if [ "$prev" = "-H" ]; then sent="${{a#If-None-Match: }}"; fi
  prev="$a"
done
echo "$sent" >> '{log}'
if [ -n "$sent" ] && [ "$sent" = "$own" ]; then
  printf 'HTTP/2.0 304 Not Modified\r\nEtag: %s\r\n\r\n' "$own"
else
  printf 'HTTP/2.0 200 OK\r\nEtag: %s\r\n\r\n' "$own"
  printf '[]\n'
fi
"#,
        log = log.display()
    );
    std::fs::write(&gh, body).unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    (gh, log)
}

#[test]
fn a_spilled_route_sends_the_callers_etag_and_a_mismatch_is_a_fresh_200() {
    let tmp = tempfile::tempdir().unwrap();
    let (gh, log) = stub(tmp.path());
    let home = tmp.path().join("reader-a");
    let spill = tmp.path().join("reader-b");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&spill).unwrap();
    let target = Target {
        repo: Some("acme/hot".to_string()),
        host: None,
    };
    let url = "repos/acme/hot/issues?state=open";
    let site = ConditionalRead::new("test_spill_etag", crate::forge_call_stats::ops::ISSUE_LIST);
    let seen_keys = RefCell::new(Vec::new());
    let route_to = |dir: &Path| {
        let dir = dir.to_path_buf();
        let seen = &seen_keys;
        move |req: &RouteRequest<'_>| {
            seen.borrow_mut().push(req.affinity_key.map(str::to_string));
            RouteDecision::Reader {
                dir: dir.clone(),
                app_id: "1".to_string(),
                placement: Placement::Spill,
            }
        }
    };

    // The caller holds home's ETag; the read is served by the spill reader.
    let held = "\"reader-a\"";
    let (_, resp, _) =
        fetch_conditional_via(site, &gh, None, &target, url, Some(held), &route_to(&spill))
            .unwrap();
    let resp = resp.unwrap();
    assert_eq!(resp.status, 200, "a validator from another credential never 304s");
    let fresh = resp.etag.clone().unwrap();
    assert_eq!(fresh, "\"reader-b\"", "the caller's entry updates to the spill reader's ETag");
    // The new ETag then validates on the spill reader...
    let (_, resp, _) =
        fetch_conditional_via(site, &gh, None, &target, url, Some(&fresh), &route_to(&spill))
            .unwrap();
    assert_eq!(resp.unwrap().status, 304);
    // ...and back home after release it is one more 200, not a stale 304.
    let (_, resp, _) =
        fetch_conditional_via(site, &gh, None, &target, url, Some(&fresh), &route_to(&home))
            .unwrap();
    assert_eq!(resp.unwrap().status, 200);

    let sent: Vec<String> = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(
        sent,
        vec![held.to_string(), fresh.clone(), fresh],
        "the caller's ETag is sent as-is"
    );
    // The router was asked with the URL as the affinity key.
    assert!(seen_keys
        .into_inner()
        .iter()
        .all(|k| k.as_deref() == Some("repos/acme/hot/issues?state=open")));
}

/// A stub where the reader (`GH_CONFIG_DIR` basename `reader-w6`) answers
/// `404` for `pulls/5` and `200` for everything else, the reader
/// `reader-w6-blind` answers `403 Resource not accessible by integration` for
/// everything (an installation that does not cover the repo), and the writer
/// answers `200` for everything. Logs `<who> <url>` per call.
fn item_stub(dir: &Path) -> (PathBuf, PathBuf) {
    let log = dir.join("item.log");
    let gh = dir.join("gh-item-stub");
    let body = format!(
        r#"#!/bin/sh
who="$(basename "${{GH_CONFIG_DIR:-writer}}")"
case "$who" in reader-w6|reader-w6-blind) ;; *) who=writer ;; esac
echo "$who $3" >> '{log}'
case "$who $3" in
  "reader-w6-blind "*)
    printf 'HTTP/2.0 403 Forbidden\r\n\r\n{{"message":"Resource not accessible by integration"}}'; exit 1 ;;
  "reader-w6 repos/acme/w6-item/pulls/5")
    printf 'HTTP/2.0 404 Not Found\r\n\r\n{{"message":"Not Found"}}'; exit 1 ;;
esac
printf 'HTTP/2.0 200 OK\r\nEtag: "e"\r\n\r\n{{}}'
"#,
        log = log.display()
    );
    std::fs::write(&gh, body).unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    (gh, log)
}

fn calls(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

/// A reader `404` is the item's own answer only for an item-scoped read, and
/// only once that reader has recently served the same repo and endpoint
/// family (W6): otherwise it may be a reader that cannot see the repo, so it
/// is withdrawn and, with no reader left, the read sheds. The writer is never
/// asked (#12394).
#[test]
fn a_reader_404_is_the_answer_only_for_a_covered_item() {
    let tmp = tempfile::tempdir().unwrap();
    let (gh, log) = item_stub(tmp.path());
    let reader = tmp.path().join("reader-w6");
    std::fs::create_dir_all(&reader).unwrap();
    let target = Target {
        repo: Some("acme/w6-item".to_string()),
        host: None,
    };
    let route = |_: &RouteRequest<'_>| RouteDecision::Reader {
        dir: reader.clone(),
        app_id: "w6-item-404".to_string(),
        placement: Placement::Home,
    };
    let item =
        ConditionalRead::new("test_item_scoped", crate::forge_call_stats::ops::PR_VIEW_STATE)
            .item_scoped();
    let plain =
        ConditionalRead::new("test_item_scoped", crate::forge_call_stats::ops::PR_VIEW_STATE);
    let fetch = |site: ConditionalRead, url: &str| {
        fetch_conditional_via(site, &gh, None, &target, url, None, &route)
    };
    let missing = "repos/acme/w6-item/pulls/5";
    let shed = |r: anyhow::Result<_>| {
        r.err()
            .is_some_and(|e| e.downcast_ref::<ReadShed>().is_some())
    };

    // No evidence yet: not an answer.
    assert!(shed(fetch(item, missing)));
    // Evidence for another family does not cover `pulls`.
    assert_eq!(
        fetch(item, "repos/acme/w6-item/issues/1")
            .unwrap()
            .1
            .unwrap()
            .status,
        200
    );
    assert!(shed(fetch(item, missing)));
    // Evidence for this family: the item's own 404.
    assert_eq!(
        fetch(item, "repos/acme/w6-item/pulls/6")
            .unwrap()
            .1
            .unwrap()
            .status,
        200
    );
    assert_eq!(fetch(item, missing).unwrap().1.unwrap().status, 404);
    // A read that is not item-scoped never takes a 404 as the answer.
    assert!(shed(fetch(plain, missing)));

    assert!(
        calls(&log).iter().all(|c| c.starts_with("reader-w6 ")),
        "writer stub NOT invoked: {:?}",
        calls(&log)
    );
}

/// (a) The first reader answers `403 Resource not accessible by integration`:
/// the next reader serves the read. (b) The only reader does: the read sheds,
/// the writer is not called, and the caller never sees a 403/404.
#[test]
fn a_blind_reader_is_skipped_and_with_none_left_the_read_sheds() {
    let tmp = tempfile::tempdir().unwrap();
    let (gh, log) = item_stub(tmp.path());
    let blind = tmp.path().join("reader-w6-blind");
    let good = tmp.path().join("reader-w6");
    std::fs::create_dir_all(&blind).unwrap();
    std::fs::create_dir_all(&good).unwrap();
    let target = Target {
        repo: Some("acme/w6-item".to_string()),
        host: None,
    };
    let site =
        ConditionalRead::new("test_blind_reader", crate::forge_call_stats::ops::PR_VIEW_STATE);
    let url = "repos/acme/w6-item/issues/1";

    let tried = RefCell::new(Vec::<String>::new());
    let two = |_: &RouteRequest<'_>| {
        let (dir, app) = if tried.borrow().is_empty() {
            (blind.clone(), "w6-blind-a")
        } else {
            (good.clone(), "w6-good-a")
        };
        tried.borrow_mut().push(app.to_string());
        RouteDecision::Reader {
            dir,
            app_id: app.to_string(),
            placement: Placement::Home,
        }
    };
    let served = fetch_conditional_via(site, &gh, None, &target, url, None, &two).unwrap();
    assert_eq!(served.1.unwrap().status, 200);
    assert_eq!(calls(&log), vec![format!("reader-w6-blind {url}"), format!("reader-w6 {url}")]);

    std::fs::remove_file(&log).unwrap();
    let only = |_: &RouteRequest<'_>| RouteDecision::Reader {
        dir: blind.clone(),
        app_id: "w6-blind-b".to_string(),
        placement: Placement::Home,
    };
    let err = fetch_conditional_via(site, &gh, None, &target, url, None, &only).unwrap_err();
    assert!(err.downcast_ref::<ReadShed>().is_some(), "{err:#}");
    assert_eq!(calls(&log), vec![format!("reader-w6-blind {url}")], "writer NOT invoked");
}
