//! Unit tests for the merge-retry route. The differential
//! (`tests/merge_pr_merge_route_differential.rs`) compares against the frozen
//! retired loop body; these name the individual properties.

use super::*;

fn inp(kind: Kind, attempt: i64, max: i64, delay: i64) -> Inputs<'static> {
    Inputs {
        kind,
        pr: "42",
        attempt,
        max,
        delay,
        response: b"HTTP 409: something",
    }
}

#[test]
fn base_modified_syncs_only_while_budget_remains() {
    assert!(matches!(decide(&inp(Kind::BaseModified, 1, 3, 5)), Route::Sync { .. }));
    assert!(matches!(decide(&inp(Kind::BaseModified, 2, 3, 10)), Route::Sync { .. }));
    // The last attempt does not sync: there would be no attempt left to use it.
    assert!(matches!(decide(&inp(Kind::BaseModified, 3, 3, 20)), Route::Fail { .. }));
}

#[test]
fn sync_sleeps_the_current_delay_and_doubles_it_for_next_time() {
    let Route::Sync {
        sleep,
        next_delay,
        wait,
        ..
    } = decide(&inp(Kind::BaseModified, 1, 3, 5))
    else {
        panic!("expected Sync");
    };
    assert_eq!((sleep, next_delay), (5, 10));
    assert_eq!(wait, "Waiting 5s for branch to sync...");
}

#[test]
fn backoff_wraps_like_bash_arithmetic() {
    let Route::Sync { next_delay, .. } = decide(&inp(Kind::BaseModified, 1, 3, i64::MAX)) else {
        panic!("expected Sync");
    };
    assert_eq!(next_delay, i64::MAX.wrapping_mul(2));
}

#[test]
fn a_head_mismatch_is_never_retried() {
    // #5579: a head that moved past the approved SHA must never be synced and
    // retried — that would merge a diff no Judge approved.
    for attempt in 1..=3 {
        assert!(matches!(decide(&inp(Kind::HeadMismatch, attempt, 3, 5)), Route::Fail { .. }));
    }
}

#[test]
fn other_quotes_the_response_bytes_verbatim() {
    let i = Inputs {
        kind: Kind::Other,
        pr: "7",
        attempt: 1,
        max: 3,
        delay: 5,
        response: b"line one\n\xff not utf-8",
    };
    assert_eq!(
        decide(&i).render(),
        b"LOOM-MERGE-ROUTE FAIL\nFailed to merge PR #7: line one\n\xff not utf-8\n".to_vec()
    );
}

#[test]
fn merge_in_progress_awaits_five_seconds_on_any_attempt() {
    for attempt in [1, 3, 9] {
        let r = decide(&inp(Kind::MergeInProgress, attempt, 3, 5));
        assert!(matches!(r, Route::Await { sleep: 5, .. }), "{r:?}");
    }
}

#[test]
fn render_keys_each_line() {
    let out = String::from_utf8(decide(&inp(Kind::BaseModified, 2, 3, 10)).render()).unwrap();
    assert_eq!(
        out,
        "LOOM-MERGE-ROUTE SYNC 10 20\n\
         BEFORE\tBranch is behind base branch, updating... (attempt 2/3)\n\
         WAIT\tWaiting 10s for branch to sync...\n\
         REWORK\tbase branch was modified; synced before merge retry 2/3\n"
    );
}
