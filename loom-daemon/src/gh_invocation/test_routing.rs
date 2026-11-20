//! Test seam (W4-C): run [`GhInvocation::execute`] against a fixed router on
//! THIS thread, with no real `gh` reachable. A site's own helper (which
//! resolves `gh` itself) can then be driven through derivation, the
//! class-aware chain and the shed mapping without a network or a stub on
//! `PATH`. An attempt reaches a test-owned executable or a nonexistent
//! program, never a real forge command.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};

use super::cwd_route::{CwdAnswer, DeriveEnv};
use super::{GhCompletion, GhInvocation};
use crate::forge_identity::{ReadClass, RouteDecision};
use crate::proc_exec::ExecError;

/// The fixed routing a test installs.
#[derive(Debug, Clone)]
pub(crate) struct TestRouting {
    pub(crate) decision: RouteDecision,
    pub(crate) env: DeriveEnv,
    pub(crate) checkout: CwdAnswer,
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
    static PROGRAM: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    static SEEN: RefCell<Vec<Seen>> = const { RefCell::new(Vec::new()) };
    static SHEDS: Cell<u64> = const { Cell::new(0) };
}

/// Installs `routing` for this thread until dropped.
pub(crate) struct Guard;

impl Guard {
    /// Use the caller's inert Forge fixture for requests the test router admits.
    #[must_use]
    pub(crate) fn with_program(self, program: &Path) -> Self {
        PROGRAM.with(|p| *p.borrow_mut() = Some(program.to_path_buf()));
        self
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        ROUTING.with(|r| *r.borrow_mut() = None);
        PROGRAM.with(|p| *p.borrow_mut() = None);
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

/// Exercise a real caller with a fake forge response under a healthy reader and
/// with no subprocess at all under an exhausted reader. The checkout is not
/// usable for derivation, so the caller has to supply its actual target.
pub(crate) fn assert_pooled_query(
    repo: &str,
    response: &serde_json::Value,
    query: impl Fn(&Path) -> bool,
) {
    use crate::forge_identity::{ExhaustCause, Placement};
    use std::time::{Duration, SystemTime};

    let tmp = tempfile::tempdir().unwrap();
    let reader = tmp.path().join("reader");
    std::fs::create_dir(&reader).unwrap();
    let calls = tmp.path().join("calls");
    let reply = tmp.path().join("reply");
    std::fs::write(&reply, response.to_string()).unwrap();
    let program = super::test_support::stub(
        tmp.path(),
        "gh-graphql",
        &format!(
            r#"printf '%s\n' "${{GH_CONFIG_DIR:-writer}}" >> '{}'
test "$1" = api && test "$2" = graphql || exit 64
cat '{}'"#,
            calls.display(),
            reply.display()
        ),
    );
    let routing = |decision| TestRouting {
        decision,
        env: DeriveEnv::default(),
        checkout: CwdAnswer::Unresolved,
    };
    {
        let _guard = install(routing(RouteDecision::Reader {
            dir: reader.clone(),
            app_id: "fixture-reader".into(),
            placement: Placement::Home,
        }));
        PROGRAM.with(|p| *p.borrow_mut() = Some(program.clone()));
        assert!(query(tmp.path()), "the healthy reader must return its response");
        let attempts = seen();
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].route_slug.as_deref(), Some(repo));
        assert!(!attempts[0].shed);
    }
    {
        let _guard = install(routing(RouteDecision::Exhausted {
            until: SystemTime::now() + Duration::from_secs(60),
            cause: ExhaustCause::Budget,
        }));
        PROGRAM.with(|p| *p.borrow_mut() = Some(program));
        assert!(!query(tmp.path()), "an exhausted reader must shed");
        let attempts = seen();
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].route_slug.as_deref(), Some(repo));
        assert!(attempts[0].shed);
    }
    let calls = std::fs::read_to_string(calls).unwrap();
    assert_eq!(calls.lines().collect::<Vec<_>>(), [reader.to_str().unwrap()]);
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
    inv.program = Some(PROGRAM.with(|p| {
        p.borrow().as_ref().map_or_else(
            || "/nonexistent/loom-w4c-test-gh".to_string(),
            |path| path.to_string_lossy().into_owned(),
        )
    }));
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
