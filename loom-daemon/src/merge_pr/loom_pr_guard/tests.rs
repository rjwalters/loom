//! Tests for the `loom:pr` review-signal guard.

use super::*;

#[test]
fn loom_pr_present_is_approved() {
    assert_eq!(assess("999", "loom:pr", "deadbeef", false), Verdict::Approved);
    assert_eq!(
        assess("999", "loom:review-requested\nloom:pr", "deadbeef", false),
        Verdict::Approved
    );
}

#[test]
fn loom_pr_present_is_approved_even_with_allow_unapproved_set() {
    // Approval short-circuits before the override branch is even considered —
    // --allow-unapproved is about a MISSING label, not a present one.
    assert_eq!(assess("999", "loom:pr", "deadbeef", true), Verdict::Approved);
}

#[test]
fn missing_loom_pr_without_override_blocks() {
    let v = assess("999", "loom:review-requested\nloom:operator", "deadbeef", false);
    match v {
        Verdict::Blocked(msg) => {
            assert!(msg.contains("Merge blocked"));
            assert!(msg.contains("loom:review-requested"));
            assert!(msg.contains("deadbeef"));
            assert!(msg.contains("--allow-unapproved"));
        }
        other => panic!("expected Blocked, got {other:?}"),
    }
}

#[test]
fn missing_loom_pr_with_override_is_overridden_not_blocked() {
    let v = assess("999", "loom:review-requested\nloom:operator", "deadbeef", true);
    match v {
        Verdict::Overridden(msg) => {
            assert!(msg.contains("--allow-unapproved set"));
            assert!(msg.contains("loom:review-requested"));
            assert!(msg.contains("deadbeef"));
            assert!(!msg.contains("Merge blocked"));
        }
        other => panic!("expected Overridden, got {other:?}"),
    }
}

#[test]
fn empty_label_set_renders_the_none_placeholder() {
    let v = assess("999", "", "deadbeef", false);
    match v {
        Verdict::Blocked(msg) => assert!(msg.contains("<none>")),
        other => panic!("expected Blocked, got {other:?}"),
    }
    let v = assess("999", "   \n  ", "deadbeef", true);
    match v {
        Verdict::Overridden(msg) => assert!(msg.contains("<none>")),
        other => panic!("expected Overridden, got {other:?}"),
    }
}

#[test]
fn a_substring_match_does_not_count_as_loom_pr() {
    // Whole-line match only, mirroring the shell's `grep -qx`.
    assert!(matches!(assess("1", "loom:private", "sha", false), Verdict::Blocked(_)));
    assert!(matches!(assess("1", "not-loom:pr", "sha", false), Verdict::Blocked(_)));
    assert!(matches!(assess("1", "loom:pr-extra", "sha", false), Verdict::Blocked(_)));
}

#[test]
fn leading_and_trailing_whitespace_on_a_label_line_still_matches() {
    assert_eq!(assess("1", "  loom:pr  ", "sha", false), Verdict::Approved);
}
