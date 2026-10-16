//! W4-C: the class-aware read chain, the shed and the gone memo.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::super::cwd_route::{CwdAnswer, DeriveEnv};
use super::super::{AccessIntent, GhCompletion, GhInvocation, GhTarget, Operation, ParentContext};
use super::v2::{is_gone_at, ShedPolicy};
use crate::cmd_out::{CmdOutcome, Unavailable};
use crate::forge_bucket_book::Resource;
use crate::forge_identity::{
    ExhaustCause, Failure, Placement, ReadClass, RouteDecision, RouteRequest,
};
use std::cell::RefCell;
use std::collections::HashSet;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const OK: &str = "echo '{}'; exit 0";
const LIMITED: &str =
    "echo 'gh: API rate limit exceeded for installation ID 1 (HTTP 403)' >&2; exit 1";
const NOT_FOUND: &str = "echo 'gh: Not Found (HTTP 404)' >&2; exit 1";

/// A stub `gh` logging `GH_CONFIG_DIR|GH_REPO` per call, answering by dir.
struct Pool {
    _tmp: tempfile::TempDir,
    r1: PathBuf,
    r2: PathBuf,
    log: PathBuf,
    gh: PathBuf,
}

fn pool(on_r1: &str, on_r2: &str, on_writer: &str) -> Pool {
    let tmp = tempfile::tempdir().unwrap();
    let r1 = tmp.path().join("reader-1");
    let r2 = tmp.path().join("reader-2");
    std::fs::create_dir_all(&r1).unwrap();
    std::fs::create_dir_all(&r2).unwrap();
    let log = tmp.path().join("calls.log");
    let gh = tmp.path().join("gh-stub");
    let body = format!(
        "#!/bin/sh\necho \"${{GH_CONFIG_DIR:-}}|${{GH_REPO:-}}\" >> '{log}'\n\
         case \"${{GH_CONFIG_DIR:-}}\" in\n'{r1}') {on_r1};;\n'{r2}') {on_r2};;\n*) {on_writer};;\nesac\n",
        log = log.display(),
        r1 = r1.display(),
        r2 = r2.display(),
    );
    std::fs::write(&gh, body).unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    Pool {
        _tmp: tmp,
        r1,
        r2,
        log,
        gh,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Who {
    R1,
    R2,
    Writer,
}

impl Pool {
    fn calls(&self) -> Vec<(Who, String)> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(|l| {
                let (dir, repo) = l.split_once('|').unwrap();
                let who = if Path::new(dir) == self.r1 {
                    Who::R1
                } else if Path::new(dir) == self.r2 {
                    Who::R2
                } else {
                    Who::Writer
                };
                (who, repo.to_string())
            })
            .collect()
    }

    fn who(&self) -> Vec<Who> {
        self.calls().into_iter().map(|(w, _)| w).collect()
    }

    /// An untargeted placeholder read in `cwd`, derived to `acme/<repo>`.
    fn read(&self, repo: &str, class: ReadClass) -> GhInvocation {
        let slug = format!("acme/{repo}");
        GhInvocation::new(
            Operation::new("worktree.issue_state_rest"),
            AccessIntent::Read,
            GhTarget::None,
            Duration::from_secs(10),
        )
        .parent(ParentContext::Missing)
        .program(&self.gh)
        .current_dir(self._tmp.path())
        .args(["api", "repos/{owner}/{repo}/issues/7", "--jq", ".state"])
        .read_class(class)
        .with_derived_route_in(
            &DeriveEnv::default(),
            &move |_: &Path, _: crate::forge_repo_facts::GhRepoEnv| CwdAnswer::Sole(slug.clone()),
        )
    }

    /// Run `inv` against a two-reader router (`reader-2` home, `reader-1`
    /// next) that honours this run's withdrawals; `pre` readers start
    /// withdrawn. With every reader withdrawn the router answers a
    /// [`ExhaustCause::Budget`] exhaustion. Returns the completion and the
    /// withdrawals.
    fn run(
        &self,
        inv: GhInvocation,
        pre: &[&str],
        policy: ShedPolicy,
        now: SystemTime,
    ) -> (GhCompletion, Vec<(String, String, Failure)>, Vec<String>) {
        self.run_as(inv, pre, policy, now, ExhaustCause::Budget)
    }

    /// [`Pool::run`] with the router's exhaustion `cause`.
    fn run_as(
        &self,
        inv: GhInvocation,
        pre: &[&str],
        policy: ShedPolicy,
        now: SystemTime,
        cause: ExhaustCause,
    ) -> (GhCompletion, Vec<(String, String, Failure)>, Vec<String>) {
        let withdrawn: RefCell<HashSet<String>> =
            RefCell::new(pre.iter().map(|s| (*s).to_string()).collect());
        let log = RefCell::new(Vec::new());
        let asked = RefCell::new(Vec::new());
        let (r1, r2) = (self.r1.clone(), self.r2.clone());
        let lookup = |req: &RouteRequest<'_>| {
            asked.borrow_mut().push(req.owner_repo.to_string());
            let w = withdrawn.borrow();
            for (app, dir) in [("app-2", &r2), ("app-1", &r1)] {
                if !w.contains(app) {
                    return RouteDecision::Reader {
                        dir: dir.clone(),
                        app_id: app.to_string(),
                        placement: Placement::Home,
                    };
                }
            }
            RouteDecision::Exhausted {
                until: now + Duration::from_secs(900),
                cause,
            }
        };
        let withdraw = |app: &str, slug: &str, f: Failure, _why: &str| {
            if f != Failure::Coverage {
                withdrawn.borrow_mut().insert(app.to_string());
            }
            log.borrow_mut()
                .push((app.to_string(), slug.to_string(), f));
        };
        let out = inv
            .execute_routed_v2_at(&lookup, &withdraw, policy, now)
            .unwrap();
        (out, log.into_inner(), asked.into_inner())
    }
}

const SHED_ON: ShedPolicy = ShedPolicy { shed: true };
const SHED_OFF: ShedPolicy = ShedPolicy { shed: false };

fn is_rate_limited(f: &Failure) -> bool {
    matches!(f, Failure::RateLimited { .. })
}

// ===== spill, shed, gate =====

#[test]
fn a_hygiene_read_spills_to_the_next_reader_then_sheds_without_the_writer() {
    let p = pool(OK, LIMITED, OK);
    let (out, withdrawn, asked) =
        p.run(p.read("spill-a", ReadClass::Hygiene), &[], SHED_ON, SystemTime::now());
    assert!(matches!(out, GhCompletion::Captured(_)));
    assert_eq!(p.who(), vec![Who::R2, Who::R1], "served on reader-1");
    assert_eq!(withdrawn.len(), 1);
    assert!(is_rate_limited(&withdrawn[0].2));
    assert!(asked.iter().all(|r| r == "acme/spill-a"), "{asked:?}");

    // Reader-1 is withdrawn too: the read is shed, the writer never runs.
    let p = pool(LIMITED, LIMITED, OK);
    let (out, withdrawn, _) =
        p.run(p.read("spill-b", ReadClass::Hygiene), &[], SHED_ON, SystemTime::now());
    assert_eq!(p.who(), vec![Who::R2, Who::R1], "writer stub NOT invoked");
    assert_eq!(withdrawn.len(), 2);
    let GhCompletion::Shed {
        owner, resource, ..
    } = &out
    else {
        panic!("expected a shed, got {out:?}");
    };
    assert_eq!((owner.as_str(), *resource), ("acme", Resource::Core));
    let outcome = super::super::outcome::classify(Ok(out), Duration::from_secs(10));
    assert!(
        matches!(outcome, CmdOutcome::Unavailable(Unavailable::Shed { .. })),
        "{outcome:?}"
    );
}

#[test]
fn a_gate_read_falls_to_the_writer_and_an_unclassified_read_is_gate() {
    let p = pool(LIMITED, LIMITED, OK);
    let inv = p.read("gate-a", ReadClass::Gate);
    let (out, withdrawn, _) = p.run(inv, &[], SHED_ON, SystemTime::now());
    assert!(matches!(out, GhCompletion::Captured(_)));
    assert_eq!(p.who(), vec![Who::R2, Who::R1, Who::Writer]);
    assert_eq!(withdrawn.len(), 2);

    let unclassified = GhInvocation::new(
        Operation::new("claim.issue_state"),
        AccessIntent::Read,
        GhTarget::None,
        Duration::from_secs(1),
    );
    assert_eq!(unclassified.class(), ReadClass::Gate);
}

#[test]
fn exhausted_up_front_gate_runs_on_the_writer_and_hygiene_sheds() {
    let p = pool(OK, OK, OK);
    let (out, _, _) =
        p.run(p.read("up-a", ReadClass::Gate), &["app-1", "app-2"], SHED_ON, SystemTime::now());
    assert!(matches!(out, GhCompletion::Captured(_)));
    assert_eq!(p.who(), vec![Who::Writer]);

    let p = pool(OK, OK, OK);
    let (out, _, _) = p.run(
        p.read("up-b", ReadClass::Observability),
        &["app-1", "app-2"],
        SHED_ON,
        SystemTime::now(),
    );
    assert!(matches!(out, GhCompletion::Shed { .. }), "{out:?}");
    assert!(p.who().is_empty(), "no request at all");
}

#[test]
fn a_hygiene_read_no_reader_can_serve_for_a_non_budget_cause_runs_on_the_writer() {
    // Every reader coverage-withdrawn from the repo, or no fresh token dir:
    // the router says Unavailable up front, and the read goes to the writer
    // (a plain writer read, no reader was tried) instead of being shed.
    let p = pool(OK, OK, OK);
    let (out, _, _) = p.run_as(
        p.read("unseen-a", ReadClass::Hygiene),
        &["app-1", "app-2"],
        SHED_ON,
        SystemTime::now(),
        ExhaustCause::Unavailable,
    );
    assert!(matches!(out, GhCompletion::Captured(_)), "{out:?}");
    assert_eq!(p.who(), vec![Who::Writer]);

    // Readers that refuse the credential: withdrawn, and once none is left
    // the router (honestly) says Unavailable: a writer fallback, not a shed.
    const BAD: &str = "echo 'gh: Bad credentials (HTTP 401)' >&2; exit 1";
    let p = pool(BAD, BAD, OK);
    let (out, withdrawn, _) = p.run_as(
        p.read("unseen-b", ReadClass::Hygiene),
        &[],
        SHED_ON,
        SystemTime::now(),
        ExhaustCause::Unavailable,
    );
    assert!(matches!(out, GhCompletion::Captured(_)), "{out:?}");
    assert_eq!(p.who(), vec![Who::R2, Who::R1, Who::Writer]);
    assert!(withdrawn.iter().all(|w| w.2 == Failure::Credential), "{withdrawn:?}");
}

#[test]
fn an_observability_read_never_reaches_the_writer_whatever_the_cause() {
    // 2026-10-15: the `fleet.state` history reads (Observability) drained the
    // writer App while the router answered `Unavailable` (a mix of budget
    // and non-budget withdrawals). Up front: shed, no request at all.
    let p = pool(OK, OK, OK);
    let (out, _, _) = p.run_as(
        p.read("obs-a", ReadClass::Observability),
        &["app-1", "app-2"],
        SHED_ON,
        SystemTime::now(),
        ExhaustCause::Unavailable,
    );
    assert!(matches!(out, GhCompletion::Shed { .. }), "{out:?}");
    assert!(p.who().is_empty(), "writer stub NOT invoked: {:?}", p.who());

    // After the readers refused the credential: still shed, never the writer.
    const BAD: &str = "echo 'gh: Bad credentials (HTTP 401)' >&2; exit 1";
    let p = pool(BAD, BAD, OK);
    let (out, _, _) = p.run_as(
        p.read("obs-b", ReadClass::Observability),
        &[],
        SHED_ON,
        SystemTime::now(),
        ExhaustCause::Unavailable,
    );
    assert!(matches!(out, GhCompletion::Shed { .. }), "{out:?}");
    assert_eq!(p.who(), vec![Who::R2, Who::R1], "writer stub NOT invoked");

    // After rate limits (budget): shed, as before.
    let p = pool(LIMITED, LIMITED, OK);
    let (out, _, _) =
        p.run(p.read("obs-c", ReadClass::Observability), &[], SHED_ON, SystemTime::now());
    assert!(matches!(out, GhCompletion::Shed { .. }), "{out:?}");
    assert_eq!(p.who(), vec![Who::R2, Who::R1], "writer stub NOT invoked");

    // The kill switch still restores the writer fallback.
    let p = pool(OK, OK, OK);
    let (out, _, _) = p.run_as(
        p.read("obs-d", ReadClass::Observability),
        &["app-1", "app-2"],
        SHED_OFF,
        SystemTime::now(),
        ExhaustCause::Unavailable,
    );
    assert!(matches!(out, GhCompletion::Captured(_)), "{out:?}");
    assert_eq!(p.who(), vec![Who::Writer]);
}

#[test]
fn a_mix_of_withdrawn_and_budget_exhausted_readers_never_calls_the_writer() {
    // reader-2 rate limited (budget), reader-1 refuses the credential
    // (withdrawn): the router answers `Withdrawn` — every exclusion ends on
    // its own — and neither deferrable class reaches the writer.
    const BAD: &str = "echo 'gh: Bad credentials (HTTP 401)' >&2; exit 1";
    for class in [ReadClass::Hygiene, ReadClass::Observability] {
        let p = pool(BAD, LIMITED, OK);
        let (out, withdrawn, _) = p.run_as(
            p.read("mix-a", class),
            &[],
            SHED_ON,
            SystemTime::now(),
            ExhaustCause::Withdrawn,
        );
        assert!(matches!(out, GhCompletion::Shed { .. }), "{class:?}: {out:?}");
        assert_eq!(p.who(), vec![Who::R2, Who::R1], "{class:?}: writer stub NOT invoked");
        assert_eq!(withdrawn.len(), 2);

        // Up front (both already out): no request at all.
        let p = pool(OK, OK, OK);
        let (out, _, _) = p.run_as(
            p.read("mix-b", class),
            &["app-1", "app-2"],
            SHED_ON,
            SystemTime::now(),
            ExhaustCause::Withdrawn,
        );
        assert!(matches!(out, GhCompletion::Shed { .. }), "{class:?}: {out:?}");
        assert!(p.who().is_empty(), "{class:?}: {:?}", p.who());
    }
    // A Gate read still takes the writer.
    let p = pool(OK, OK, OK);
    let (out, _, _) = p.run_as(
        p.read("mix-c", ReadClass::Gate),
        &["app-1", "app-2"],
        SHED_ON,
        SystemTime::now(),
        ExhaustCause::Withdrawn,
    );
    assert!(matches!(out, GhCompletion::Captured(_)), "{out:?}");
    assert_eq!(p.who(), vec![Who::Writer]);
}

#[test]
fn a_withdrawn_gateway_with_no_reader_pool_never_calls_the_writer() {
    // #12101 withdraws the gateway after repeated timeouts / 5xx. On a host
    // whose only reader is the gateway the pool then answers NoPool, which
    // used to mean "the writer". The gateway-first router now answers
    // `Withdrawn` until the gateway is back.
    let until = SystemTime::now() + Duration::from_secs(600);
    let gateway_only = |_: &RouteRequest<'_>| {
        crate::forge_gateway::past_withdrawn_gateway(RouteDecision::NoPool, Some(until))
    };
    assert_eq!(
        gateway_only(&RouteRequest::gate("acme/gw", None, Resource::Core)),
        RouteDecision::Exhausted {
            until,
            cause: ExhaustCause::Withdrawn
        }
    );
    for class in [ReadClass::Hygiene, ReadClass::Observability] {
        let p = pool(OK, OK, OK);
        let out = p
            .read("gw-only", class)
            .execute_routed_v2_at(
                &gateway_only,
                &|_: &str, _: &str, _: Failure, _: &str| {},
                SHED_ON,
                SystemTime::now(),
            )
            .unwrap();
        assert!(matches!(out, GhCompletion::Shed { .. }), "{class:?}: {out:?}");
        assert!(p.who().is_empty(), "{class:?}: writer stub NOT invoked: {:?}", p.who());
    }
    // No gateway withdrawal: NoPool stays NoPool (a host with no readers at
    // all reads on the writer, as before).
    assert_eq!(
        crate::forge_gateway::past_withdrawn_gateway(RouteDecision::NoPool, None),
        RouteDecision::NoPool
    );
}

#[test]
fn a_pool_that_vanishes_mid_read_falls_back_to_the_writer() {
    let p = pool(OK, LIMITED, OK);
    let first = std::cell::Cell::new(true);
    let r2 = p.r2.clone();
    let out = p
        .read("vanish", ReadClass::Hygiene)
        .execute_routed_v2_at(
            &|_: &RouteRequest<'_>| {
                if first.replace(false) {
                    RouteDecision::Reader {
                        dir: r2.clone(),
                        app_id: "app-2".into(),
                        placement: Placement::Home,
                    }
                } else {
                    RouteDecision::NoPool
                }
            },
            &|_: &str, _: &str, _: Failure, _: &str| {},
            SHED_ON,
            SystemTime::now(),
        )
        .unwrap();
    assert!(matches!(out, GhCompletion::Captured(_)), "{out:?}");
    assert_eq!(p.who(), vec![Who::R2, Who::Writer]);
}

#[test]
fn shed_off_sends_hygiene_to_the_writer_like_gate() {
    let p = pool(LIMITED, LIMITED, OK);
    let (out, _, _) = p.run(p.read("off-a", ReadClass::Hygiene), &[], SHED_OFF, SystemTime::now());
    assert!(matches!(out, GhCompletion::Captured(_)));
    assert_eq!(p.who(), vec![Who::R2, Who::R1, Who::Writer]);
}

#[test]
fn every_reader_attempt_names_the_derived_repo_and_the_writer_keeps_its_env() {
    let p = pool(LIMITED, LIMITED, OK);
    let (_, _, _) = p.run(p.read("env-a", ReadClass::Gate), &[], SHED_ON, SystemTime::now());
    let calls = p.calls();
    assert_eq!(calls[0], (Who::R2, "acme/env-a".to_string()));
    assert_eq!(calls[1], (Who::R1, "acme/env-a".to_string()));
    // The writer attempt carries whatever LOOM_REPO the process has (it is
    // not the derived slug unless LOOM_REPO says so).
    let loom_repo = std::env::var("LOOM_REPO")
        .or_else(|_| std::env::var("GH_REPO"))
        .unwrap_or_default();
    assert_eq!(calls[2], (Who::Writer, loom_repo));
}

#[test]
fn a_non_credential_failure_is_returned_without_spill_or_writer() {
    let p = pool(OK, "echo 'HTTP 502: Bad Gateway' >&2; exit 1", OK);
    let (_, withdrawn, _) =
        p.run(p.read("five-oh-two", ReadClass::Hygiene), &[], SHED_ON, SystemTime::now());
    assert_eq!(p.who(), vec![Who::R2]);
    assert!(withdrawn.is_empty());
}

#[test]
fn no_pool_is_one_plain_writer_call() {
    let p = pool(OK, OK, OK);
    let inv = p.read("nopool", ReadClass::Hygiene);
    let out = inv
        .execute_routed_v2_at(
            &|_: &RouteRequest<'_>| RouteDecision::NoPool,
            &|_: &str, _: &str, _: Failure, _: &str| {},
            SHED_ON,
            SystemTime::now(),
        )
        .unwrap();
    assert!(matches!(out, GhCompletion::Captured(_)));
    assert_eq!(p.who(), vec![Who::Writer]);
}

// ===== reader-only (#12389 / #12394) =====

/// A reader-only read never reaches the writer: not with no pool, not with
/// every reader out, not after a reader's coverage 404. `LOOM_READ_SHED=0`
/// (break-glass) lifts it like every other shed.
#[test]
fn a_reader_only_gate_read_is_shed_instead_of_reaching_the_writer() {
    let nopool = |_: &RouteRequest<'_>| RouteDecision::NoPool;
    let ignore = |_: &str, _: &str, _: Failure, _: &str| {};

    let p = pool(OK, OK, OK);
    let inv = p.read("ro-nopool", ReadClass::Gate).reader_only();
    let out = inv
        .execute_routed_v2_at(&nopool, &ignore, SHED_ON, SystemTime::now())
        .unwrap();
    assert!(matches!(out, GhCompletion::Shed { .. }), "{out:?}");
    assert!(p.who().is_empty(), "no request at all");

    let p = pool(LIMITED, LIMITED, OK);
    let inv = p.read("ro-limited", ReadClass::Gate).reader_only();
    let (out, _, _) = p.run(inv, &[], SHED_ON, SystemTime::now());
    assert!(matches!(out, GhCompletion::Shed { .. }), "{out:?}");
    assert_eq!(p.who(), vec![Who::R2, Who::R1], "writer stub NOT invoked");

    let p = pool(OK, NOT_FOUND, OK);
    let inv = p.read("ro-coverage", ReadClass::Gate).reader_only();
    let (out, _, _) = p.run(inv, &[], SHED_ON, SystemTime::now());
    assert!(matches!(out, GhCompletion::Shed { .. }), "{out:?}");
    assert_eq!(p.who(), vec![Who::R2], "no writer confirmation");

    // A reader that answers serves it as usual.
    let p = pool(OK, OK, OK);
    let (out, _, _) =
        p.run(p.read("ro-ok", ReadClass::Gate).reader_only(), &[], SHED_ON, SystemTime::now());
    assert!(matches!(out, GhCompletion::Captured(_)));
    assert_eq!(p.who(), vec![Who::R2]);

    // Break-glass: shedding off sends it to the writer like any Gate read.
    let p = pool(OK, OK, OK);
    let inv = p.read("ro-off", ReadClass::Gate).reader_only();
    let out = inv
        .execute_routed_v2_at(&nopool, &ignore, SHED_OFF, SystemTime::now())
        .unwrap();
    assert!(matches!(out, GhCompletion::Captured(_)));
    assert_eq!(p.who(), vec![Who::Writer]);
}

// ===== the 404 gone memo =====

#[test]
fn a_404_for_both_is_remembered_for_hygiene_but_gate_still_confirms() {
    let now = SystemTime::now();
    let p = pool(NOT_FOUND, NOT_FOUND, NOT_FOUND);
    let (_, withdrawn, _) = p.run(p.read("gone-a", ReadClass::Hygiene), &[], SHED_ON, now);
    assert_eq!(p.who(), vec![Who::R2, Who::Writer]);
    assert!(withdrawn.is_empty(), "the reader is not withdrawn");
    let key = super::super::affinity_key(&p.read("gone-a", ReadClass::Hygiene).args);
    assert!(is_gone_at("acme/gone-a", &key, now + Duration::from_secs(10)));

    // Within the TTL a Hygiene read costs one call (the reader's).
    let p2 = pool(NOT_FOUND, NOT_FOUND, NOT_FOUND);
    let (out, withdrawn, _) = p2.run(
        p2.read("gone-a", ReadClass::Hygiene),
        &[],
        SHED_ON,
        now + Duration::from_secs(60),
    );
    assert_eq!(p2.who(), vec![Who::R2]);
    assert!(withdrawn.is_empty());
    assert!(matches!(out, GhCompletion::Captured(_)));

    // A Gate read always confirms on the writer.
    let p3 = pool(NOT_FOUND, NOT_FOUND, NOT_FOUND);
    p3.run(p3.read("gone-a", ReadClass::Gate), &[], SHED_ON, now + Duration::from_secs(60));
    assert_eq!(p3.who(), vec![Who::R2, Who::Writer]);

    // Past the TTL the writer is asked again.
    let p4 = pool(NOT_FOUND, NOT_FOUND, NOT_FOUND);
    p4.run(
        p4.read("gone-a", ReadClass::Hygiene),
        &[],
        SHED_ON,
        now + Duration::from_secs(3700),
    );
    assert_eq!(p4.who(), vec![Who::R2, Who::Writer]);
}

#[test]
fn a_reader_404_the_writer_can_read_withdraws_for_the_repo_and_is_not_remembered() {
    let now = SystemTime::now();
    let p = pool(OK, NOT_FOUND, OK);
    let (_, withdrawn, _) = p.run(p.read("cov-a", ReadClass::Hygiene), &[], SHED_ON, now);
    assert_eq!(p.who(), vec![Who::R2, Who::Writer]);
    assert_eq!(withdrawn, vec![("app-2".into(), "acme/cov-a".into(), Failure::Coverage)]);
    let key = super::super::affinity_key(&p.read("cov-a", ReadClass::Hygiene).args);
    assert!(!is_gone_at("acme/cov-a", &key, now));
}

// ===== the shed's text =====

#[test]
fn a_shed_never_reads_as_a_rate_limit() {
    let shed = Unavailable::Shed {
        until: SystemTime::UNIX_EPOCH + Duration::from_secs(1_900_000_000),
    };
    let text = shed.to_string();
    assert!(text.starts_with("deferred: reader budget low until 2030-"), "{text}");
    assert!(text.ends_with("(loom-shed)"), "{text}");
    assert!(!crate::rate_limit_breaker::indicates_rate_limit(&text));
    assert!(crate::rate_limit_breaker::global_observe_failure(&text, "w4c_test").is_none());
    // The way most consumers see it: wrapped in their own error.
    let wrapped = format!("failed to invoke gh (worktree.issue_state): {text}");
    assert!(!crate::rate_limit_breaker::indicates_rate_limit(&wrapped));
}

// ===== accounting =====

#[test]
fn derived_reads_book_role_reader_with_the_derived_repo_and_sheds_charge_nothing() {
    let sink = tempfile::tempdir().unwrap();
    crate::forge_call_stats::set_test_sink_dir(Some(sink.path().to_path_buf()));
    let p = pool(OK, LIMITED, OK);
    p.run(p.read("acct-a", ReadClass::Hygiene), &[], SHED_ON, SystemTime::now());
    let p2 = pool(LIMITED, LIMITED, OK);
    p2.run(p2.read("acct-b", ReadClass::Observability), &[], SHED_ON, SystemTime::now());
    crate::forge_call_stats::set_test_sink_dir(None);
    let mut rows = Vec::new();
    for entry in std::fs::read_dir(sink.path()).unwrap().flatten() {
        if entry.file_name().to_string_lossy().starts_with("calls-") {
            let text = std::fs::read_to_string(entry.path()).unwrap();
            rows.extend(
                text.lines()
                    .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap()),
            );
        }
    }
    let of = |repo: &str| -> Vec<&serde_json::Value> {
        rows.iter().filter(|r| r["rp"] == repo).collect()
    };
    let a = of("acme/acct-a");
    assert_eq!(a.len(), 2, "{a:?}");
    assert!(
        a.iter()
            .all(|r| r["ir"] == "reader" && r["ro"] == "derived"),
        "{a:?}"
    );
    let b = of("acme/acct-b");
    let shed: Vec<_> = b.iter().filter(|r| r["o"] == "shed").collect();
    assert_eq!(shed.len(), 1, "{b:?}");
    assert_eq!(shed[0]["ir"], "reader");
    assert_eq!(shed[0]["tk"], "shed");
    assert_eq!(shed[0]["rr"], "core");
    assert!(b.iter().all(|r| r["ir"] != "writer-fallback"), "{b:?}");
}

// ===== kill switch: legacy =====

#[test]
fn the_legacy_path_keeps_the_pre_change_fallback() {
    // `LOOM_READ_ROUTING=legacy` runs `execute_routed`: no derivation (the
    // DeriveEnv says legacy), and a typed read whose reader is rate-limited
    // re-runs on the writer whatever its class.
    let p = pool(LIMITED, LIMITED, OK);
    let legacy = DeriveEnv {
        legacy: true,
        ..DeriveEnv::default()
    };
    let fresh = GhInvocation::new(
        Operation::new("worktree.issue_state_rest"),
        AccessIntent::Read,
        GhTarget::None,
        Duration::from_secs(10),
    )
    .current_dir(p._tmp.path())
    .args(["api", "repos/{owner}/{repo}/issues/7"])
    .with_derived_route_in(&legacy, &|_: &Path, _: crate::forge_repo_facts::GhRepoEnv| {
        CwdAnswer::Sole("acme/x".into())
    });
    assert!(fresh.reader_slug().is_none());

    let typed = GhInvocation::new(
        Operation::new("issue.list"),
        AccessIntent::Read,
        GhTarget::repo("acme/legacy-b").unwrap(),
        Duration::from_secs(10),
    )
    .parent(ParentContext::Missing)
    .program(&p.gh)
    .args(["api", "repos/acme/legacy-b/issues"])
    .read_class(ReadClass::Hygiene);
    let r2 = p.r2.clone();
    let withdrawn = RefCell::new(Vec::new());
    let out = typed
        .execute_routed(
            &move |_: &RouteRequest<'_>| RouteDecision::Reader {
                dir: r2.clone(),
                app_id: "app-2".into(),
                placement: Placement::Home,
            },
            &|app: &str, slug: &str, f: Failure, _: &str| {
                withdrawn
                    .borrow_mut()
                    .push((app.to_string(), slug.to_string(), f))
            },
        )
        .unwrap();
    assert!(matches!(out, GhCompletion::Captured(_)));
    assert_eq!(p.who(), vec![Who::R2, Who::Writer], "the pre-change reader → writer");
    assert_eq!(withdrawn.borrow().len(), 1);
}

/// #12074: inside a work-finder tick's read scope a read every reader has
/// rate-limited is shed at once, even a Gate read: no writer call, so the
/// tick answers from its caches and the dispatch App's budget is kept.
#[test]
fn a_tick_scoped_gate_read_is_shed_not_moved_to_the_writer() {
    let p = pool(LIMITED, LIMITED, OK);
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let scope = super::super::read_deadline::enter(deadline, true);
    let (out, _, _) = p.run(p.read("tick-gate", ReadClass::Gate), &[], SHED_ON, SystemTime::now());
    drop(scope);
    assert_eq!(p.who(), vec![Who::R2, Who::R1], "writer stub NOT invoked");
    assert!(matches!(out, GhCompletion::Shed { .. }), "{out:?}");

    // #12074 review: readers out for a cause that is not budget still reach
    // the writer inside a tick: only a rate-limit back-off is shed.
    let p = pool(LIMITED, LIMITED, OK);
    let scope = super::super::read_deadline::enter(deadline, true);
    let (out, _, _) = p.run_as(
        p.read("tick-unavail", ReadClass::Gate),
        &[],
        SHED_ON,
        SystemTime::now(),
        ExhaustCause::Unavailable,
    );
    drop(scope);
    assert!(matches!(out, GhCompletion::Captured(_)), "{out:?}");
    assert_eq!(p.who(), vec![Who::R2, Who::R1, Who::Writer]);
}

/// #12074 review: a starved repo's escalated tick lifts the budget shed in
/// the facade chain too: a Hygiene read whose readers are all out of budget
/// reaches the writer.
#[test]
fn an_escalated_tick_sends_a_budget_exhausted_hygiene_read_to_the_writer() {
    let p = pool(OK, OK, OK);
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let scope = super::super::read_deadline::enter(deadline, true);
    let (out, _, _) = p.run(
        p.read("esc-a", ReadClass::Hygiene),
        &["app-1", "app-2"],
        SHED_ON,
        SystemTime::now(),
    );
    assert!(matches!(out, GhCompletion::Shed { .. }), "an ordinary tick sheds: {out:?}");
    assert!(p.who().is_empty());
    let escalated = super::super::read_deadline::allow_writer();
    let (out, _, _) = p.run(
        p.read("esc-b", ReadClass::Hygiene),
        &["app-1", "app-2"],
        SHED_ON,
        SystemTime::now(),
    );
    drop(escalated);
    drop(scope);
    assert!(matches!(out, GhCompletion::Captured(_)), "{out:?}");
    assert_eq!(p.who(), vec![Who::Writer]);
}
