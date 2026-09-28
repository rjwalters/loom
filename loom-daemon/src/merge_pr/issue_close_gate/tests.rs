//! Tests for the async-close-race worktree-cleanup gate (#4186).

use super::*;

#[test]
fn a_close_target_needs_no_live_state() {
    // The fast path: no `state` supplied at all, and the answer is already
    // determined from `close_targets` alone — no `NeedState` round trip.
    assert_eq!(decide("42", "42", None), Decision::CloseTarget);
    assert_eq!(decide("7\n42\n100", "42", None), Decision::CloseTarget);
}

#[test]
fn membership_is_an_exact_line_match() {
    // Not a substring match: "142" must never match issue "42", and a
    // partial-increment reference formatted differently must never match
    // either. This is the same trap #5234's declaration-vs-mention guard
    // exists for elsewhere in this port.
    assert_ne!(decide("142", "42", None), Decision::CloseTarget);
    assert_ne!(decide("4\n2", "42", None), Decision::CloseTarget);
    // Also not a prefix/suffix-of-line match.
    assert_ne!(decide("42x", "42", None), Decision::CloseTarget);
    assert_ne!(decide("x42", "42", None), Decision::CloseTarget);
}

#[test]
fn empty_close_targets_never_matches() {
    assert_eq!(decide("", "42", None), Decision::NeedState);
}

#[test]
fn not_a_close_target_with_no_state_needs_one() {
    // The caller has not fetched `forge_get_issue_state` yet — the verb
    // cannot answer without it, and must say so distinctly from `Preserve`
    // so the shell knows to make the second call rather than assume "no".
    assert_eq!(decide("7\n100", "42", None), Decision::NeedState);
}

#[test]
fn a_closed_live_state_authorizes_cleanup() {
    assert_eq!(decide("7\n100", "42", Some("CLOSED")), Decision::StateClosed);
}

#[test]
fn an_open_live_state_preserves() {
    assert_eq!(decide("7\n100", "42", Some("OPEN")), Decision::Preserve);
}

#[test]
fn a_failed_lookups_empty_state_preserves() {
    // forge_get_issue_state prints nothing on any lookup failure or an
    // unrecognized value — never guess "closed" from that.
    assert_eq!(decide("7\n100", "42", Some("")), Decision::Preserve);
}

#[test]
fn an_unrecognized_state_value_preserves() {
    // Anything that is not the literal "CLOSED" preserves — including a
    // lowercase or otherwise-shaped value forge_get_issue_state itself would
    // never emit (it only ever prints "OPEN"/"CLOSED"/nothing), so a future
    // caller that skips that normalization still fails toward preservation.
    assert_eq!(decide("7\n100", "42", Some("closed")), Decision::Preserve);
    assert_eq!(decide("7\n100", "42", Some("unknown")), Decision::Preserve);
}

#[test]
fn close_target_wins_even_when_a_state_is_also_supplied() {
    // Membership is checked first and is sufficient on its own — a caller
    // that (incorrectly) supplied a stale/wrong `--state` alongside a genuine
    // close target must not have that override the no-race answer.
    assert_eq!(decide("42", "42", Some("OPEN")), Decision::CloseTarget);
}

#[test]
fn is_closed_for_cleanup_matches_the_boolean_the_shell_wants() {
    assert!(Decision::CloseTarget.is_closed_for_cleanup());
    assert!(Decision::StateClosed.is_closed_for_cleanup());
    assert!(!Decision::NeedState.is_closed_for_cleanup());
    assert!(!Decision::Preserve.is_closed_for_cleanup());
}

#[test]
fn tokens_are_stable() {
    assert_eq!(Decision::CloseTarget.token(), "CLOSE-TARGET");
    assert_eq!(Decision::StateClosed.token(), "STATE-CLOSED");
    assert_eq!(Decision::NeedState.token(), "NEED-STATE");
    assert_eq!(Decision::Preserve.token(), "PRESERVE");
}
