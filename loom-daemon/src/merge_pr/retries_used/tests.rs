//! Unit tests for the retries-used extraction. The differential
//! (`tests/merge_pr_retries_used_differential.rs`) compares against the frozen
//! retired shell; these name the individual properties.

use super::*;

#[test]
fn the_resolved_attempt_is_read_from_the_reason() {
    let r = "cached mergeable=false was stale; recheck #2 (post-backoff, uncached) now reports mergeable=true";
    assert_eq!(retries_used(r, "3"), "2");
}

#[test]
fn every_other_path_reports_the_configured_budget() {
    let r = "forge reports mergeable=false after 3 recheck(s); base/head ref unavailable";
    assert_eq!(retries_used(r, "3"), "3");
    assert_eq!(retries_used("", "5"), "5");
}

#[test]
fn the_leftmost_occurrence_followed_by_a_digit_wins() {
    assert_eq!(retries_used("recheck #x recheck #7 recheck #9", "3"), "7");
    assert_eq!(retries_used("recheck # recheck #12", "3"), "12");
}

#[test]
fn the_digit_run_is_greedy_and_verbatim() {
    assert_eq!(retries_used("recheck #007z", "3"), "007");
    assert_eq!(retries_used("recheck #99999999999999999999", "3"), "99999999999999999999");
}

#[test]
fn non_ascii_digits_do_not_match() {
    assert_eq!(retries_used("recheck #\u{0663}", "3"), "3");
}
