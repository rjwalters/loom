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

#[test]
fn override_comment_renders_the_byte_frozen_shape() {
    let body = override_comment("42", "deadbeef", "loom:review-requested", "2026-09-30T12:00:00Z");
    assert!(body.starts_with("## Merge Proceeded Without `loom:pr` (Override)"));
    assert!(body.contains("PR #42 was merged via `merge-pr.sh --allow-unapproved`"));
    assert!(body.contains("- **Head SHA**: `deadbeef`"));
    assert!(body.contains("- **Labels at merge time**: loom:review-requested"));
    assert!(body.contains("(#7419)"));
    assert!(body.ends_with("*Recorded by merge-pr.sh at 2026-09-30T12:00:00Z*"));
    assert!(
        !body.ends_with('\n'),
        "the retired body ended at its closing quote, not a newline"
    );
}

#[test]
fn override_comment_empty_labels_renders_the_none_placeholder_unlike_whitespace() {
    // `${PR_LABELS:-<none>}`: only the EMPTY string substitutes. This is
    // deliberately NOT `shown()`'s trimmed behaviour (see the doc comment on
    // `override_comment`) — a whitespace-only label line is not empty here.
    let empty = override_comment("1", "sha", "", "TS");
    assert!(empty.contains("- **Labels at merge time**: <none>"));
    let whitespace = override_comment("1", "sha", "   ", "TS");
    assert!(
        whitespace.contains("- **Labels at merge time**:    "),
        "whitespace-only labels must render verbatim, not as <none>: {whitespace:?}"
    );
}

#[test]
fn override_comment_preserves_embedded_newlines_in_a_multi_label_set() {
    // `$PR_LABELS` is newline-joined (`jq -r '.labels[]?.name'`); the retired
    // `${PR_LABELS:-<none>}` interpolated that verbatim, embedded newlines and
    // all.
    let body = override_comment("1", "sha", "loom:pr\nloom:operator", "TS");
    assert!(body.contains("- **Labels at merge time**: loom:pr\nloom:operator"));
}
