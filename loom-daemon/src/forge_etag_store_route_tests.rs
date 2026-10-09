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
/// `404` for `pulls/5` and `200` for everything else, and the writer answers
/// `200` for everything. Logs `<who> <url>` per call.
fn item_stub(dir: &Path) -> (PathBuf, PathBuf) {
    let log = dir.join("item.log");
    let gh = dir.join("gh-item-stub");
    let body = format!(
        r#"#!/bin/sh
who="$(basename "${{GH_CONFIG_DIR:-writer}}")"
[ "$who" = reader-w6 ] || who=writer
echo "$who $3" >> '{log}'
case "$who $3" in
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

/// W6: an item-scoped reader `404` is the item's answer — no writer retry —
/// only when the same reader recently served the same repo's SAME endpoint
/// family. Evidence for `issues` never covers `pulls`, and a read that is not
/// item-scoped keeps the Coverage path.
#[test]
fn an_item_scoped_reader_404_needs_same_family_evidence() {
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
    let status = |site: ConditionalRead, url: &str| {
        let (_, resp, _) =
            fetch_conditional_via(site, &gh, None, &target, url, None, &route).unwrap();
        resp.unwrap().status
    };
    let missing = "repos/acme/w6-item/pulls/5";

    // No evidence: the reader's 404 re-runs on the writer.
    assert_eq!(status(item, missing), 200);
    // Evidence for `issues` only: still the writer.
    assert_eq!(status(item, "repos/acme/w6-item/issues/1"), 200);
    assert_eq!(status(item, missing), 200);
    // Evidence for `pulls`: the 404 is the item's answer, no writer retry.
    assert_eq!(status(item, "repos/acme/w6-item/pulls/6"), 200);
    assert_eq!(status(item, missing), 404);
    assert_eq!(
        crate::forge_call_stats::counters::get(crate::forge_identity::served::ITEM_SCOPED_404),
        1
    );
    // A whole-resource read is never item-scoped: Coverage, writer retry.
    let plain =
        ConditionalRead::new("test_item_scoped", crate::forge_call_stats::ops::PR_VIEW_STATE);
    assert_eq!(status(plain, missing), 200);

    let calls: Vec<String> = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    let m = missing;
    assert_eq!(
        calls,
        vec![
            format!("reader-w6 {m}"),
            format!("writer {m}"),
            "reader-w6 repos/acme/w6-item/issues/1".to_string(),
            format!("reader-w6 {m}"),
            format!("writer {m}"),
            "reader-w6 repos/acme/w6-item/pulls/6".to_string(),
            format!("reader-w6 {m}"),
            format!("reader-w6 {m}"),
            format!("writer {m}"),
        ]
    );
}
