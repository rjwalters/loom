//! A deferrable conditional read is shed, never moved to the writer, when
//! its readers are out of budget (W7 B1); any other cause still falls back.

use super::*;
use crate::forge_etag_store::fetch_conditional_via;
use crate::forge_identity::{ExhaustCause, Placement, ReadClass, RouteDecision, RouteRequest};
use std::cell::Cell;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

/// A `gh` stub that logs the basename of the `GH_CONFIG_DIR` it ran under
/// (`writer` for anything that is not one of the test's reader dirs) and
/// answers a rate limit under `reader-limited`, else `200 []`.
fn stub(dir: &Path) -> (PathBuf, PathBuf) {
    let log = dir.join("calls.log");
    let gh = dir.join("gh-deferrable-stub");
    let body = format!(
        r#"#!/bin/sh
who=writer
case "${{GH_CONFIG_DIR:-}}" in
  */reader-*) who=$(basename "$GH_CONFIG_DIR") ;;
esac
echo "$who" >> '{log}'
if [ "$who" = reader-limited ]; then
  printf 'HTTP/2.0 403 Forbidden\r\nX-Ratelimit-Remaining: 0\r\nX-Ratelimit-Resource: core\r\n\r\n'
  printf '{{"message": "API rate limit exceeded"}}\n'
  echo 'gh: API rate limit exceeded for installation (HTTP 403)' 1>&2
  exit 1
fi
printf 'HTTP/2.0 200 OK\r\nEtag: W/"e"\r\n\r\n[]\n'
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

fn target() -> Target {
    Target {
        repo: Some(format!("w7-owner-{}/shed", std::process::id())),
        host: None,
    }
}

const URL: &str = "repos/o/r/issues?state=open&sort=created&direction=asc&per_page=100";

fn site(class: ReadClass) -> ConditionalRead {
    ConditionalRead::new("test.deferrable", crate::forge_call_stats::ops::ISSUE_LIST)
        .deferrable(class)
}

fn budget() -> RouteDecision {
    RouteDecision::Exhausted {
        until: SystemTime::now() + Duration::from_secs(600),
        cause: ExhaustCause::Budget,
    }
}

fn is_shed(r: &Result<FetchResult>) -> bool {
    r.as_ref()
        .err()
        .is_some_and(|e| e.downcast_ref::<ReadShed>().is_some())
}

/// Every reader out of budget up front: no request at all, and the error
/// says shed.
#[test]
fn readers_out_of_budget_up_front_is_a_shed_with_no_request() {
    let tmp = tempfile::tempdir().unwrap();
    let (gh, log) = stub(tmp.path());
    let seen = Cell::new(None);
    let route = |req: &RouteRequest<'_>| {
        seen.set(Some(req.class));
        budget()
    };
    let r =
        fetch_conditional_via(site(ReadClass::Hygiene), &gh, None, &target(), URL, None, &route);
    assert!(is_shed(&r), "{:?}", r.map(|x| x.0));
    assert!(calls(&log).is_empty(), "{:?}", calls(&log));
    assert_eq!(seen.get(), Some(ReadClass::Hygiene), "the router sees the site's class");
}

/// A reader that answers a rate limit is withdrawn, the router is asked
/// again, and with no reader left for budget the read is shed: the writer
/// is never called.
#[test]
fn a_rate_limited_reader_is_shed_not_retried_on_the_writer() {
    let tmp = tempfile::tempdir().unwrap();
    let (gh, log) = stub(tmp.path());
    let reader = tmp.path().join("reader-limited");
    std::fs::create_dir_all(&reader).unwrap();
    let app = format!("w7-test-app-{}", std::process::id());
    let asked = Cell::new(0);
    let route = |_: &RouteRequest<'_>| {
        asked.set(asked.get() + 1);
        if asked.get() == 1 {
            RouteDecision::Reader {
                dir: reader.clone(),
                app_id: app.clone(),
                placement: Placement::Home,
            }
        } else {
            budget()
        }
    };
    let r =
        fetch_conditional_via(site(ReadClass::Hygiene), &gh, None, &target(), URL, None, &route);
    assert!(is_shed(&r));
    assert_eq!(calls(&log), ["reader-limited"], "one reader call, no writer call");
    assert_eq!(asked.get(), 2);

    // A router that keeps offering the same (rate-limited) reader is no
    // reader left, and a rate limit is budget: still shed.
    std::fs::remove_file(&log).unwrap();
    let same = |_: &RouteRequest<'_>| RouteDecision::Reader {
        dir: reader.clone(),
        app_id: app.clone(),
        placement: Placement::Home,
    };
    let r = fetch_conditional_via(site(ReadClass::Hygiene), &gh, None, &target(), URL, None, &same);
    assert!(is_shed(&r));
    assert_eq!(calls(&log), ["reader-limited"]);
}

/// The next reader serves the read when one has headroom.
#[test]
fn a_rate_limited_reader_spills_to_the_next_reader() {
    let tmp = tempfile::tempdir().unwrap();
    let (gh, log) = stub(tmp.path());
    let limited = tmp.path().join("reader-limited");
    let spare = tmp.path().join("reader-spare");
    std::fs::create_dir_all(&limited).unwrap();
    std::fs::create_dir_all(&spare).unwrap();
    let pid = std::process::id();
    let asked = Cell::new(0);
    let route = |_: &RouteRequest<'_>| {
        asked.set(asked.get() + 1);
        let (dir, app_id) = if asked.get() == 1 {
            (limited.clone(), format!("w7-limited-{pid}"))
        } else {
            (spare.clone(), format!("w7-spare-{pid}"))
        };
        RouteDecision::Reader {
            dir,
            app_id,
            placement: Placement::Spill,
        }
    };
    let (_, resp, _) =
        fetch_conditional_via(site(ReadClass::Hygiene), &gh, None, &target(), URL, None, &route)
            .unwrap();
    assert_eq!(resp.unwrap().status, 200);
    assert_eq!(calls(&log), ["reader-limited", "reader-spare"]);
}

/// Out of readers for a reason that is not budget, or no pool at all: the
/// writer serves it, as for a Gate read (shedding there could last forever).
#[test]
fn readers_unavailable_for_another_cause_still_fall_back_to_the_writer() {
    let tmp = tempfile::tempdir().unwrap();
    let (gh, log) = stub(tmp.path());
    let unavailable = |_: &RouteRequest<'_>| RouteDecision::Exhausted {
        until: SystemTime::now(),
        cause: ExhaustCause::Unavailable,
    };
    let no_pool = |_: &RouteRequest<'_>| RouteDecision::NoPool;
    for route in [
        &unavailable as &dyn Fn(&RouteRequest<'_>) -> RouteDecision,
        &no_pool,
    ] {
        let (_, resp, _) =
            fetch_conditional_via(site(ReadClass::Hygiene), &gh, None, &target(), URL, None, route)
                .unwrap();
        assert_eq!(resp.unwrap().status, 200);
    }
    assert_eq!(calls(&log), ["writer", "writer"]);
}

/// The default class is unchanged: a Gate read whose reader is rate limited
/// is retried on the writer.
#[test]
fn a_gate_read_keeps_its_writer_fallback() {
    let tmp = tempfile::tempdir().unwrap();
    let (gh, log) = stub(tmp.path());
    let reader = tmp.path().join("reader-limited");
    std::fs::create_dir_all(&reader).unwrap();
    let app = format!("w7-gate-app-{}", std::process::id());
    let route = |_: &RouteRequest<'_>| RouteDecision::Reader {
        dir: reader.clone(),
        app_id: app.clone(),
        placement: Placement::Home,
    };
    let gate = ConditionalRead::new("test.gate", crate::forge_call_stats::ops::ISSUE_LIST);
    assert_eq!(gate.class, ReadClass::Gate);
    let (_, resp, _) =
        fetch_conditional_via(gate, &gh, None, &target(), URL, None, &route).unwrap();
    assert_eq!(resp.unwrap().status, 200);
    assert_eq!(calls(&log), ["reader-limited", "writer"]);
}
