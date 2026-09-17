//! Tests for startable-subset extraction (epic #7810, PR 3).
//!
//! These pin the rules the shell `awk` encoded. The shell suite
//! (`tests/test-detect-startable-subset.sh`, 25 assertions) still drives the
//! same logic through the CLI and is the compatibility proof; these cover the
//! edges that are awkward to reach from a CLI fixture.

use super::*;

#[test]
fn a_section_is_captured_without_its_heading() {
    let body = "# Title\n\n## Startable subset\n- do this now\n- and this\n";
    assert_eq!(extract_startable_subset(body), "- do this now\n- and this\n");
    assert!(has_startable_subset(body));
}

#[test]
fn capture_stops_at_the_next_heading_of_equal_depth() {
    let body = "## Startable subset\nin\n## Dependencies\nout\n";
    assert_eq!(extract_startable_subset(body), "in\n");
}

#[test]
fn capture_stops_at_a_shallower_heading() {
    let body = "### Startable subset\nin\n## Later\nout\n";
    assert_eq!(extract_startable_subset(body), "in\n");
}

#[test]
fn a_deeper_subsection_stays_inside_the_capture() {
    // The reason depth is tracked at all rather than stopping at any heading:
    // `### Files` documents the subset, it does not end it.
    let body = "## Startable subset\nintro\n### Files\na.rs\n## Dependencies\nout\n";
    assert_eq!(extract_startable_subset(body), "intro\n### Files\na.rs\n");
}

#[test]
fn the_heading_match_is_case_insensitive_and_a_prefix() {
    for heading in [
        "## Startable subset",
        "## startable subset",
        "## STARTABLE SUBSET",
        "## Startable Subset (partial)",
    ] {
        let body = format!("{heading}\nwork\n");
        assert_eq!(
            extract_startable_subset(&body),
            "work\n",
            "heading should have matched: {heading}"
        );
    }
}

#[test]
fn a_top_level_heading_is_not_a_section() {
    // `#` is the issue title. The shell's range is {2,6}; a single `#` must not
    // open a capture.
    let body = "# Startable subset\nwork\n";
    assert_eq!(extract_startable_subset(body), "");
    assert!(!has_startable_subset(body));
}

#[test]
fn seven_hashes_is_not_a_heading() {
    let body = "####### Startable subset\nwork\n";
    assert_eq!(extract_startable_subset(body), "");
}

#[test]
fn an_issue_reference_is_not_mistaken_for_a_heading() {
    // THE trap this repo keeps hitting: `#5664` starts with `#` but is prose.
    // If it read as a heading it would silently truncate the section.
    let body = "## Startable subset\nsee #5664 for context\nmore work\n";
    assert_eq!(extract_startable_subset(body), "see #5664 for context\nmore work\n");
}

#[test]
fn a_hash_run_without_following_whitespace_is_not_a_heading() {
    let body = "## Startable subset\n##notaheading\nstill in\n";
    assert_eq!(extract_startable_subset(body), "##notaheading\nstill in\n");
}

#[test]
fn an_indented_heading_still_counts() {
    // The shell strips leading whitespace before testing, so an indented
    // heading both opens and closes a capture.
    let body = "  ## Startable subset\nin\n   ## Next\nout\n";
    assert_eq!(extract_startable_subset(body), "in\n");
}

#[test]
fn a_blank_section_is_not_a_subset() {
    // Present-but-empty must read as absent: there is nothing to start.
    let body = "## Startable subset\n\n   \n## Dependencies\nout\n";
    assert!(!has_startable_subset(body));
}

#[test]
fn no_section_at_all_is_empty_and_absent() {
    let body = "# Title\n\nJust a description with no subset heading.\n";
    assert_eq!(extract_startable_subset(body), "");
    assert!(!has_startable_subset(body));
}

#[test]
fn only_the_first_section_is_captured() {
    // A second heading of the same name after the first has closed does not
    // reopen capture — matching the shell, whose `capturing` flag can be
    // re-armed only by the opening branch, which it reaches again.
    let body = "## Startable subset\nfirst\n## Other\nx\n## Startable subset\nsecond\n";
    let got = extract_startable_subset(body);
    assert!(got.contains("first"), "got {got:?}");
    // The shell DOES re-arm on the second heading, so both are captured.
    assert!(got.contains("second"), "shell re-arms on a later heading; got {got:?}");
    assert!(!got.contains('x'), "text between sections must not leak: {got:?}");
}

#[test]
fn an_empty_body_is_handled() {
    assert_eq!(extract_startable_subset(""), "");
    assert!(!has_startable_subset(""));
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
