//! Dispatch **park-guard** test for the `loom:operator` hold (vibesql#6664).
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

/// AC (vibesql#6664): a generic `loom:operator` hold must NOT refuse dispatch
/// by itself. The hold's automation-stop effect is expressed in the work
/// finder's candidate filter (SKIP_LABELS via OPERATOR_HOLD_LABEL), not in
/// the park set this guard consults — the re-evaluation routes (watchdog
/// re-dispatch, reaper checkpoint-resume, an operator's explicit
/// `loom-daemon dispatch <N>` override) must keep reaching held items per
/// `label-state-machine.md`'s re-evaluability contract. If this test starts
/// failing because `loom:operator` graduated into PARK_LABELS, that change
/// must also answer how a human overrides their own hold.
#[test]
#[serial]
fn dispatch_park_guard_allows_operator_hold_alone() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    std::env::set_var("LOOM_REPO", "rjwalters/loom");
    let (mut reg, gh_log) = park_guard_registry(ws, "loom:operator loom:curated", 0, "", false);

    let out = reg
        .dispatch(&SweepKind::Issue(6172), None, None, None, None)
        .expect("a generic loom:operator hold alone must never refuse dispatch");
    assert!(wait_until_dead(out.pid, FIXTURE_CHILD_WAIT_MS));

    let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        calls.contains("api repos/rjwalters/loom/issues/6172 --jq .labels[].name"),
        "the guard still probed; it just did not refuse; got: {calls:?}"
    );
    assert!(
        calls.contains("issue edit 6172"),
        "dispatch proceeded to the label flip; got: {calls:?}"
    );
    std::env::remove_var("LOOM_REPO");
}
