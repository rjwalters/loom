//! Tests for the backticked partial-increment trailer detector (#8796).
//!
//! Two properties carry the whole feature and are asserted in pairs
//! throughout: the incident shape WARNS, and every prose shape that merely
//! mentions the trailer STAYS SILENT. A detector that failed the second half
//! would be worse than none — an advisory warning that fires on documentation
//! is one operators learn to skip past, taking the real case with it.

use super::*;

/// The exact downstream incident: rjwalters/kicad-tools PR #5686's body wrote
/// the trailer as a code span on its own line instead of as plain text.
const INCIDENT_BODY: &str = "## Summary\n\nImplements the first slice.\n\n`Part of #5240`\n";

// --- The parser's answer is UNCHANGED (AC #1) ------------------------------

#[test]
fn the_declaration_parser_still_ignores_a_backticked_trailer() {
    // This is the property the whole fix is built around NOT changing. #5234's
    // code-span exclusion stays exactly as it was; only the silence around it
    // is addressed.
    assert_eq!(
        refs::partial_increment_refs(INCIDENT_BODY),
        Vec::<u64>::new(),
        "a backticked trailer is still not a declaration"
    );
}

// --- The incident shape warns ----------------------------------------------

#[test]
fn the_incident_shape_is_detected() {
    assert_eq!(backticked_trailer_refs(INCIDENT_BODY), vec![5240]);
}

#[test]
fn the_warning_names_the_issue_and_quotes_the_offending_line() {
    let warnings = backticked_trailer_warnings(INCIDENT_BODY, 5686, false);
    assert_eq!(warnings.len(), 2, "one advisory pair per affected issue");
    assert!(
        warnings[0].contains("#5240"),
        "warning must name the stranded issue: {}",
        warnings[0]
    );
    assert!(
        warnings[0].contains("\"`Part of #5240`\""),
        "warning must quote the offending line verbatim: {}",
        warnings[0]
    );
    assert!(warnings[0].contains("PR #5686"), "warning must name the PR: {}", warnings[0]);
    assert!(
        warnings[1].contains("PLAIN TEXT"),
        "second line must state the remedy: {}",
        warnings[1]
    );
    assert!(
        warnings[1].contains("nothing is blocked"),
        "second line must say the warning is advisory: {}",
        warnings[1]
    );
}

#[test]
fn every_accepted_spelling_and_marker_is_detected() {
    for body in [
        "`Part of #4574`",
        "  `Part of #4574`",
        "- `Part of #4574`",
        "* `Contributes to #4574`",
        "+ `Part of #4574`",
        "> `Part of #4574`",
        "3. `Part of #4574`",
        "`part of #4574`",
        "`CONTRIBUTES TO #4574`",
        "`Part of #4574`.",
        "`Part of #4574`  ",
        "``Part of #4574``",
        "`Part of  #4574`",
    ] {
        assert_eq!(backticked_trailer_refs(body), vec![4574], "should detect: {body:?}");
    }
}

#[test]
fn a_numbered_marker_ordinal_does_not_leak_in_as_an_issue_number() {
    // The sibling of the trap that shaped `partial_increment_refs`: scanning
    // the whole matched span for digit runs reads `3. `Part of #789`` as
    // referencing BOTH 3 and 789, and the detector would then warn about an
    // issue the body never mentions.
    assert_eq!(backticked_trailer_refs("3. `Part of #789`"), vec![789]);
}

// --- Prose shapes stay silent ----------------------------------------------

#[test]
fn the_5234_mid_sentence_mention_does_not_warn() {
    let body = "Closes #4600\n\n\
                If you would rather I attribute this differently, say so and I will\n\
                switch the reference to `Part of #4574` instead.\n";
    assert_eq!(backticked_trailer_refs(body), Vec::<u64>::new());
    assert!(backticked_trailer_warnings(body, 1, false).is_empty());
}

#[test]
fn a_docs_line_listing_trailers_as_examples_does_not_warn() {
    let body = "Use `Part of #123` / `Contributes to #456` for a partial increment.";
    assert_eq!(backticked_trailer_refs(body), Vec::<u64>::new());
}

#[test]
fn a_trailer_inside_a_fenced_block_does_not_warn() {
    let body = "Write the trailer plainly:\n\n```markdown\n`Part of #123`\n```\n\nPart of #456\n";
    assert_eq!(backticked_trailer_refs(body), Vec::<u64>::new());
    assert!(backticked_trailer_warnings(body, 1, false).is_empty());
}

#[test]
fn a_plain_text_trailer_does_not_warn() {
    let body = "## Summary\n\nImplements the first slice.\n\nPart of #123\n";
    assert_eq!(backticked_trailer_refs(body), Vec::<u64>::new());
    assert!(backticked_trailer_warnings(body, 1, false).is_empty());
}

#[test]
fn an_issue_declared_in_both_shapes_does_not_warn() {
    // The reset fires for #123 regardless, so there is nothing to say.
    let body = "Part of #123\n\n`Part of #123`\n";
    assert_eq!(refs::partial_increment_refs(body), vec![123]);
    assert_eq!(backticked_trailer_refs(body), vec![123], "the backticked line is still seen…");
    assert!(
        backticked_trailer_warnings(body, 1, false).is_empty(),
        "…but it is suppressed, because #123 is already a parsed declaration"
    );
}

#[test]
fn a_mixed_body_warns_only_about_the_undeclared_issue() {
    let body = "Part of #123\n\n`Contributes to #456`\n";
    let warnings = backticked_trailer_warnings(body, 1, false);
    assert_eq!(warnings.len(), 2);
    assert!(warnings[0].contains("#456"), "{}", warnings[0]);
    assert!(
        !warnings[0].contains("#123 wrapped"),
        "the parsed declaration must not be warned about: {}",
        warnings[0]
    );
}

#[test]
fn a_backticked_closing_keyword_is_not_this_detectors_business() {
    // `Closes #N` in a code span is a different (and harmless) shape: GitHub
    // does not honour it either, but no Loom automation depends on it, so
    // widening the detector to cover it would only add false alarms.
    assert_eq!(backticked_trailer_refs("`Closes #123`"), Vec::<u64>::new());
}

#[test]
fn a_trailer_with_trailing_prose_on_the_same_line_does_not_warn() {
    // Whole-line is the discriminator. Anything after the closing backtick
    // other than one punctuation mark means the author was writing a sentence.
    assert_eq!(backticked_trailer_refs("`Part of #123` once #124 lands"), Vec::<u64>::new());
    assert_eq!(backticked_trailer_refs("The trailer `Part of #123`"), Vec::<u64>::new());
}

// --- Dry-run contract -------------------------------------------------------

#[test]
fn dry_run_prefixes_both_lines() {
    let warnings = backticked_trailer_warnings(INCIDENT_BODY, 5686, true);
    assert!(warnings[0].starts_with("[dry-run] "), "{}", warnings[0]);
    assert!(
        warnings[1].starts_with("  [dry-run] "),
        "the indent stays outside the prefix, matching the conflict warnings: {}",
        warnings[1]
    );
}

#[test]
fn without_dry_run_there_is_no_prefix() {
    let warnings = backticked_trailer_warnings(INCIDENT_BODY, 5686, false);
    assert!(!warnings[0].contains("[dry-run]"));
    assert!(!warnings[1].contains("[dry-run]"));
}

// --- Rendering --------------------------------------------------------------

#[test]
fn multiple_offending_lines_for_one_issue_are_joined_like_the_other_snippets() {
    let body = "`Part of #123`\n\n- `Contributes to #123`\n";
    assert_eq!(
        backticked_trailer_snippets(body, 123),
        "- `Contributes to #123`\", \"`Part of #123`"
    );
    // Still one advisory pair — the warning is per issue, not per line.
    assert_eq!(backticked_trailer_warnings(body, 1, false).len(), 2);
}

#[test]
fn snippets_for_an_unmatched_issue_are_empty() {
    assert_eq!(backticked_trailer_snippets(INCIDENT_BODY, 999), "");
}

#[test]
fn an_empty_body_is_silent() {
    assert!(backticked_trailer_warnings("", 1, false).is_empty());
    assert_eq!(backticked_trailer_refs(""), Vec::<u64>::new());
}
