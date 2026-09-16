//! Tests for the four-way decision (epic #7810, PR 4).

use super::*;

const H: &str = "abc1234567890def";
const W: u64 = DEFAULT_HEARTBEAT_HOURS;

#[test]
fn an_empty_hash_is_none_and_claims_nothing() {
    // operator-premise with VERDICT=open: nothing to report this pass.
    let a = decide("", "", 0, W);
    assert_eq!(a, Action::None);
    assert!(!a.claims());
}

#[test]
fn an_empty_hash_is_none_even_when_a_prior_marker_exists() {
    // "Nothing concluded" is not "the conclusion cleared". A prior marker does
    // not turn an absent conclusion into a reportable one.
    assert_eq!(decide("", H, 999, W), Action::None);
}

#[test]
fn the_first_ever_check_always_comments() {
    let a = decide(H, "", 0, W);
    assert_eq!(a, Action::Comment);
    assert!(a.claims());
}

#[test]
fn a_changed_conclusion_comments_regardless_of_the_window() {
    // Window irrelevant: a changed conclusion is never suppressed. This is
    // also what carries the #6516 orthogonal-blocker escalation, which changes
    // the hash by construction.
    assert_eq!(decide(H, "different", 0, W), Action::Comment);
    assert_eq!(decide(H, "different", 1000, W), Action::Comment);
}

#[test]
fn an_unchanged_conclusion_inside_the_window_skips_and_takes_no_claim() {
    // #7617: the no-op path must not take `loom:curating`. Claiming to
    // discover there is nothing to say is the cost this branch exists to avoid.
    let a = decide(H, H, W - 1, W);
    assert_eq!(a, Action::Skip);
    assert!(!a.claims());
}

#[test]
fn the_window_boundary_is_at_or_past_not_strictly_past() {
    // `(( PRIOR_AGE_HOURS < heartbeat_hours ))` — so equality heartbeats.
    assert_eq!(decide(H, H, W - 1, W), Action::Skip);
    assert_eq!(decide(H, H, W, W), Action::Heartbeat);
    assert_eq!(decide(H, H, W + 1, W), Action::Heartbeat);
}

#[test]
fn a_heartbeat_claims() {
    assert!(decide(H, H, W, W).claims());
}

#[test]
fn a_zero_window_heartbeats_immediately() {
    assert_eq!(decide(H, H, 0, 0), Action::Heartbeat);
}

#[test]
fn the_rendered_tokens_are_the_contract_curator_md_parses() {
    assert_eq!(Action::None.as_str(), "none");
    assert_eq!(Action::Skip.as_str(), "skip");
    assert_eq!(Action::Comment.as_str(), "comment");
    assert_eq!(Action::Heartbeat.as_str(), "heartbeat");
}

#[test]
fn exactly_the_two_posting_actions_claim() {
    // The claim rule is "posting requires a claim", so it must track which
    // actions post — not be maintained as a separate list that can drift.
    for (a, claims) in [
        (Action::None, false),
        (Action::Skip, false),
        (Action::Comment, true),
        (Action::Heartbeat, true),
    ] {
        assert_eq!(a.claims(), claims, "{}", a.as_str());
    }
}
