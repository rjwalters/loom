//! Tests for the `champion:hold-state` staleness warning.
//!
//! The T-numbered cases mirror `test-merge-pr-loom-pr-label-guard.sh`'s own
//! hold-state assertions (T6/T7/T8/T10/T12), which still run against this code
//! through the shell wrapper. They are duplicated here so a Rust-only change
//! that breaks them fails without a shell suite in the loop.

use super::*;

// --- the retained suite's cases, as unit tests -----------------------------

#[test]
fn t6_a_marker_naming_a_different_head_warns() {
    let comments = "<!-- champion:hold-state head=abc1234 -->\nSome other hold-state prose.";
    let msg = assess("999", comments, "deadbeef").expect("stale marker must warn");
    assert!(msg.contains("champion:hold-state marker recorded head=abc1234"));
    assert!(msg.contains("deadbeef"));
    assert!(msg.contains("PR #999's current head"));
}

#[test]
fn t7_a_marker_naming_the_current_head_is_silent() {
    let comments = "<!-- champion:hold-state head=deadbeef -->\nHeld for review.";
    assert_eq!(assess("999", comments, "deadbeef"), None);
}

#[test]
fn t8_no_marker_at_all_is_silent() {
    assert_eq!(
        assess("999", "Just a regular Judge approval comment, no marker here.", "deadbeef"),
        None
    );
}

#[test]
fn empty_comments_are_silent() {
    assert_eq!(assess("999", "", "deadbeef"), None);
    assert_eq!(recorded_head(""), None);
}

// --- "most recent wins" ----------------------------------------------------

#[test]
fn the_last_qualifying_marker_wins() {
    // Comment bodies arrive concatenated in chronological order, so the last
    // marker is the most recent hold episode.
    let comments = "<!-- champion:hold-state head=aaaa1111 -->\nfirst hold\n\
                    <!-- champion:hold-state head=bbbb2222 -->\nsecond hold\n";
    assert_eq!(recorded_head(comments).as_deref(), Some("bbbb2222"));
}

#[test]
fn two_markers_on_one_line_take_the_later_one() {
    let line =
        "<!-- champion:hold-state head=aaaa1111 --> <!-- champion:hold-state head=bbbb2222 -->";
    assert_eq!(recorded_head(line).as_deref(), Some("bbbb2222"));
}

// --- defect 1: an empty capture must not mask a real marker ----------------

#[test]
fn a_documentation_line_with_a_placeholder_sha_does_not_erase_a_real_hold() {
    // The retired shell's `[0-9a-f]*` matched `head=` with nothing after it,
    // and `tail -1` then handed that empty capture to the caller — so ONE
    // comment quoting champion-pr-merge.md's own template silently disabled
    // the staleness check for the whole PR.
    let comments = "<!-- champion:hold-state head=abc1234 -->\nHolding.\n\
                    Later comment: the hold records <!-- champion:hold-state head=<sha> -->\n";
    assert_eq!(recorded_head(comments).as_deref(), Some("abc1234"));
    assert!(assess("999", comments, "deadbeef").is_some());
}

#[test]
fn a_placeholder_only_stream_records_nothing() {
    let comments = "<!-- champion:hold-state head=<sha> -->\n";
    assert_eq!(recorded_head(comments), None);
}

// --- defect 2: only an HTML-comment-delimited marker is authoritative ------

#[test]
fn a_prose_mention_is_not_a_recorded_head() {
    let comments = "Champion writes champion:hold-state head=abc1234 into the hold notice.\n";
    assert_eq!(recorded_head(comments), None);
}

#[test]
fn a_backticked_mention_is_not_a_recorded_head() {
    let comments = "See the `champion:hold-state head=abc1234` marker for how this works.\n";
    assert_eq!(recorded_head(comments), None);
}

#[test]
fn an_unterminated_html_comment_does_not_reach_across_lines() {
    // Line-local by construction: the `-->` on a LATER line belongs to a
    // different comment body as far as this scan is concerned. Without that,
    // one malformed comment changes how every later one reads.
    let comments = "<!-- champion:hold-state head=abc1234\n-->\n";
    assert_eq!(recorded_head(comments), None);
}

#[test]
fn a_prose_mention_does_not_outrank_an_earlier_real_marker() {
    let comments = "<!-- champion:hold-state head=abc1234 -->\nHolding.\n\
                    Note: champion:hold-state head=ffff9999 is what the marker looks like.\n";
    assert_eq!(recorded_head(comments).as_deref(), Some("abc1234"));
}

#[test]
fn leading_whitespace_before_the_html_comment_is_fine() {
    let comments = "    <!-- champion:hold-state head=abc1234 -->\n";
    assert_eq!(recorded_head(comments).as_deref(), Some("abc1234"));
}

#[test]
fn a_marker_sharing_its_html_comment_with_other_text_still_counts() {
    // The producer writes the marker alone, but nothing about the contract
    // requires that, and a stricter rule would be a second way to lose a real
    // marker.
    let comments = "<!-- champion:hold-state head=abc1234 (episode 2) -->\n";
    assert_eq!(recorded_head(comments).as_deref(), Some("abc1234"));
}

// --- SHA shape -------------------------------------------------------------

#[test]
fn an_uppercase_sha_is_not_matched_matching_the_shell_and_the_producer() {
    // Forge head SHAs are lowercase hex; the retired shell's class was
    // lowercase-only too. Widening it here would be a divergence with no
    // producer behind it.
    assert_eq!(recorded_head("<!-- champion:hold-state head=ABC1234 -->"), None);
}

#[test]
fn the_capture_stops_at_the_first_non_hex_character() {
    assert_eq!(
        recorded_head("<!-- champion:hold-state head=abc1234-stale -->").as_deref(),
        Some("abc1234")
    );
}

#[test]
fn an_abbreviated_marker_still_differs_from_a_full_head() {
    // Not prefix-tolerant, matching the shell: every observed producer writes
    // the full 40-character SHA, so treating a prefix as equal would suppress
    // a warning on evidence no producer generates.
    let comments = "<!-- champion:hold-state head=deadbee -->";
    assert!(assess("999", comments, "deadbeef").is_some());
}

// --- message shape ---------------------------------------------------------

#[test]
fn the_message_is_the_retired_shell_text() {
    assert_eq!(
        message("42", "abc1234", "deadbeef"),
        "champion:hold-state marker recorded head=abc1234, but PR #42's current head is deadbeef \
— the hold/approval state may have been recorded against a different tree than the one about to \
merge. loom:pr's presence means Judge approved SOME head; verify it still covers this one before \
proceeding."
    );
}

#[test]
fn the_clean_sentinel_is_not_a_substring_of_the_warning() {
    // The shell wrapper tells the two apart by exact comparison, but a
    // sentinel that could appear inside a warning would be a trap for any
    // future caller that greps instead.
    assert!(!message("42", "abc1234", "deadbeef").contains(CLEAN));
}
