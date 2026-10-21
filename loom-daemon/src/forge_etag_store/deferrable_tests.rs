//! A conditional read is shed, never moved to the writer, when no reader can
//! serve it (W7 B1, #12394): every class, every cause.

use super::*;
use crate::forge_etag_store::fetch_conditional_via;
use crate::forge_identity::{ExhaustCause, Placement, ReadClass, RouteDecision, RouteRequest};
use std::cell::Cell;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

/// A `gh` stub that logs the basename of the `GH_CONFIG_DIR` it ran under
/// (`writer` for anything that is not one of the test's reader dirs) and
/// answers a rate limit under `reader-limited`, a 403 "not accessible by
/// integration" under `reader-blind`, a 404 under `reader-gone`, else
/// `200 []`.
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
if [ "$who" = reader-blind ]; then
  printf 'HTTP/2.0 403 Forbidden\r\n\r\n'
  printf '{{"message": "Resource not accessible by integration"}}\n'
  echo 'gh: Resource not accessible by integration (HTTP 403)' 1>&2
  exit 1
fi
if [ "$who" = reader-gone ]; then
  printf 'HTTP/2.0 404 Not Found\r\n\r\n{{"message": "Not Found"}}\n'
  echo 'gh: Not Found (HTTP 404)' 1>&2
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

/// Out of readers for any cause, a Gate read included, is a shed: the writer
/// never serves it (#12394). Only no pool at all reads on the writer.
#[test]
fn readers_unavailable_for_any_cause_are_shed_and_only_no_pool_reads_on_the_writer() {
    let tmp = tempfile::tempdir().unwrap();
    let (gh, log) = stub(tmp.path());
    let unavailable = |_: &RouteRequest<'_>| RouteDecision::Exhausted {
        until: SystemTime::now(),
        cause: ExhaustCause::Unavailable,
    };
    for class in [
        ReadClass::Gate,
        ReadClass::Hygiene,
        ReadClass::Observability,
    ] {
        let r = fetch_conditional_via(site(class), &gh, None, &target(), URL, None, &unavailable);
        assert!(is_shed(&r), "{class:?}");
    }
    assert!(calls(&log).is_empty(), "{:?}", calls(&log));
    let no_pool = |_: &RouteRequest<'_>| RouteDecision::NoPool;
    let (_, resp, _) =
        fetch_conditional_via(site(ReadClass::Hygiene), &gh, None, &target(), URL, None, &no_pool)
            .unwrap();
    assert_eq!(resp.unwrap().status, 200);
    assert_eq!(calls(&log), ["writer"]);
}

/// The default class is shed too: a Gate read whose reader is rate limited
/// is not retried on the writer (#12394).
#[test]
fn a_gate_read_is_shed_not_retried_on_the_writer() {
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
    let r = fetch_conditional_via(gate, &gh, None, &target(), URL, None, &route);
    assert!(is_shed(&r));
    assert_eq!(calls(&log), ["reader-limited"]);
}

/// #12074: past the tick deadline a listing page is not sent at all; the
/// error is a `TickDeadlineReached` (not a `ReadShed`: no reader budget is
/// involved, #12641), which the step gather answers with its last good listing.
#[test]
fn a_listing_past_the_tick_deadline_is_shed_with_no_request() {
    let tmp = tempfile::tempdir().unwrap();
    let (gh, log) = stub(tmp.path());
    let past = std::time::Instant::now()
        .checked_sub(Duration::from_secs(1))
        .unwrap();
    let _scope = crate::gh_invocation::read_deadline::enter(past);
    let no_pool = |_: &RouteRequest<'_>| RouteDecision::NoPool;
    let r =
        fetch_conditional_via(site(ReadClass::Hygiene), &gh, None, &target(), URL, None, &no_pool);
    let err = r.map(|x| x.0).unwrap_err();
    assert!(err.downcast_ref::<TickDeadlineReached>().is_some(), "{err:#}");
    assert!(err.downcast_ref::<ReadShed>().is_none(), "{err:#}");
    assert!(calls(&log).is_empty(), "{:?}", calls(&log));
}

/// A conditional read is shed for every cause (a mix of budget and
/// non-budget withdrawals, a withdrawn gateway with no reader pool behind it,
/// an unavailable pool), and the writer is never run.
#[test]
fn withdrawn_or_unavailable_readers_never_send_a_read_to_the_writer() {
    let tmp = tempfile::tempdir().unwrap();
    let (gh, log) = stub(tmp.path());
    let until = SystemTime::now() + Duration::from_secs(600);
    let withdrawn = |_: &RouteRequest<'_>| RouteDecision::Exhausted {
        until,
        cause: ExhaustCause::Withdrawn,
    };
    let gateway_only = |_: &RouteRequest<'_>| {
        crate::forge_gateway::past_withdrawn_gateway(RouteDecision::NoPool, Some(until))
    };
    let unavailable = |_: &RouteRequest<'_>| RouteDecision::Exhausted {
        until,
        cause: ExhaustCause::Unavailable,
    };
    for class in [
        ReadClass::Gate,
        ReadClass::Hygiene,
        ReadClass::Observability,
    ] {
        for route in [
            &withdrawn as &dyn Fn(&RouteRequest<'_>) -> RouteDecision,
            &gateway_only,
            &unavailable,
        ] {
            let r = fetch_conditional_via(site(class), &gh, None, &target(), URL, None, route);
            assert!(is_shed(&r), "{class:?}");
        }
    }
    assert!(calls(&log).is_empty(), "{:?}", calls(&log));
}

/// A reader that cannot see the repo (403 "not accessible by integration",
/// or a 404 with no evidence it covers the repo) is not an answer: it is
/// withdrawn for the repo and the next reader serves the read (#12394 review).
#[test]
fn a_reader_blind_to_the_repo_fails_over_to_the_next_reader() {
    for refusing in ["reader-blind", "reader-gone"] {
        let tmp = tempfile::tempdir().unwrap();
        let (gh, log) = stub(tmp.path());
        let blind = tmp.path().join(refusing);
        let spare = tmp.path().join("reader-spare");
        std::fs::create_dir_all(&blind).unwrap();
        std::fs::create_dir_all(&spare).unwrap();
        let pid = std::process::id();
        let asked = Cell::new(0);
        let route = |_: &RouteRequest<'_>| {
            asked.set(asked.get() + 1);
            let (dir, app_id) = if asked.get() == 1 {
                (blind.clone(), format!("w7-{refusing}-{pid}"))
            } else {
                (spare.clone(), format!("w7-spare-{pid}"))
            };
            RouteDecision::Reader {
                dir,
                app_id,
                placement: Placement::Home,
            }
        };
        let (_, resp, _) =
            fetch_conditional_via(site(ReadClass::Gate), &gh, None, &target(), URL, None, &route)
                .unwrap();
        assert_eq!(resp.unwrap().status, 200, "{refusing}");
        assert_eq!(calls(&log), [refusing, "reader-spare"], "{refusing}: no writer call");
        let repo = target().repo.unwrap();
        assert!(
            crate::forge_identity::repo_withdrawn_at(
                &format!("w7-{refusing}-{pid}"),
                &repo,
                SystemTime::now()
            ),
            "{refusing}: withdrawn for the repo"
        );
    }
}

/// The only reader refusing the repo: the read is shed — never a 404 the
/// caller would read as "absent", and never the writer.
#[test]
fn the_only_reader_refusing_the_repo_is_a_shed_not_an_absent() {
    for refusing in ["reader-blind", "reader-gone"] {
        let tmp = tempfile::tempdir().unwrap();
        let (gh, log) = stub(tmp.path());
        let blind = tmp.path().join(refusing);
        std::fs::create_dir_all(&blind).unwrap();
        let app = format!("w7-only-{refusing}-{}", std::process::id());
        let asked = Cell::new(0);
        // The router offers the reader, then (it is withdrawn from the repo)
        // nothing: an unavailable pool. And a router that keeps offering it.
        let then_none = |_: &RouteRequest<'_>| {
            asked.set(asked.get() + 1);
            if asked.get() == 1 {
                RouteDecision::Reader {
                    dir: blind.clone(),
                    app_id: app.clone(),
                    placement: Placement::Home,
                }
            } else {
                RouteDecision::Exhausted {
                    until: SystemTime::now() + Duration::from_secs(600),
                    cause: ExhaustCause::Unavailable,
                }
            }
        };
        let same = |_: &RouteRequest<'_>| RouteDecision::Reader {
            dir: blind.clone(),
            app_id: app.clone(),
            placement: Placement::Home,
        };
        for route in [
            &then_none as &dyn Fn(&RouteRequest<'_>) -> RouteDecision,
            &same,
        ] {
            let r = fetch_conditional_via(
                site(ReadClass::Gate).item_scoped(),
                &gh,
                None,
                &target(),
                "repos/o/r/issues/7",
                None,
                route,
            );
            assert!(is_shed(&r), "{refusing}: {:?}", r.map(|x| x.1.map(|h| h.status)));
        }
        assert_eq!(calls(&log), [refusing, refusing], "{refusing}: writer stub NOT invoked");
    }
}
