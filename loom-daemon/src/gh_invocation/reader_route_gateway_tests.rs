//! 2am#4367 P4: a gateway outage falls back to the readers, then the writer.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::super::{AccessIntent, GhCompletion, GhInvocation, GhTarget, Operation, ParentContext};
use super::v2::ShedPolicy;
use crate::forge_gateway::GATEWAY_APP_ID;
use crate::forge_identity::{Failure, Placement, ReadClass, RouteDecision, RouteRequest};
use crate::proc_exec::Completion;
use std::cell::RefCell;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

const DOWN: &str =
    "echo 'Get \"https://gw.example.com/repos/acme/r\": dial tcp: connection refused' >&2; exit 1";
const BAD_GATEWAY: &str = "echo 'gh: Bad Gateway (HTTP 502)' >&2; exit 1";
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
            GhTarget::repo("acme/r").unwrap(),
            Duration::from_secs(10),
        )
        .parent(ParentContext::Missing)
        .program(&self.gh)
        .args(["api", "repos/acme/r/issues/1"])
        .read_class(ReadClass::Gate);
        let out = inv
            .execute_routed_v2_at(&lookup, &withdraw, ShedPolicy { shed: true }, SystemTime::now())
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
fn a_bad_gateway_with_no_reader_falls_back_to_the_writer() {
    let s = stub(BAD_GATEWAY, OK, OK);
    let (out, _) = s.run(false);
    assert!(ok(&out), "{out:?}");
    assert_eq!(s.calls(), ["gateway", "writer"]);
}

/// A real answer from GitHub through the gateway keeps today's rules: a 404
/// is confirmed on the writer, never treated as an outage.
#[test]
fn a_not_found_through_the_gateway_is_not_an_outage() {
    let s = stub(NOT_FOUND, OK, NOT_FOUND);
    let (_, withdrawals) = s.run(false);
    assert_eq!(s.calls(), ["gateway", "writer"]);
    assert!(crate::forge_gateway::outage::take_test_outages().is_empty());
    assert!(withdrawals.is_empty(), "{withdrawals:?}");
}
