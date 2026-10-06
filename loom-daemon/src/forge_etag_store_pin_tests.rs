//! [`cached_read_via`] under a [`ReadPin`] (W9): a writer pin never rides a
//! reader App, an unconditional pin sends no `If-None-Match`, and a `304`
//! serves the stored body.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::forge_identity::{Placement, RouteDecision, RouteRequest};
use std::os::unix::fs::PermissionsExt;

/// A `gh` stub whose ETag is `"<GH_CONFIG_DIR basename>-<version>"`, the
/// version read from `version` beside it. It answers `304` only to its own
/// current ETag and logs `dir=<basename> sent=<If-None-Match>` per call.
fn stub(dir: &Path) -> (PathBuf, PathBuf) {
    let log = dir.join("calls.log");
    let gh = dir.join("gh-pin-stub");
    std::fs::write(dir.join("version"), "1").unwrap();
    let body = format!(
        r#"#!/bin/sh
who="$(basename "${{GH_CONFIG_DIR:-writer}}")"
own="\"$who-$(cat '{version}')\""
sent=""
prev=""
for a in "$@"; do
  if [ "$prev" = "-H" ]; then sent="${{a#If-None-Match: }}"; fi
  prev="$a"
done
echo "dir=$who sent=$sent" >> '{log}'
if [ -n "$sent" ] && [ "$sent" = "$own" ]; then
  printf 'HTTP/2.0 304 Not Modified\r\nEtag: %s\r\n\r\n' "$own"
else
  printf 'HTTP/2.0 200 OK\r\nEtag: %s\r\n\r\n' "$own"
  printf '{{"v":"%s"}}\n' "$(cat '{version}')"
fi
"#,
        log = log.display(),
        version = dir.join("version").display(),
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

fn reader(dir: &Path) -> impl Fn(&RouteRequest<'_>) -> RouteDecision {
    let dir = dir.to_path_buf();
    move |_: &RouteRequest<'_>| RouteDecision::Reader {
        dir: dir.clone(),
        app_id: "1".to_string(),
        placement: Placement::Home,
    }
}

const SITE: ConditionalRead =
    ConditionalRead::new("test_pin_read", crate::forge_call_stats::ops::ISSUE_VIEW_STATE);

#[test]
fn an_unpinned_read_rides_the_reader_and_a_304_serves_the_stored_body() {
    let tmp = tempfile::tempdir().unwrap();
    let (gh, log) = stub(tmp.path());
    let route = reader(&tmp.path().join("reader-a"));
    std::fs::create_dir_all(tmp.path().join("reader-a")).unwrap();
    let at = (Some("acme/pin-unpinned"), "repos/acme/pin-unpinned/issues/1", "test-pin-a-");

    let first = cached_read_via(SITE, &gh, None, at, ReadPin::default(), &route).unwrap();
    assert!(!first.not_modified);
    let second = cached_read_via(SITE, &gh, None, at, ReadPin::default(), &route).unwrap();
    assert!(second.not_modified, "the stored ETag is sent and answered 304");
    assert_eq!(second.body, first.body, "a 304 serves the stored body");
    assert_eq!(
        calls(&log),
        vec![
            "dir=reader-a sent=".to_string(),
            "dir=reader-a sent=\"reader-a-1\"".to_string()
        ]
    );
}

#[test]
fn a_writer_pin_never_consults_the_reader_route() {
    let tmp = tempfile::tempdir().unwrap();
    let (gh, log) = stub(tmp.path());
    std::fs::create_dir_all(tmp.path().join("reader-b")).unwrap();
    let route = reader(&tmp.path().join("reader-b"));
    let at = (Some("acme/pin-writer"), "repos/acme/pin-writer/issues/2", "test-pin-b-");
    let pin = ReadPin {
        writer: true,
        unconditional: false,
    };
    let first = cached_read_via(SITE, &gh, None, at, pin, &route).unwrap();
    let second = cached_read_via(SITE, &gh, None, at, pin, &route).unwrap();
    assert!(second.not_modified, "the writer's own ETag revalidates");
    assert_eq!(second.body, first.body);
    let seen = calls(&log);
    assert_eq!(seen.len(), 2);
    assert!(
        seen.iter().all(|c| !c.starts_with("dir=reader-b ")),
        "a writer-pinned read went to the reader: {seen:?}"
    );
}

#[test]
fn an_unconditional_pin_sends_no_etag_and_refreshes_the_entry() {
    let tmp = tempfile::tempdir().unwrap();
    let (gh, log) = stub(tmp.path());
    let no_pool = |_: &RouteRequest<'_>| RouteDecision::NoPool;
    let at = (Some("acme/pin-fresh"), "repos/acme/pin-fresh/issues/3", "test-pin-c-");
    let fresh = ReadPin {
        writer: true,
        unconditional: true,
    };
    cached_read_via(SITE, &gh, None, at, ReadPin::default(), &no_pool).unwrap();
    // The object changes; an unconditional read must see it.
    std::fs::write(tmp.path().join("version"), "2").unwrap();
    let after = cached_read_via(SITE, &gh, None, at, fresh, &no_pool).unwrap();
    assert!(!after.not_modified);
    assert_eq!(after.body.as_deref().map(str::trim), Some(r#"{"v":"2"}"#));
    // The next conditional read revalidates against the refreshed entry.
    let next = cached_read_via(SITE, &gh, None, at, ReadPin::default(), &no_pool).unwrap();
    assert!(next.not_modified);
    assert_eq!(next.body, after.body);
    let seen = calls(&log);
    assert_eq!(seen.len(), 3);
    assert!(seen[1].ends_with(" sent="), "the pinned read sent an ETag: {seen:?}");
    assert!(seen[2].ends_with("-2\""), "the entry was not refreshed: {seen:?}");
}
