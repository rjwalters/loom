//! Dispatch **park-guard** tests for the holds: the `loom:operator` hold (a
//! park since #11795), the `loom:epic` tracking container (a skip, not a
//! park, #11399) and `loom:frozen` (#11743).
//!
//! Split out as a sibling file rather than appended to `dispatch/tests.rs`:
//! that module is over the 1000-line ratchet threshold and frozen at its
//! current size (see `.loom/docs/file-size-policy.md`). Registered from its
//! foot via `#[path]`, the shape `guards.rs` already uses for
//! `guards_union_tests.rs` / `guards_preflip_tests.rs`.

use super::*;
use crate::sweep_registry::test_support::*;
use serial_test::serial;
use tempfile::tempdir;

/// #11795: `loom:operator` is an issue-dispatch park. The dispatch-time guard
/// every route goes through (work finder, watchdog re-dispatch, checkpoint
/// resume, explicit `loom-daemon dispatch <N>`, the epic supervisor) refuses
/// it with no label write, so a hold applied after dispatch is never
/// overridden (#4366). A human overrides by removing the label. An unmigrated
/// item carrying a retired name is refused the same way, under that name.
#[test]
#[serial]
fn dispatch_park_guard_refuses_the_operator_hold_under_any_name() {
    for (labels, want) in [
        ("loom:operator loom:curated", "loom:operator"),
        ("loom:issue loom:operator", "loom:operator"),
        ("loom:issue loom:operator-only", "loom:operator-only"),
        ("loom:issue loom:operator-decision", "loom:operator-decision"),
    ] {
        let tmp = tempdir().unwrap();
        std::env::set_var("LOOM_REPO", "rjwalters/loom");
        let (mut reg, gh_log) = park_guard_registry(tmp.path(), labels, 0, "", false);
        let err = reg
            .dispatch(&SweepKind::Issue(6172), None, None, None, None)
            .expect_err("an operator hold must be refused");
        let typed = err
            .downcast_ref::<ParkedIssueDispatchError>()
            .expect("refusal must carry the typed ParkedIssueDispatchError");
        assert_eq!(typed.label, want, "{labels}");
        let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(!calls.contains("issue edit"), "{labels}: {calls}");
        std::env::remove_var("LOOM_REPO");
    }
}

/// AC (#11399): `loom:epic` stops the work finder from listing an epic as a
/// candidate (it is in SKIP_LABELS), but it is not a park. An operator's
/// explicit `loom-daemon dispatch <N>` / `dispatch_sweep` on an epic is their
/// call, so this guard must let it through even when the epic also carries
/// `loom:issue`.
#[test]
#[serial]
fn dispatch_park_guard_allows_an_explicit_dispatch_on_an_epic() {
    assert!(crate::work_finder::SKIP_LABELS.contains(&"loom:epic"));
    assert!(!crate::work_finder::PARK_LABELS.contains(&"loom:epic"));

    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    std::env::set_var("LOOM_REPO", "rjwalters/loom");
    let (mut reg, gh_log) = park_guard_registry(ws, "loom:epic loom:issue", 0, "", false);

    let out = reg
        .dispatch(&SweepKind::Issue(11399), None, None, None, None)
        .expect("`loom:epic` is a skip, not a park: an explicit dispatch must not be refused");
    assert!(wait_until_dead(out.pid, FIXTURE_CHILD_WAIT_MS));

    let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        calls.contains("api repos/rjwalters/loom/issues/11399 --jq .labels[].name"),
        "the guard probed the labels and did not refuse; got: {calls:?}"
    );
    assert!(
        calls.contains("issue edit 11399"),
        "dispatch proceeded to the label flip; got: {calls:?}"
    );
    std::env::remove_var("LOOM_REPO");
}

/// #11743: `loom:frozen` is a park, so the dispatch-time park guard — the one
/// check every route goes through (work finder, watchdog re-dispatch,
/// checkpoint-resume, explicit `loom-daemon dispatch <N>`, the epic
/// supervisor's child dispatch) — refuses it with no label write, alone or
/// beside `loom:issue`. `loom:frozen-x` is not frozen (exact match, #11366).
#[test]
#[serial]
fn dispatch_refuses_a_frozen_issue_on_the_shared_park_guard() {
    for labels in ["loom:frozen", "loom:issue loom:frozen"] {
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        std::env::set_var("LOOM_REPO", "rjwalters/loom");
        let (mut reg, gh_log) = park_guard_registry(ws, labels, 0, "", false);

        let err = reg
            .dispatch(&SweepKind::Issue(11743), None, None, None, None)
            .expect_err("a frozen issue must be refused");
        let typed = err
            .downcast_ref::<ParkedIssueDispatchError>()
            .expect("refusal must carry the typed ParkedIssueDispatchError");
        assert_eq!(typed.label, "loom:frozen", "{labels}");
        let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(!calls.contains("issue edit"), "{labels}: {calls}");
        std::env::remove_var("LOOM_REPO");
    }
    let tmp = tempdir().unwrap();
    std::env::set_var("LOOM_REPO", "rjwalters/loom");
    let (mut reg, _) = park_guard_registry(tmp.path(), "loom:issue loom:frozen-x", 0, "", false);
    let refused = reg
        .dispatch(&SweepKind::Issue(11744), None, None, None, None)
        .err()
        .and_then(|e| {
            e.downcast_ref::<ParkedIssueDispatchError>()
                .map(|t| t.label.clone())
        });
    assert_eq!(refused, None, "loom:frozen-x is not a park");
    std::env::remove_var("LOOM_REPO");
}
