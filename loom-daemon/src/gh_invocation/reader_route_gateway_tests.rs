//! 2am#4367 P4: a gateway outage falls back to the readers, then the writer.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::super::{AccessIntent, GhCompletion, GhInvocation, GhTarget, Operation, ParentContext};
use crate::forge_gateway::GATEWAY_APP_ID;
use crate::forge_identity::{Failure, Placement, ReadClass, RouteDecision, RouteRequest};
use crate::proc_exec::Completion;
use std::cell::RefCell;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

const DOWN: &str =
    "echo 'Get \"https://gw.example.com/repos/acme/r\": dial tcp: connection refused' >&2; exit 1";
const BAD_GATEWAY: &str =
    "echo '{\"error\":\"upstream_error\",\"request_id\":\"r1\"}'; echo 'gh: HTTP 502' >&2; exit 1";
/// A 502 GitHub itself returned through the relay: an answer, not an outage.
const RELAYED_502: &str =
    "echo '{\"message\":\"Server Error\"}'; echo 'gh: Server Error (HTTP 502)' >&2; exit 1";
/// gh refused the argv before sending anything.
const GH_REFUSED: &str = "echo 'unknown flag: --nope' >&2; exit 1";
const NOT_FOUND: &str = "echo 'gh: Not Found (HTTP 404)' >&2; exit 1";
const OK: &str = "echo '{}'; exit 0";

struct Stub {
    _tmp: tempfile::TempDir,
    gw: PathBuf,
    reader: PathBuf,
    log: PathBuf,
    gh: PathBuf,
}

fn stub(on_gw: &str, on_reader: &str, on_writer: &str) -> Stub {
    let tmp = tempfile::tempdir().unwrap();
    let gw = tmp.path().join("gw");
    let reader = tmp.path().join("reader");
    let log = tmp.path().join("calls.log");
    let gh = tmp.path().join("gh-stub");
    std::fs::write(
        &gh,
        format!(
            "#!/bin/sh\necho \"${{GH_CONFIG_DIR:-writer}}\" >> '{log}'\n\
             case \"${{GH_CONFIG_DIR:-}}\" in\n'{gw}') {on_gw};;\n'{r}') {on_reader};;\n*) {on_writer};;\nesac\n",
            log = log.display(),
            gw = gw.display(),
            r = reader.display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    Stub {
        _tmp: tmp,
        gw,
        reader,
        log,
        gh,
    }
}

impl Stub {
    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(|l| {
                if std::path::Path::new(l) == self.gw {
                    "gateway".into()
                } else if std::path::Path::new(l) == self.reader {
                    "reader".into()
                } else {
                    "writer".into()
                }
            })
            .collect()
    }

    /// Run one targeted read against a router offering the gateway first,
    /// then (`with_reader`) a reader, honouring the run's withdrawals.
    fn run(&self, with_reader: bool) -> (GhCompletion, Vec<(String, Failure)>) {
        self.run_on(with_reader, "acme/r")
    }

    /// [`Stub::run`] for `repo` (`issues/1` of it).
    fn run_on(&self, with_reader: bool, repo: &str) -> (GhCompletion, Vec<(String, Failure)>) {
        let withdrawn: RefCell<Vec<String>> = RefCell::new(Vec::new());
        let log = RefCell::new(Vec::new());
        let lookup = |_: &RouteRequest<'_>| {
            let w = withdrawn.borrow();
            if !w.iter().any(|a| a == GATEWAY_APP_ID)
                && crate::forge_gateway::outage::take_test_outages().is_empty()
            {
                return RouteDecision::Reader {
                    dir: self.gw.clone(),
                    app_id: GATEWAY_APP_ID.into(),
                    placement: Placement::Home,
                };
            }
            drop(w);
            withdrawn.borrow_mut().push(GATEWAY_APP_ID.into());
            if with_reader && !withdrawn.borrow().iter().any(|a| a == "app-1") {
                return RouteDecision::Reader {
                    dir: self.reader.clone(),
                    app_id: "app-1".into(),
                    placement: Placement::Home,
                };
            }
            RouteDecision::NoPool
        };
        let withdraw = |app: &str, _: &str, f: Failure, _: &str| {
            withdrawn.borrow_mut().push(app.to_string());
            log.borrow_mut().push((app.to_string(), f));
        };
        let inv = GhInvocation::new(
            Operation::new("issue.view"),
            AccessIntent::Read,
            GhTarget::repo(repo).unwrap(),
            Duration::from_secs(10),
        )
        .parent(ParentContext::Missing)
        .program(&self.gh)
        .args(["api".to_string(), format!("repos/{repo}/issues/1")])
        .read_class(ReadClass::Gate);
        let out = inv
            .execute_routed_v2_at(&lookup, &withdraw, SystemTime::now())
            .unwrap();
        (out, log.into_inner())
    }
}

fn ok(out: &GhCompletion) -> bool {
    matches!(out, GhCompletion::Captured(Completion::Exited(o)) if o.status.success())
}

#[test]
fn an_unreachable_gateway_falls_back_to_the_reader() {
    let s = stub(DOWN, OK, OK);
    let (out, withdrawals) = s.run(true);
    assert!(ok(&out), "{out:?}");
    assert_eq!(s.calls(), ["gateway", "reader"]);
    assert!(withdrawals.is_empty(), "an outage is not a credential failure: {withdrawals:?}");
}

#[test]
fn a_bad_gateway_with_no_reader_is_shed_never_sent_to_the_writer() {
    let s = stub(BAD_GATEWAY, OK, OK);
    let (out, _) = s.run(false);
    assert!(matches!(out, GhCompletion::Shed { .. }), "{out:?}");
    assert_eq!(s.calls(), ["gateway"], "writer stub NOT invoked");
}

/// A 404 from GitHub through the gateway is never an outage. It is the
/// item's answer only with evidence the gateway covers the repo's family
/// (W6); without it the gateway is withdrawn for the repo, the next reader
/// serves the read, and with none left it is shed (#12394 review).
#[test]
fn a_not_found_through_the_gateway_is_not_an_outage() {
    // No evidence, no reader: shed; the gateway is withdrawn for the repo.
    let s = stub(NOT_FOUND, OK, NOT_FOUND);
    let (out, withdrawals) = s.run_on(false, "acme/gw-404-a");
    assert!(matches!(out, GhCompletion::Shed { .. }), "{out:?}");
    assert_eq!(s.calls(), ["gateway"], "writer stub NOT invoked");
    assert!(crate::forge_gateway::outage::take_test_outages().is_empty());
    assert_eq!(withdrawals, [(GATEWAY_APP_ID.to_string(), Failure::CoverageUnproven)]);

    // No evidence, a reader behind it: the reader serves the read.
    let s = stub(NOT_FOUND, OK, NOT_FOUND);
    let (out, withdrawals) = s.run_on(true, "acme/gw-404-b");
    assert!(ok(&out), "{out:?}");
    assert_eq!(s.calls(), ["gateway", "reader"]);
    assert!(crate::forge_gateway::outage::take_test_outages().is_empty());
    assert_eq!(withdrawals, [(GATEWAY_APP_ID.to_string(), Failure::CoverageUnproven)]);

    // The gateway recently served the repo's `issues`: its 404 is the answer.
    crate::forge_identity::served::note_reader_served(
        GATEWAY_APP_ID,
        "acme/gw-404-c",
        "issues",
        SystemTime::now(),
    );
    let s = stub(NOT_FOUND, OK, NOT_FOUND);
    let (out, withdrawals) = s.run_on(true, "acme/gw-404-c");
    assert!(matches!(out, GhCompletion::Captured(_)) && !ok(&out), "{out:?}");
    assert_eq!(s.calls(), ["gateway"]);
    assert!(withdrawals.is_empty(), "{withdrawals:?}");
}

/// A relayed GitHub 5xx, and a non-`api` read's bare `HTTP 500` (gh prints
/// nothing else for `pr view`), cannot be told from a gateway fault: with no
/// reader left they are shed (never rerun on the writer) and never withdraw
/// the gateway.
#[test]
fn an_unmarked_5xx_is_shed_and_does_not_withdraw() {
    const PR_VIEW_500: &str = "echo 'HTTP 500 (https://gw.example.com/graphql)' >&2; exit 1";
    for on_gw in [RELAYED_502, PR_VIEW_500] {
        let s = stub(on_gw, OK, OK);
        let (out, withdrawals) = s.run(false);
        assert!(matches!(out, GhCompletion::Shed { .. }), "{on_gw}: {out:?}");
        assert_eq!(s.calls(), ["gateway"], "{on_gw}");
        assert!(crate::forge_gateway::outage::take_test_outages().is_empty(), "{on_gw}");
        assert!(withdrawals.is_empty(), "{on_gw}: {withdrawals:?}");
    }
}

/// gh refusing the argv before sending anything is the answer: no rerun.
#[test]
fn a_gh_usage_error_is_neither_rerun_nor_an_outage() {
    let s = stub(GH_REFUSED, OK, OK);
    let (out, _) = s.run(true);
    assert!(!ok(&out));
    assert_eq!(s.calls(), ["gateway"]);
    assert!(crate::forge_gateway::outage::take_test_outages().is_empty());
}

/// #12074 review: inside a work-finder tick's read scope, a gateway that
/// keeps failing without signalling an outage (a relayed 5xx, a timeout) is
/// shed on every tick; the writer never serves it (#12394).
#[test]
fn a_scoped_read_past_a_failing_gateway_is_shed_not_sent_to_the_writer() {
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let _scope = super::super::read_deadline::enter(deadline);
    for tick in 0..2 {
        let s = stub(RELAYED_502, OK, OK);
        let (out, _) = s.run(false);
        assert!(matches!(out, GhCompletion::Shed { .. }), "tick {tick}: {out:?}");
        assert_eq!(s.calls(), ["gateway"], "tick {tick}");
    }
}

/// #12074 review: the third consecutive rerun fault withdraws the gateway,
/// so a hung gateway stops charging every read its full timeout; an answer
/// in between starts the count again.
#[test]
fn consecutive_gateway_reruns_withdraw_it() {
    use crate::forge_gateway::outage::{
        escalate, note_answered, take_test_outages, Fault, RERUNS_BEFORE_WITHDRAW,
    };
    let _ = take_test_outages();
    for _ in 1..RERUNS_BEFORE_WITHDRAW {
        assert_eq!(escalate(Fault::Rerun), Fault::Rerun);
    }
    note_answered();
    for _ in 1..RERUNS_BEFORE_WITHDRAW {
        assert_eq!(escalate(Fault::Rerun), Fault::Rerun, "an answer reset the count");
    }
    assert_eq!(escalate(Fault::Rerun), Fault::Withdraw);
    assert_eq!(escalate(Fault::Rerun), Fault::Rerun, "a withdrawal starts over");

    // Through the chain's classifier: the third timed-out gateway attempt in
    // a row withdraws it (a timeout alone used to rerun forever).
    note_answered();
    let timed_out = || {
        GhCompletion::Captured(Completion::TimedOut {
            stdout: Vec::new(),
            stderr: Vec::new(),
        })
    };
    for attempt in 1..=RERUNS_BEFORE_WITHDRAW {
        let expected = if attempt < RERUNS_BEFORE_WITHDRAW {
            Fault::Rerun
        } else {
            Fault::Withdraw
        };
        assert_eq!(super::gateway_fault(&timed_out()), expected, "attempt {attempt}");
    }
}

/// #12124: the parsed HTTP status decides whether the gateway answered, ahead
/// of the exit code, so an exit-0 run carrying a 5xx never resets the count.
#[test]
fn exit_zero_with_parsed_5xx_is_not_an_answer() {
    use crate::forge_gateway::outage::{answered, escalate, note_answered, Fault};
    use std::os::unix::process::ExitStatusExt;
    let run = |code: i32, stdout: &str, stderr: &str| std::process::Output {
        status: std::process::ExitStatus::from_raw(code << 8),
        stdout: stdout.as_bytes().to_vec(),
        stderr: stderr.as_bytes().to_vec(),
    };
    assert!(!answered(&run(0, "HTTP/2.0 502 Bad Gateway\r\n\r\n", "")));
    assert!(!answered(&run(0, "", "gh: Bad Gateway (HTTP 502)")));
    assert!(answered(&run(0, "HTTP/2.0 304 Not Modified\r\n\r\n", "")));
    assert!(answered(&run(0, "", "")));
    assert!(!answered(&run(1, "", "")));

    // The count survives an exit-0 502 and still withdraws on the third rerun.
    note_answered();
    assert_eq!(escalate(Fault::Rerun), Fault::Rerun);
    assert_eq!(escalate(Fault::Rerun), Fault::Rerun);
    let masked = run(0, "HTTP/2.0 502 Bad Gateway\r\n\r\n", "");
    if answered(&masked) {
        note_answered();
    }
    assert_eq!(escalate(Fault::Rerun), Fault::Withdraw);
}
