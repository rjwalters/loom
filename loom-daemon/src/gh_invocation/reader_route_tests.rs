//! Read routing at the choke point (#9872).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::super::{AccessIntent, GhInvocation, GhTarget, Operation, ParentContext};
use crate::forge_bucket_book::Resource;
use crate::forge_identity::Failure;
use std::cell::RefCell;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

const OK: &str = "echo '{}'; exit 0";
const NOT_FOUND: &str = "echo 'HTTP 404: Not Found' >&2; exit 1";
const BAD_CREDS: &str = "echo 'HTTP 401: Bad credentials' >&2; exit 1";

struct Fixture {
    _tmp: tempfile::TempDir,
    reader: PathBuf,
    log: PathBuf,
    gh: PathBuf,
}

/// A stub `gh` that appends its `GH_CONFIG_DIR` to a log, then runs
/// `on_reader` under the reader's dir and `on_writer` under anything else.
fn fixture(on_reader: &str, on_writer: &str) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let reader = tmp.path().join("reader-app");
    std::fs::create_dir_all(&reader).unwrap();
    let log = tmp.path().join("calls.log");
    let gh = tmp.path().join("gh-stub");
    let body = format!(
        "#!/bin/sh\necho \"${{GH_CONFIG_DIR:-}}\" >> '{log}'\nif [ \"${{GH_CONFIG_DIR:-}}\" = '{r}' ]; then {on_reader}; else {on_writer}; fi\n",
        log = log.display(),
        r = reader.display(),
    );
    std::fs::write(&gh, body).unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    Fixture {
        _tmp: tmp,
        reader,
        log,
        gh,
    }
}

impl Fixture {
    /// The `GH_CONFIG_DIR` of each call, in order.
    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn inv(&self, intent: AccessIntent) -> GhInvocation {
        GhInvocation::new(
            Operation::new("issue.list"),
            intent,
            GhTarget::repo("o/r").unwrap(),
            Duration::from_secs(10),
        )
        .parent(ParentContext::Missing)
        .program(&self.gh)
        .args(["api", "repos/o/r/issues"])
    }

    /// Execute through the routing step with this fixture's reader for
    /// `o/r`, returning the withdrawals it asked for.
    fn run(&self, inv: GhInvocation) -> Vec<(String, String, Failure)> {
        let withdrawn = RefCell::new(Vec::new());
        let reader = self.reader.clone();
        let lookup = move |req: &crate::forge_identity::RouteRequest<'_>| {
            if req.owner_repo == "o/r" {
                crate::forge_identity::RouteDecision::Reader {
                    dir: reader.clone(),
                    app_id: "app-1".to_string(),
                    placement: crate::forge_identity::Placement::Home,
                }
            } else {
                crate::forge_identity::RouteDecision::NoPool
            }
        };
        let withdraw = |app: &str, slug: &str, f: Failure, _why: &str| {
            withdrawn
                .borrow_mut()
                .push((app.to_string(), slug.to_string(), f));
        };
        let _ = inv.execute_routed(&lookup, &withdraw);
        withdrawn.into_inner()
    }
}

fn is_reader(f: &Fixture, dir: &str) -> bool {
    Path::new(dir) == f.reader
}

#[test]
fn a_captured_repo_read_runs_under_the_reader() {
    let f = fixture(OK, OK);
    let withdrawn = f.run(f.inv(AccessIntent::Read));
    let calls = f.calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert!(is_reader(&f, &calls[0]), "{calls:?}");
    assert!(withdrawn.is_empty());
}

#[test]
fn writes_passthrough_and_pinned_reads_stay_on_the_writer() {
    for (name, make) in [
        (
            "write",
            (|f: &Fixture| f.inv(AccessIntent::Write)) as fn(&Fixture) -> GhInvocation,
        ),
        ("passthrough", |f| f.inv(AccessIntent::Read).passthrough()),
        ("writer_identity", |f| f.inv(AccessIntent::Read).writer_identity()),
        ("explicit dir", |f| {
            let other = f.reader.with_file_name("chosen");
            f.inv(AccessIntent::Read).gh_config_dir(Some(&other))
        }),
    ] {
        let f = fixture(OK, OK);
        let withdrawn = f.run(make(&f));
        let calls = f.calls();
        assert_eq!(calls.len(), 1, "{name}: {calls:?}");
        assert!(!is_reader(&f, &calls[0]), "{name} ran under the reader: {calls:?}");
        assert!(withdrawn.is_empty(), "{name}");
    }
}

#[test]
fn an_untargeted_read_or_no_reader_is_one_writer_call() {
    let f = fixture(OK, OK);
    let untargeted = GhInvocation::new(
        Operation::new("issue.list"),
        AccessIntent::Read,
        GhTarget::None,
        Duration::from_secs(10),
    )
    .program(&f.gh);
    f.run(untargeted);
    // A repo with no reader (no pool configured): the lookup says None.
    let other = f.inv(AccessIntent::Read);
    let other = GhInvocation {
        target: GhTarget::repo("x/y").unwrap(),
        ..other
    };
    f.run(other);
    let calls = f.calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(calls.iter().all(|c| !is_reader(&f, c)), "{calls:?}");
}

#[test]
fn an_auth_failure_on_the_reader_retries_on_the_writer_and_withdraws() {
    let f = fixture(BAD_CREDS, OK);
    let withdrawn = f.run(f.inv(AccessIntent::Read));
    let calls = f.calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(is_reader(&f, &calls[0]) && !is_reader(&f, &calls[1]), "{calls:?}");
    assert_eq!(withdrawn, vec![("app-1".into(), "o/r".into(), Failure::Credential)]);
}

#[test]
fn a_reader_404_the_writer_can_read_withdraws_the_reader_for_the_repo() {
    let f = fixture(NOT_FOUND, OK);
    let withdrawn = f.run(f.inv(AccessIntent::Read));
    assert_eq!(f.calls().len(), 2);
    assert_eq!(withdrawn, vec![("app-1".into(), "o/r".into(), Failure::Coverage)]);
}

#[test]
fn a_404_for_both_identities_does_not_withdraw_the_reader() {
    let f = fixture(NOT_FOUND, NOT_FOUND);
    let withdrawn = f.run(f.inv(AccessIntent::Read));
    assert_eq!(f.calls().len(), 2);
    assert!(withdrawn.is_empty(), "{withdrawn:?}");
}

#[test]
fn each_attempt_is_accounted_under_its_identity_role() {
    let f = fixture(BAD_CREDS, OK);
    let sink = tempfile::tempdir().unwrap();
    crate::forge_call_stats::set_test_sink_dir(Some(sink.path().to_path_buf()));
    f.run(f.inv(AccessIntent::Read));
    f.run(f.inv(AccessIntent::Write));
    let report = crate::forge_call_stats::status_report(chrono::Utc::now(), None);
    crate::forge_call_stats::set_test_sink_dir(None);
    let roles = report.identity_roles.unwrap();
    let count = |role: &str| {
        roles
            .iter()
            .find(|r| r.role == role)
            .map(|r| (r.ok, r.error))
    };
    assert_eq!(count("reader"), Some((0, 1)), "{roles:?}");
    assert_eq!(count("writer-fallback"), Some((1, 0)), "{roles:?}");
    assert_eq!(count("writer"), Some((1, 0)), "{roles:?}");
}

#[test]
fn a_graphql_unresolvable_repo_on_the_reader_falls_back_and_withdraws() {
    let unresolved =
        "echo \"GraphQL: Could not resolve to a Repository with the name 'o/r'.\" >&2; exit 1";
    let f = fixture(unresolved, OK);
    let withdrawn = f.run(f.inv(AccessIntent::Read));
    assert_eq!(f.calls().len(), 2);
    assert_eq!(withdrawn, vec![("app-1".into(), "o/r".into(), Failure::Coverage)]);
}

#[test]
fn a_non_credential_failure_on_the_reader_is_not_retried() {
    let f = fixture("echo 'HTTP 502: Bad Gateway' >&2; exit 1", OK);
    let withdrawn = f.run(f.inv(AccessIntent::Read));
    assert_eq!(f.calls().len(), 1);
    assert!(withdrawn.is_empty());
}

#[test]
fn routing_never_changes_the_env_plan_it_starts_from() {
    // No pool configured means byte-for-byte the pre-#9872 child: routing
    // lives in `execute`, so the plan of an eligible read equals the plan of
    // the same read pinned to the writer.
    let f = fixture(OK, OK);
    for inv in [
        f.inv(AccessIntent::Read),
        f.inv(AccessIntent::Read)
            .current_dir(f.reader.parent().unwrap()),
        GhInvocation::new(
            Operation::new("issue.list"),
            AccessIntent::Read,
            GhTarget::None,
            Duration::from_secs(10),
        ),
    ] {
        let pinned = inv.clone().writer_identity();
        assert_eq!(inv.env_plan_with(None, None), pinned.env_plan_with(None, None));
    }
}

#[test]
fn the_lookup_is_asked_for_the_pool_the_call_spends() {
    let f = fixture(OK, OK);
    let asked = RefCell::new(Vec::new());
    let lookup = |req: &crate::forge_identity::RouteRequest<'_>| {
        asked.borrow_mut().push(req.resource);
        crate::forge_identity::RouteDecision::NoPool
    };
    let withdraw = |_: &str, _: &str, _: Failure, _: &str| {};
    let _ = f.inv(AccessIntent::Read).execute_routed(&lookup, &withdraw);
    let graphql = GhInvocation::new(
        Operation::new("issue.view"),
        AccessIntent::Read,
        GhTarget::repo("o/r").unwrap(),
        Duration::from_secs(10),
    )
    .parent(ParentContext::Missing)
    .program(&f.gh)
    .args(["issue", "view", "1"]);
    let _ = graphql.execute_routed(&lookup, &withdraw);
    assert_eq!(asked.into_inner(), [Resource::Core, Resource::Graphql]);
}

#[test]
fn the_route_request_carries_an_etag_blind_affinity_key_and_the_resource() {
    // W4-B: two reads differing only in If-None-Match (and in --include /
    // --jq) ask the router with the same key, so they land on one reader.
    let f = fixture(OK, OK);
    let seen = RefCell::new(Vec::new());
    let lookup = |req: &crate::forge_identity::RouteRequest<'_>| {
        seen.borrow_mut().push((
            req.owner_repo.to_string(),
            req.affinity_key.map(str::to_string),
            req.resource,
            req.class,
        ));
        crate::forge_identity::RouteDecision::NoPool
    };
    let withdraw = |_: &str, _: &str, _: Failure, _: &str| {};
    for etag in ["W/\"1\"", "W/\"2\""] {
        let inv = GhInvocation::new(
            Operation::new("issue.view"),
            AccessIntent::Read,
            GhTarget::repo("o/r").unwrap(),
            Duration::from_secs(10),
        )
        .parent(ParentContext::Missing)
        .program(&f.gh)
        .args(["api", "--include", "repos/o/r/issues/5", "--jq", ".state"])
        .arg("-H")
        .arg(format!("If-None-Match: {etag}"));
        let _ = inv.execute_routed(&lookup, &withdraw);
    }
    let seen = seen.into_inner();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0], seen[1], "ETag rotation must not move a URL");
    assert_eq!(seen[0].0, "o/r");
    assert_eq!(seen[0].1.as_deref(), Some("api\u{1f}repos/o/r/issues/5"));
    assert_eq!(seen[0].2, crate::forge_bucket_book::Resource::Core);
    assert_eq!(seen[0].3, crate::forge_identity::ReadClass::Gate);
}

#[test]
fn an_exhausted_route_runs_on_the_writer() {
    // Every read is Gate until W4-C: Exhausted keeps today's writer path.
    let f = fixture(OK, OK);
    let lookup = |_: &crate::forge_identity::RouteRequest<'_>| {
        crate::forge_identity::RouteDecision::Exhausted {
            until: std::time::SystemTime::now(),
        }
    };
    let withdraw = |_: &str, _: &str, _: Failure, _: &str| {};
    let _ = f.inv(AccessIntent::Read).execute_routed(&lookup, &withdraw);
    let calls = f.calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert!(!is_reader(&f, &calls[0]));
}
