//! Tests for verdict-finding extraction (epic #7810, PR 3).

use super::*;

#[test]
fn bullets_become_one_finding_per_line() {
    let c = "Verdict prose.\n- first finding\n- second finding\n";
    assert_eq!(extract_findings(c), "- first finding\n- second finding\n");
}

#[test]
fn both_bullet_markers_are_recognised() {
    assert_eq!(extract_findings("- dash\n"), "- dash\n");
    assert_eq!(extract_findings("* star\n"), "* star\n");
}

#[test]
fn a_bullet_marker_needs_following_whitespace() {
    // `*emphasis*` opening a line is prose, not a bullet.
    assert_eq!(extract_findings("*emphasised* prose\n"), "");
}

#[test]
fn an_indented_continuation_folds_onto_its_bullet() {
    let c = "- a finding that wraps\n  onto a second line\n";
    assert_eq!(extract_findings(c), "- a finding that wraps   onto a second line\n");
}

#[test]
fn prose_resuming_ends_the_list() {
    // The load-bearing rule: verdict prose after the bullets often contains
    // issue references. Reading it as findings would classify a merits verdict
    // as a dependency and un-escalate work a human parked deliberately.
    let c = "- a finding\nUnindented prose mentioning #99 as a blocker.\n- not a finding\n";
    assert_eq!(extract_findings(c), "- a finding\n");
}

#[test]
fn a_blank_line_ends_the_list() {
    // A blank line has no non-space character, so it is not a continuation.
    let c = "- a finding\n\n- after the gap\n";
    assert_eq!(extract_findings(c), "- a finding\n");
}

#[test]
fn recommended_actions_ends_the_list() {
    let c = "- a finding\n**Recommended actions**\n- do this\n";
    assert_eq!(extract_findings(c), "- a finding\n");
}

#[test]
fn recommended_actions_before_any_bullet_yields_nothing() {
    let c = "Some prose.\n**Recommended actions**\n- do this\n";
    assert_eq!(extract_findings(c), "");
}

#[test]
fn prose_before_the_first_bullet_is_skipped() {
    let c = "Intro line.\nAnother intro line.\n- the finding\n";
    assert_eq!(extract_findings(c), "- the finding\n");
}

#[test]
fn a_comment_with_no_bullets_yields_nothing() {
    assert_eq!(extract_findings("Just prose, no list.\n"), "");
    assert_eq!(extract_findings(""), "");
}

// ---------------------------------------------------------------------------
// The differential tests that used to live here (epic #7810, PR 3)
// ---------------------------------------------------------------------------
//
// This port was not translated on trust. Each function above landed in #7943
// beside a DIFFERENTIAL test that ran it and the shell original over the same
// fixture corpus and asserted they agreed, character for character, with an
// anti-vacuity guard so a shell that silently produced nothing could not pass.
//
// Those tests are removed here with the shell they compared against: a
// comparison needs both sides, and keeping a copy of the retired
// implementation purely to compare with would be keeping the thing this epic
// retires. The evidence is the merged CI run on #7943, not a fixture that
// pins a deleted file forever.
//
// What still runs both ways is the black-box suite
// `defaults/scripts/tests/test-classify-dependency-block.sh`, whose assertions
// were written against the shell and now drive this implementation unchanged
// through the same CLI.
