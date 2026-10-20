//! #12555: a [`GhInvocation::reader_only`] read never mints the writer.
//! #12584: neither does any other read its site did not pin to the writer,
//! whatever builder made it and whatever credential it was handed.
//!
//! The in-process daemon reads (`claim_reconciliation::gh_call::read`,
//! `SweepRegistry::gh_inv(.., AccessIntent::Read, ..)`) are reader-only.
//! Each test drives the reader chain against a stub `gh` that logs which
//! credential directory it ran under, and asserts the writer (no reader
//! directory) is never run on a host that has a reader pool.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::super::cwd_route::{CwdAnswer, DeriveEnv};
use super::super::{AccessIntent, GhCompletion, GhInvocation, GhTarget, Operation, ParentContext};
use crate::forge_identity::{ExhaustCause, Failure, Placement, RouteDecision, RouteRequest};
use std::cell::RefCell;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// A stub `gh` answering `{}` and logging `GH_CONFIG_DIR` per call, and one
/// reader directory.
struct Stub {
    tmp: tempfile::TempDir,
    reader: PathBuf,
    log: PathBuf,
    gh: PathBuf,
}

fn stub() -> Stub {
    let tmp = tempfile::tempdir().unwrap();
    let reader = tmp.path().join("reader");
    std::fs::create_dir_all(&reader).unwrap();
    let log = tmp.path().join("calls.log");
    let gh = tmp.path().join("gh-stub");
    let body = format!(
        "#!/bin/sh\necho \"${{GH_CONFIG_DIR:-writer}}\" >> '{}'\necho '{{}}'\nexit 0\n",
        log.display()
    );
    std::fs::write(&gh, body).unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    Stub {
        tmp,
        reader,
        log,
        gh,
    }
}

impl Stub {
    /// The `GH_CONFIG_DIR` each call ran under (`writer` when unset).
    fn dirs(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// `true` per call that ran under the reader directory, `false` per call
    /// that ran on the writer.
    fn calls(&self) -> Vec<bool> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(|l| Path::new(l) == self.reader)
            .collect()
    }

    /// An untargeted `{owner}/{repo}` comments read, as the claim and guard
    /// passes build it.
    fn read(&self, op: &'static str) -> GhInvocation {
        GhInvocation::new(
            Operation::new(op),
            AccessIntent::Read,
            GhTarget::None,
            Duration::from_secs(10),
        )
        .parent(ParentContext::Missing)
        .program(&self.gh)
        .current_dir(self.tmp.path())
        .args([
            "api",
            "repos/{owner}/{repo}/issues/7/comments",
            "--paginate",
        ])
    }

    /// Derive `inv`'s route with the checkout answering `checkout`, then run
    /// it against a router that always answers `decision`. Returns the
    /// completion and the repos the router was asked about.
    fn run(
        &self,
        inv: GhInvocation,
        checkout: CwdAnswer,
        decision: &RouteDecision,
    ) -> (GhCompletion, Vec<String>) {
        let inv =
            inv.with_derived_route_in(&DeriveEnv::default(), &move |_: &Path, _| checkout.clone());
        let asked = RefCell::new(Vec::new());
        let lookup = |req: &RouteRequest<'_>| {
            asked.borrow_mut().push(req.owner_repo.to_string());
            decision.clone()
        };
        let withdraw = |_: &str, _: &str, _: Failure, _: &str| {};
        let done = inv
            .execute_routed_v2_at(&lookup, &withdraw, SystemTime::now())
            .unwrap();
        (done, asked.into_inner())
    }

    fn reader_decision(&self) -> RouteDecision {
        RouteDecision::Reader {
            dir: self.reader.clone(),
            app_id: "app-1".to_string(),
            placement: Placement::Home,
        }
    }
}

fn exhausted() -> RouteDecision {
    RouteDecision::Exhausted {
        until: SystemTime::now() + Duration::from_secs(60),
        cause: ExhaustCause::Budget,
    }
}

fn is_shed(done: &GhCompletion) -> bool {
    matches!(done, GhCompletion::Shed { .. })
}

#[test]
fn the_daemon_read_helpers_build_reader_only_reads() {
    let gh = Path::new("gh");
    let root = Path::new("/");
    use crate::claim_reconciliation::gh_call;
    assert!(gh_call::read("claim.lease_comments", gh, root).is_reader_only());
    assert!(
        gh_call::read_within("quarantine.issue_comments", gh, root, Duration::from_secs(1))
            .is_reader_only()
    );
    assert!(gh_call::inv("verdict.pr_comments", AccessIntent::Read, gh, root).is_reader_only());
    assert!(!gh_call::write("claim.pr_add_label", gh, root).is_reader_only());
}

#[test]
fn writer_identity_cannot_pin_a_reader_only_read() {
    let s = stub();
    // Builder order must not matter.
    let before = s
        .read("guard.lease_comments")
        .writer_identity()
        .reader_only();
    let after = s
        .read("guard.lease_comments")
        .reader_only()
        .writer_identity();
    for inv in [before, after] {
        let (done, _) =
            s.run(inv, CwdAnswer::Sole("acme/widget".to_string()), &s.reader_decision());
        assert!(!is_shed(&done));
    }
    assert_eq!(s.calls(), vec![true, true], "both ran on the reader");
}

#[test]
fn a_reader_only_read_no_reader_can_serve_is_shed_not_sent_to_the_writer() {
    let s = stub();
    let inv = s
        .read("claim.lease_comments")
        .reader_only()
        .writer_identity();
    let (done, _) = s.run(inv, CwdAnswer::Sole("acme/widget".to_string()), &exhausted());
    assert!(is_shed(&done), "{done:?}");
    assert!(s.calls().is_empty(), "nothing was sent: {:?}", s.calls());
}

#[test]
fn an_unroutable_reader_only_read_is_shed_on_a_pooled_host() {
    // The checkout's sources disagree, so no route is derived: before #12555
    // this ran on the writer. The accounting repo only tells the router
    // which owner to classify; the read is not sent to the reader under it.
    for decision in [stub().reader_decision(), exhausted()] {
        let s = stub();
        let inv = s
            .read("verdict.pr_comments")
            .identity_scope(None, Some("acme/widget"))
            .reader_only();
        let (done, asked) = s.run(inv, CwdAnswer::Disagree, &decision);
        assert!(is_shed(&done), "{done:?}");
        assert_eq!(asked, vec!["acme/widget".to_string()]);
        assert!(s.calls().is_empty(), "nothing was sent: {:?}", s.calls());
    }
}

#[test]
fn a_host_with_no_reader_pool_still_reads_on_its_only_credential() {
    let s = stub();
    let inv = s
        .read("claim.pr_timeline")
        .identity_scope(None, Some("acme/widget"))
        .reader_only();
    let (done, _) = s.run(inv, CwdAnswer::Disagree, &RouteDecision::NoPool);
    assert!(!is_shed(&done));
    assert_eq!(s.calls(), vec![false], "the writer: there is no other credential");
}

#[test]
fn an_unroutable_read_is_shed_without_the_marker_too() {
    // #12584: no `reader_only()` on the builder; still never the writer.
    let s = stub();
    let inv = s
        .read("other.read")
        .identity_scope(None, Some("acme/widget"));
    assert!(inv.is_reader_only());
    let (done, asked) = s.run(inv, CwdAnswer::Disagree, &s.reader_decision());
    assert!(is_shed(&done), "{done:?}");
    assert_eq!(asked, vec!["acme/widget".to_string()]);
    assert!(s.calls().is_empty(), "nothing was sent: {:?}", s.calls());
}

#[test]
fn a_routable_read_without_the_marker_rides_the_reader_or_is_shed() {
    let s = stub();
    let (done, _) = s.run(
        s.read("pr_lease.comments"),
        CwdAnswer::Sole("acme/widget".to_string()),
        &s.reader_decision(),
    );
    assert!(!is_shed(&done));
    let (done, _) = s.run(
        s.read("pr_lease.comments"),
        CwdAnswer::Sole("acme/widget".to_string()),
        &exhausted(),
    );
    assert!(is_shed(&done), "{done:?}");
    assert_eq!(s.calls(), vec![true], "the reader, then nothing");
}

#[test]
fn a_writer_pinned_read_is_the_only_one_left_on_the_writer() {
    // An asker-dependent probe, listed in `tests/writer_read_guard.rs`.
    let s = stub();
    let inv = s
        .read("probe.permission")
        .identity_scope(None, Some("acme/widget"))
        .writer_identity();
    assert!(!inv.is_reader_only());
    let (done, asked) = s.run(inv, CwdAnswer::Disagree, &s.reader_decision());
    assert!(!is_shed(&done));
    assert!(asked.is_empty(), "the router is not consulted");
    assert_eq!(s.calls(), vec![false]);
}

#[test]
fn a_reader_credential_the_site_chose_is_left_alone() {
    let s = stub();
    let inv = s
        .read("store.read")
        .gh_config_dir(Some(&s.reader))
        .identity_role(crate::forge_identity::IdentityRole::Reader)
        .without_token_env();
    let inv = inv.with_derived_route_in(&DeriveEnv::default(), &|_: &Path, _| CwdAnswer::Disagree);
    assert!(!inv.has_site_credential_set_aside());
    let (done, asked) = s.run(inv, CwdAnswer::Disagree, &exhausted());
    assert!(!is_shed(&done));
    assert!(asked.is_empty(), "a hand-routed reader read is not re-routed");
    assert_eq!(s.calls(), vec![true]);
}

/// A writer directory a site hands a read (`fleet_store`'s writer App dir,
/// `forge_etag_store`'s owner dir), in every combination of the three
/// markers that name it.
fn writer_credentials(s: &Stub, op: &'static str) -> Vec<GhInvocation> {
    let chosen = s.tmp.path().join("writer-app");
    std::fs::create_dir_all(&chosen).unwrap();
    let writer = crate::forge_identity::IdentityRole::Writer;
    vec![
        s.read(op).gh_config_dir(Some(&chosen)),
        s.read(op).gh_config_dir(Some(&chosen)).without_token_env(),
        s.read(op).identity_role(writer),
        s.read(op)
            .gh_config_dir(Some(&chosen))
            .identity_role(writer)
            .without_token_env(),
    ]
}

#[test]
fn a_writer_credential_a_site_chose_is_replaced_by_a_reader() {
    let s = stub();
    for inv in writer_credentials(&s, "store.read") {
        let (done, asked) =
            s.run(inv, CwdAnswer::Sole("acme/widget".to_string()), &s.reader_decision());
        assert!(!is_shed(&done), "{done:?}");
        assert_eq!(asked, vec!["acme/widget".to_string()]);
    }
    assert_eq!(s.calls(), vec![true; 4], "every one ran on the reader");
}

#[test]
fn a_writer_credential_a_site_chose_is_shed_when_no_reader_can_serve() {
    let s = stub();
    for inv in writer_credentials(&s, "store.read") {
        let (done, _) = s.run(inv, CwdAnswer::Sole("acme/widget".to_string()), &exhausted());
        assert!(is_shed(&done), "{done:?}");
    }
    for inv in writer_credentials(&s, "store.unroutable") {
        let inv = inv.identity_scope(None, Some("acme/widget"));
        let (done, _) = s.run(inv, CwdAnswer::Disagree, &s.reader_decision());
        assert!(is_shed(&done), "{done:?}");
    }
    assert!(s.calls().is_empty(), "nothing was sent: {:?}", s.dirs());
}

#[test]
fn a_writer_credential_a_site_chose_is_restored_on_a_host_with_no_pool() {
    let s = stub();
    let chosen = s.tmp.path().join("writer-app");
    for inv in writer_credentials(&s, "store.read") {
        let had_dir = inv.env_plan(None).iter().any(|e| e.key == "GH_CONFIG_DIR");
        let (done, _) =
            s.run(inv, CwdAnswer::Sole("acme/widget".to_string()), &RouteDecision::NoPool);
        assert!(!is_shed(&done));
        let ran = s.dirs().pop().unwrap();
        if had_dir {
            assert_eq!(Path::new(&ran), chosen, "the owner's writer the site picked");
        }
    }
    assert_eq!(s.calls(), vec![false; 4], "no reader exists: the writer");
}

#[test]
fn a_graphql_read_that_names_its_repo_rides_the_reader() {
    let s = stub();
    let inv = GhInvocation::new(
        Operation::new("guard.open_pr_graphql"),
        AccessIntent::Read,
        GhTarget::None,
        Duration::from_secs(10),
    )
    .parent(ParentContext::Missing)
    .program(&s.gh)
    .current_dir(s.tmp.path())
    .args(["api", "graphql", "-f", "query=query { viewer { login } }"])
    .reader_only()
    .reader_repo("acme/widget");
    let (done, asked) = s.run(inv, CwdAnswer::Unresolved, &s.reader_decision());
    assert!(!is_shed(&done));
    assert_eq!(asked, vec!["acme/widget".to_string()]);
    assert_eq!(s.calls(), vec![true]);
}
