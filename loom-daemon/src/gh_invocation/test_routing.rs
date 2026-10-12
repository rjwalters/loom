//! Test seam (W4-C): run [`GhInvocation::execute`] against a fixed router on
//! THIS thread, with no real `gh` reachable. A site's own helper (which
//! resolves `gh` itself) can then be driven through derivation, the
//! class-aware chain and the shed mapping without a network or a stub on
//! `PATH`. Any attempt that does reach a process runs a program that does
//! not exist, so it surfaces as `Unavailable::Spawn`, never as a forge call.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::cell::{Cell, RefCell};

use super::cwd_route::{CwdAnswer, DeriveEnv};
use super::reader_route::ShedPolicy;
use super::{GhCompletion, GhInvocation};
use crate::forge_identity::{ReadClass, RouteDecision};
use crate::proc_exec::ExecError;

/// The fixed routing a test installs.
#[derive(Debug, Clone)]
pub(crate) struct TestRouting {
    pub(crate) decision: RouteDecision,
    pub(crate) env: DeriveEnv,
    pub(crate) checkout: CwdAnswer,
    pub(crate) shed: bool,
}

/// One routed execution, as the seam saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Seen {
    pub(crate) op: &'static str,
    pub(crate) class: ReadClass,
    pub(crate) route_slug: Option<String>,
    pub(crate) shed: bool,
}

thread_local! {
    static ROUTING: RefCell<Option<TestRouting>> = const { RefCell::new(None) };
    static SEEN: RefCell<Vec<Seen>> = const { RefCell::new(Vec::new()) };
    static SHEDS: Cell<u64> = const { Cell::new(0) };
}

/// Installs `routing` for this thread until dropped.
pub(crate) struct Guard;

impl Drop for Guard {
    fn drop(&mut self) {
        ROUTING.with(|r| *r.borrow_mut() = None);
        SEEN.with(|s| s.borrow_mut().clear());
        SHEDS.with(|c| c.set(0));
    }
}

#[must_use]
pub(crate) fn install(routing: TestRouting) -> Guard {
    ROUTING.with(|r| *r.borrow_mut() = Some(routing));
    SEEN.with(|s| s.borrow_mut().clear());
    SHEDS.with(|c| c.set(0));
    Guard
}

/// Every execution routed through the seam since [`install`].
pub(crate) fn seen() -> Vec<Seen> {
    SEEN.with(|s| s.borrow().clone())
}

/// Count one shed (called by the chain under `cfg(test)`).
pub(crate) fn note_shed() {
    SHEDS.with(|c| c.set(c.get() + 1));
}

/// Sheds since [`install`].
pub(crate) fn sheds() -> u64 {
    SHEDS.with(Cell::get)
}

/// `Some` when a routing is installed on this thread.
pub(super) fn run(inv: &GhInvocation) -> Option<Result<GhCompletion, ExecError>> {
    let routing = ROUTING.with(|r| r.borrow().clone())?;
    let mut inv = inv.clone();
    inv.program = Some("/nonexistent/loom-w4c-test-gh".to_string());
    let checkout = routing.checkout.clone();
    let inv = inv.with_derived_route_in(
        &routing.env,
        &move |_: &std::path::Path, _: crate::forge_repo_facts::GhRepoEnv| checkout.clone(),
    );
    let before = sheds();
    let op = inv.operation.as_str();
    let class = inv.read_class;
    let route_slug = inv.route_slug.clone();
    let decision = routing.decision.clone();
    let result = inv.execute_routed_v2(
        &move |_: &crate::forge_identity::RouteRequest<'_>| decision.clone(),
        &|_: &str, _: &str, _: crate::forge_identity::Failure, _: &str| {},
        ShedPolicy { shed: routing.shed },
    );
    SEEN.with(|s| {
        s.borrow_mut().push(Seen {
            op,
            class,
            route_slug,
            shed: sheds() > before,
        });
    });
    Some(result)
}
