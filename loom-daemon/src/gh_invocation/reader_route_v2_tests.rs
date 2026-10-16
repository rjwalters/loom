//! W4-C: the class-aware read chain, the shed and the gone memo.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::super::cwd_route::{CwdAnswer, DeriveEnv};
use super::super::{AccessIntent, GhCompletion, GhInvocation, GhTarget, Operation, ParentContext};
use crate::cmd_out::{CmdOutcome, Unavailable};
use crate::forge_bucket_book::Resource;
use crate::forge_identity::{
    ExhaustCause, Failure, Placement, ReadClass, RouteDecision, RouteRequest,
};
use crate::proc_exec::Completion;
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
        now: SystemTime,
    ) -> (GhCompletion, Vec<(String, String, Failure)>, Vec<String>) {
        self.run_as(inv, pre, now, ExhaustCause::Budget)
    }

    /// [`Pool::run`] with the router's exhaustion `cause`.
    fn run_as(
        &self,
        inv: GhInvocation,
        pre: &[&str],
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
            // Every withdrawal takes the reader out of this repo's walk (a
            // coverage one for this repo only, which is the only repo here).
            withdrawn.borrow_mut().insert(app.to_string());
            log.borrow_mut()
                .push((app.to_string(), slug.to_string(), f));
        };
        let out = inv.execute_routed_v2_at(&lookup, &withdraw, now).unwrap();
        (out, log.into_inner(), asked.into_inner())
    }
}

fn is_rate_limited(f: &Failure) -> bool {
    matches!(f, Failure::RateLimited { .. })
}

// ===== spill, shed, gate =====

#[test]
fn a_hygiene_read_spills_to_the_next_reader_then_sheds_without_the_writer() {
    let p = pool(OK, LIMITED, OK);
    let (out, withdrawn, asked) =
        p.run(p.read("spill-a", ReadClass::Hygiene), &[], SystemTime::now());
    assert!(matches!(out, GhCompletion::Captured(_)));
    assert_eq!(p.who(), vec![Who::R2, Who::R1], "served on reader-1");
    assert_eq!(withdrawn.len(), 1);
    assert!(is_rate_limited(&withdrawn[0].2));
    assert!(asked.iter().all(|r| r == "acme/spill-a"), "{asked:?}");

    // Reader-1 is withdrawn too: the read is shed, the writer never runs.
    let p = pool(LIMITED, LIMITED, OK);
    let (out, withdrawn, _) = p.run(p.read("spill-b", ReadClass::Hygiene), &[], SystemTime::now());
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
fn a_gate_read_is_shed_like_any_other_and_an_unclassified_read_is_gate() {
    // #12394: no class falls to the writer; a Gate read is shed too.
    let p = pool(LIMITED, LIMITED, OK);
    let inv = p.read("gate-a", ReadClass::Gate);
    let (out, withdrawn, _) = p.run(inv, &[], SystemTime::now());
    assert!(matches!(out, GhCompletion::Shed { .. }), "{out:?}");
    assert_eq!(p.who(), vec![Who::R2, Who::R1], "writer stub NOT invoked");
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
fn exhausted_up_front_every_class_sheds_with_no_request() {
    for (name, class) in [
        ("up-a", ReadClass::Gate),
        ("up-b", ReadClass::Hygiene),
        ("up-c", ReadClass::Observability),
    ] {
        let p = pool(OK, OK, OK);
        let (out, _, _) = p.run(p.read(name, class), &["app-1", "app-2"], SystemTime::now());
        assert!(matches!(out, GhCompletion::Shed { .. }), "{class:?}: {out:?}");
        assert!(p.who().is_empty(), "{class:?}: no request at all");
    }
}

#[test]
fn a_read_no_reader_can_serve_for_any_cause_is_shed_never_run_on_the_writer() {
    // Every reader coverage-withdrawn from the repo, or no fresh token dir:
    // the router says Unavailable up front. Before #12394 a Gate or Hygiene
    // read then went to the writer; now it is shed.
    for class in [
        ReadClass::Gate,
        ReadClass::Hygiene,
        ReadClass::Observability,
    ] {
        let p = pool(OK, OK, OK);
        let (out, _, _) = p.run_as(
            p.read("unseen-a", class),
            &["app-1", "app-2"],
            SystemTime::now(),
            ExhaustCause::Unavailable,
        );
        assert!(matches!(out, GhCompletion::Shed { .. }), "{class:?}: {out:?}");
        assert!(p.who().is_empty(), "{class:?}: {:?}", p.who());
    }

    // Readers that refuse the credential: withdrawn, and once none is left
    // the read is shed, not re-run on the writer.
    const BAD: &str = "echo 'gh: Bad credentials (HTTP 401)' >&2; exit 1";
    let p = pool(BAD, BAD, OK);
    let (out, withdrawn, _) = p.run_as(
        p.read("unseen-b", ReadClass::Gate),
        &[],
        SystemTime::now(),
        ExhaustCause::Unavailable,
    );
    assert!(matches!(out, GhCompletion::Shed { .. }), "{out:?}");
    assert_eq!(p.who(), vec![Who::R2, Who::R1], "writer stub NOT invoked");
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
        SystemTime::now(),
        ExhaustCause::Unavailable,
    );
    assert!(matches!(out, GhCompletion::Shed { .. }), "{out:?}");
    assert_eq!(p.who(), vec![Who::R2, Who::R1], "writer stub NOT invoked");

    // After rate limits (budget): shed, as before.
    let p = pool(LIMITED, LIMITED, OK);
    let (out, _, _) = p.run(p.read("obs-c", ReadClass::Observability), &[], SystemTime::now());
    assert!(matches!(out, GhCompletion::Shed { .. }), "{out:?}");
    assert_eq!(p.who(), vec![Who::R2, Who::R1], "writer stub NOT invoked");
}

#[test]
fn a_mix_of_withdrawn_and_budget_exhausted_readers_never_calls_the_writer() {
    // reader-2 rate limited (budget), reader-1 refuses the credential
    // (withdrawn): the router answers `Withdrawn` and no class reaches the writer.
    const BAD: &str = "echo 'gh: Bad credentials (HTTP 401)' >&2; exit 1";
    for class in [
        ReadClass::Gate,
        ReadClass::Hygiene,
        ReadClass::Observability,
    ] {
        let p = pool(BAD, LIMITED, OK);
        let (out, withdrawn, _) =
            p.run_as(p.read("mix-a", class), &[], SystemTime::now(), ExhaustCause::Withdrawn);
        assert!(matches!(out, GhCompletion::Shed { .. }), "{class:?}: {out:?}");
        assert_eq!(p.who(), vec![Who::R2, Who::R1], "{class:?}: writer stub NOT invoked");
        assert_eq!(withdrawn.len(), 2);

        // Up front (both already out): no request at all.
        let p = pool(OK, OK, OK);
        let (out, _, _) = p.run_as(
            p.read("mix-b", class),
            &["app-1", "app-2"],
            SystemTime::now(),
            ExhaustCause::Withdrawn,
        );
        assert!(matches!(out, GhCompletion::Shed { .. }), "{class:?}: {out:?}");
        assert!(p.who().is_empty(), "{class:?}: {:?}", p.who());
    }
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
    for class in [
        ReadClass::Gate,
        ReadClass::Hygiene,
        ReadClass::Observability,
    ] {
        let p = pool(OK, OK, OK);
        let out = p
            .read("gw-only", class)
            .execute_routed_v2_at(
                &gateway_only,
                &|_: &str, _: &str, _: Failure, _: &str| {},
                SystemTime::now(),
            )
            .unwrap();
        assert!(matches!(out, GhCompletion::Shed { .. }), "{class:?}: {out:?}");
        assert!(p.who().is_empty(), "{class:?}: writer stub NOT invoked: {:?}", p.who());
    }
    // No gateway withdrawal: NoPool stays NoPool (a host with no readers at
    // all reads on the writer: the one allow-listed exception).
    assert_eq!(
        crate::forge_gateway::past_withdrawn_gateway(RouteDecision::NoPool, None),
        RouteDecision::NoPool
    );
}

#[test]
fn a_pool_that_vanishes_mid_read_is_shed_not_sent_to_the_writer() {
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
            SystemTime::now(),
        )
        .unwrap();
    assert!(matches!(out, GhCompletion::Shed { .. }), "{out:?}");
    assert_eq!(p.who(), vec![Who::R2], "writer stub NOT invoked");
}

#[test]
fn every_reader_attempt_names_the_derived_repo_and_the_writer_is_never_called() {
    let p = pool(LIMITED, LIMITED, OK);
    let (out, _, _) = p.run(p.read("env-a", ReadClass::Gate), &[], SystemTime::now());
    assert!(matches!(out, GhCompletion::Shed { .. }), "{out:?}");
    let calls = p.calls();
    assert_eq!(
        calls,
        vec![
            (Who::R2, "acme/env-a".to_string()),
            (Who::R1, "acme/env-a".to_string())
        ]
    );
}

#[test]
fn a_non_credential_failure_is_returned_without_spill_or_writer() {
    let p = pool(OK, "echo 'HTTP 502: Bad Gateway' >&2; exit 1", OK);
    let (_, withdrawn, _) =
        p.run(p.read("five-oh-two", ReadClass::Hygiene), &[], SystemTime::now());
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
            SystemTime::now(),
        )
        .unwrap();
    assert!(matches!(out, GhCompletion::Captured(_)));
    assert_eq!(p.who(), vec![Who::Writer]);
}

// ===== a 403/404: an answer only from a reader that covers the repo =====

const BLIND: &str = "echo 'gh: Resource not accessible by integration (HTTP 403)' >&2; exit 1";
const UNRESOLVED: &str =
    "echo \"GraphQL: Could not resolve to a Repository with the name 'acme/x'.\" >&2; exit 1";

#[test]
fn a_covered_readers_item_404_is_the_answer_and_nothing_is_withdrawn() {
    // W6: reader-2 recently served this repo's `issues`, so its 404 on
    // `issues/7` is the item's own answer: no other reader, no writer.
    let now = SystemTime::now();
    for (name, class) in [
        ("cov-a-gate", ReadClass::Gate),
        ("cov-a-hyg", ReadClass::Hygiene),
        ("cov-a-obs", ReadClass::Observability),
    ] {
        let slug = format!("acme/{name}");
        crate::forge_identity::served::note_reader_served("app-2", &slug, "issues", now);
        let p = pool(OK, NOT_FOUND, OK);
        let (out, withdrawn, _) = p.run(p.read(name, class), &[], now);
        assert!(matches!(out, GhCompletion::Captured(_)), "{class:?}: {out:?}");
        assert_eq!(p.who(), vec![Who::R2], "{class:?}: writer stub NOT invoked");
        assert!(withdrawn.is_empty(), "{class:?}: {withdrawn:?}");
    }
}

#[test]
fn evidence_comes_from_a_success_and_another_familys_404_is_shed_not_withdrawn() {
    // A 200 from reader-2 on `issues/7` is the evidence: recorded by the chain.
    let now = SystemTime::now();
    let p = pool(OK, OK, OK);
    let (out, _, _) = p.run(p.read("cov-ev", ReadClass::Gate), &[], now);
    assert!(matches!(out, GhCompletion::Captured(_)), "{out:?}");
    assert!(crate::forge_identity::served::reader_recently_served(
        "app-2",
        "acme/cov-ev",
        "issues",
        now
    ));
    // Evidence for `pulls` only does not make an `issues` 404 an answer, but
    // it does show reader-2 sees the repo: the read is shed (no answer) and
    // reader-2 is NOT withdrawn, so one missing item cannot take it off the
    // repo.
    crate::forge_identity::served::note_reader_served("app-2", "acme/cov-fam", "pulls", now);
    let p = pool(OK, NOT_FOUND, OK);
    let (out, withdrawn, _) = p.run(p.read("cov-fam", ReadClass::Gate), &[], now);
    assert!(matches!(out, GhCompletion::Shed { .. }), "{out:?}");
    assert_eq!(p.who(), vec![Who::R2], "writer stub NOT invoked");
    assert!(withdrawn.is_empty(), "{withdrawn:?}");
}

#[test]
fn an_uncovered_or_refusing_reader_is_withdrawn_for_the_repo_and_the_next_serves() {
    // A 403 "not accessible by integration", a GraphQL "could not resolve to
    // a repository", or a 404 with no evidence: the reader is blind to the
    // repo, not an answer. It is withdrawn (coverage: this repo only) and
    // the next reader serves the read. Even with evidence, a 403 is never an
    // item's answer.
    let now = SystemTime::now();
    for (name, refusal, seed) in [
        ("blind-a", BLIND, false),
        ("blind-b", UNRESOLVED, false),
        ("blind-c", NOT_FOUND, false),
        ("blind-d", BLIND, true),
    ] {
        if seed {
            crate::forge_identity::served::note_reader_served(
                "app-2",
                &format!("acme/{name}"),
                "issues",
                now,
            );
        }
        let p = pool(OK, refusal, OK);
        let (out, withdrawn, _) = p.run(p.read(name, ReadClass::Gate), &[], now);
        let GhCompletion::Captured(Completion::Exited(o)) = &out else {
            panic!("{name}: expected reader-1's answer, got {out:?}");
        };
        assert!(o.status.success(), "{name}: served by reader-1");
        assert_eq!(p.who(), vec![Who::R2, Who::R1], "{name}: writer stub NOT invoked");
        assert_eq!(withdrawn.len(), 1, "{name}: {withdrawn:?}");
        assert_eq!(withdrawn[0].0, "app-2", "{name}");
        assert_eq!(withdrawn[0].1, format!("acme/{name}"), "{name}");
        // A plain 404 with no evidence is withdrawn briefly (#12511).
        let want = if name == "blind-c" {
            Failure::CoverageUnproven
        } else {
            Failure::Coverage
        };
        assert_eq!(withdrawn[0].2, want, "{name}");
    }
}

#[test]
fn every_reader_refusing_the_repo_sheds_never_absent_and_never_the_writer() {
    for (name, r1, r2) in [
        ("blind-all-a", BLIND, BLIND),
        ("blind-all-b", NOT_FOUND, BLIND),
        ("blind-all-c", UNRESOLVED, NOT_FOUND),
    ] {
        let p = pool(r1, r2, OK);
        let (out, withdrawn, _) = p.run_as(
            p.read(name, ReadClass::Gate),
            &[],
            SystemTime::now(),
            ExhaustCause::Unavailable,
        );
        assert!(matches!(out, GhCompletion::Shed { .. }), "{name}: {out:?}");
        assert_eq!(p.who(), vec![Who::R2, Who::R1], "{name}: writer stub NOT invoked");
        assert!(
            withdrawn
                .iter()
                .all(|w| matches!(w.2, Failure::Coverage | Failure::CoverageUnproven)),
            "{name}: {withdrawn:?}"
        );
        // The caller sees "no answer", never a 404 it would read as absent.
        let outcome = super::super::outcome::classify(Ok(out), Duration::from_secs(10));
        assert!(
            matches!(outcome, CmdOutcome::Unavailable(Unavailable::Shed { .. })),
            "{name}: {outcome:?}"
        );
    }
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
    p.run(p.read("acct-a", ReadClass::Hygiene), &[], SystemTime::now());
    let p2 = pool(LIMITED, LIMITED, OK);
    p2.run(p2.read("acct-b", ReadClass::Observability), &[], SystemTime::now());
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

// ===== tick scope =====

/// #12074: inside a work-finder tick's read scope a read every reader has
/// rate-limited is shed at once, even a Gate read: no writer call, so the
/// tick answers from its caches and the dispatch App's budget is kept (since
/// #12394 that holds outside a tick scope too).
#[test]
fn a_tick_scoped_gate_read_is_shed_not_moved_to_the_writer() {
    let p = pool(LIMITED, LIMITED, OK);
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let scope = super::super::read_deadline::enter(deadline);
    let (out, _, _) = p.run(p.read("tick-gate", ReadClass::Gate), &[], SystemTime::now());
    drop(scope);
    assert_eq!(p.who(), vec![Who::R2, Who::R1], "writer stub NOT invoked");
    assert!(matches!(out, GhCompletion::Shed { .. }), "{out:?}");
}
