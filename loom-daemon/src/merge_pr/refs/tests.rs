//! Tests for the closing-reference / partial-increment analysis.
//!
//! The two incident cases (#5234, the numbered-list ordinal) come first,
//! because they are the reason this code is shaped the way it is and they are
//! what a future simplification would break.

use super::*;

// --- #5234: a backticked hypothetical must not count as a declaration ---

#[test]
fn a_backticked_hypothetical_mention_is_not_a_declaration() {
    // The literal repro: prose describing what the author *would* write,
    // marked hypothetical with backticks. A bare grep read this as a declared
    // intent and reopened an issue that had been correctly closed.
    let body = "Closes #4600\n\n\
                If you would rather I attribute this differently, say so and I will\n\
                switch the reference to `Part of #4574` instead.\n";
    assert_eq!(
        partial_increment_refs(body),
        Vec::<u64>::new(),
        "a backticked mid-sentence mention is prose, not a declaration"
    );
    // The genuine closing reference is still seen.
    assert_eq!(closing_refs(body), vec![4600]);
}

#[test]
fn a_line_leading_declaration_is_still_read() {
    // The narrowing must not have thrown away the real case.
    for body in [
        "Part of #4574\n",
        "  Part of #4574\n",
        "- Part of #4574\n",
        "* Contributes to #4574\n",
        "+ Part of #4574\n",
        "> Part of #4574\n",
        "3. Part of #4574\n",
        "part of #4574\n",
        "CONTRIBUTES TO #4574\n",
    ] {
        assert_eq!(
            partial_increment_refs(body),
            vec![4574],
            "must read a line-leading declaration: {body:?}"
        );
    }
}

#[test]
fn the_inline_code_strip_changes_the_answer_and_this_pins_which_way() {
    // The strip's real single-line effect is to REMOVE a code span so what
    // follows becomes eligible for the line-leading anchor — i.e. it ADDS a
    // match. Verified against the frozen shell, which agrees.
    //
    // This case exists because the retirement record for the strip originally
    // named `a_backticked_hypothetical_mention_is_not_a_declaration` as its
    // successor, and that test would pass with the strip DELETED: its #5234
    // body has the mention mid-sentence, where the line-leading anchor rejects
    // it regardless. Review measured it — removing the strip changes the
    // answer for 0 of 29 corpus entries — so the retirement was leaning on a
    // test that did not exercise it.
    assert_eq!(
        partial_increment_refs("subject\n\n`x` Part of #5\n"),
        vec![5],
        "the strip blanks the span, and `Part of` then satisfies the line-leading anchor"
    );

    // And the shape where it genuinely prevents nothing: a declaration wholly
    // inside a code span is rejected by the anchor whether or not the strip
    // runs, because the line begins with a backtick.
    assert_eq!(partial_increment_refs("subject\n\n`Part of #4574`\n"), Vec::<u64>::new());
}

// --- the numbered-list ordinal trap ---

#[test]
fn a_numbered_list_ordinal_is_not_read_as_an_issue_number() {
    // Scanning the whole matched span for digit runs reads `3. Part of #789`
    // as BOTH 3 and 789. Combined with a real `Closes #3` elsewhere, #3 would
    // be registered as a declared partial increment and reopened immediately
    // after a correct close.
    let body = "Closes #3\n\n3. Part of #789\n";
    assert_eq!(
        partial_increment_refs(body),
        vec![789],
        "the list ordinal 3 must not become a referenced issue"
    );
    assert_eq!(closing_refs(body), vec![3]);
}

// --- closing-keyword boundary ---

#[test]
fn a_keyword_substring_does_not_match() {
    // `\b` is what stops `Discloses #5` matching `close`.
    assert_eq!(closing_refs("Discloses #5\n"), Vec::<u64>::new());
    assert_eq!(closing_refs("Foreclosed #5\n"), Vec::<u64>::new());
    assert_eq!(closing_refs("closes #5\n"), vec![5]);
}

#[test]
fn every_documented_closing_keyword_form_is_recognised() {
    for (body, want) in [
        ("close #1", 1u64),
        ("closes #2", 2),
        ("closed #3", 3),
        ("fix #4", 4),
        ("fixes #5", 5),
        ("fixed #6", 6),
        ("resolve #7", 7),
        ("resolves #8", 8),
        ("resolved #9", 9),
        ("CLOSES #10", 10),
    ] {
        assert_eq!(closing_refs(body), vec![want], "{body:?}");
    }
}

// --- fenced blocks ---

#[test]
fn a_reference_inside_a_fenced_block_is_not_a_declaration() {
    let body = "```\nPart of #111\n```\nPart of #222\n";
    assert_eq!(partial_increment_refs(body), vec![222]);
}

#[test]
fn an_unclosed_fence_swallows_the_rest_deliberately() {
    // Reproduced from the shell rather than corrected: the awk toggle has no
    // notion of an unterminated fence, so everything after one is invisible.
    // Changing it here would change which references the guard sees, which is
    // not this port's job — it is a behaviour question for its own issue.
    let body = "Part of #1\n```\nPart of #2\nPart of #3\n";
    assert_eq!(partial_increment_refs(body), vec![1]);
}

#[test]
fn closing_refs_do_not_strip_fences() {
    // Asymmetry carried over verbatim: the closing-keyword scan reads the raw
    // text, fences and all, while the partial-increment scan strips them. The
    // shell does exactly this, and the difference is load-bearing — a `Closes
    // #N` quoted in a code block still counts as a closing reference today.
    let body = "```\nCloses #77\n```\n";
    assert_eq!(closing_refs(body), vec![77]);
    assert_eq!(partial_increment_refs(body), Vec::<u64>::new());
}

// --- ordering and dedup ---

#[test]
fn results_are_deduped_and_ascending_numeric() {
    // `sort -un` is numeric, so 9 sorts before 10 — a lexical sort would not.
    let body = "Closes #10\nfixes #9\nresolves #10\nclose #100\n";
    assert_eq!(closing_refs(body), vec![9, 10, 100]);
}

// --- snippets ---

#[test]
fn closing_snippets_are_rendered_for_direct_interpolation() {
    let body = "Closes #7 and later fixes #7 again\n";
    // Sorted, deduped, joined so the caller can drop it inside quotes.
    assert_eq!(closing_ref_snippets(body, 7), "Closes #7\", \"fixes #7");
}

#[test]
fn a_snippet_query_is_issue_scoped_and_boundary_safe() {
    let body = "Closes #7\nCloses #77\n";
    assert_eq!(closing_ref_snippets(body, 7), "Closes #7");
    assert_eq!(closing_ref_snippets(body, 77), "Closes #77");
}

#[test]
fn partial_snippets_are_trimmed_and_fence_stripped() {
    let body = "```\n  Part of #5\n```\n  - Part of #5\n";
    assert_eq!(partial_increment_ref_snippets(body, 5), "- Part of #5");
}

#[test]
fn absent_references_render_empty_so_the_caller_can_attribute_blame() {
    // Empty is the signal the caller uses to decide WHICH source is at fault.
    assert_eq!(closing_ref_snippets("nothing here", 5), "");
    assert_eq!(partial_increment_ref_snippets("nothing here", 5), "");
}

// --- degenerate input ---

#[test]
fn empty_and_whitespace_input_yield_nothing_rather_than_panicking() {
    for body in ["", "\n", "   ", "```", "`", "#", "#0"] {
        let _ = partial_increment_refs(body);
        let _ = closing_refs(body);
        let _ = closing_ref_snippets(body, 1);
        let _ = partial_increment_ref_snippets(body, 1);
    }
}

#[test]
fn a_reference_number_too_large_for_u64_is_dropped_not_panicked() {
    let body = "Closes #99999999999999999999999999\n";
    assert_eq!(closing_refs(body), Vec::<u64>::new());
}

// --- backticked partial-increment trailer warning (#5690, ported #8831) ---
//
// Every case below is a byte-identical port of the shell suite's BT1-BT8
// (`defaults/scripts/tests/test-merge-pr-partial-increment.sh` prior to
// #8831) — those expectations ARE the reviewed downstream test cases
// (rjwalters/kicad-tools#5692), ported faithfully rather than "improved".

#[test]
fn bt1_the_5690_incident_shape_is_detected_and_still_not_a_declaration() {
    // The exact incident: PR #5686's body wrote the trailer as `Part of
    // #5240` (whole line, wrapped in an inline code span) instead of the
    // plain Part of #5240. The parser's answer must NOT change; the new
    // detector is what makes the silence visible.
    let body = "## Summary\n\nImplements the first slice.\n\n`Part of #5240`";
    assert_eq!(
        partial_increment_refs(body),
        Vec::<u64>::new(),
        "#5690 incident: backticked '`Part of #5240`' is still NOT a declaration (#5234 unchanged)"
    );
    assert_eq!(
        backticked_partial_increment_trailer_refs(body),
        vec![5240],
        "#5690 incident: the backticked-trailer detector DOES see #5240"
    );

    let warnings = backticked_partial_increment_warnings(body, "5686", false);
    assert!(
        warnings.contains("Backticked partial-increment trailer (#5690)"),
        "warning names the finding: {warnings:?}"
    );
    assert!(
        warnings.contains("\"`Part of #5240`\""),
        "warning quotes the offending line verbatim: {warnings:?}"
    );
    assert!(warnings.contains("#5240"), "warning names the affected issue: {warnings:?}");
}

#[test]
fn bt2_a_plain_text_trailer_does_not_warn() {
    // The correct shape — the one the convention asks for and the one the
    // reset already handles — must not warn.
    let body = "## Summary\n\nImplements the first slice.\n\nPart of #123";
    assert_eq!(
        backticked_partial_increment_trailer_refs(body),
        Vec::<u64>::new(),
        "plain-text 'Part of #123' trailer: detector finds nothing to warn about"
    );
    assert_eq!(backticked_partial_increment_warnings(body, "1", false), "");
}

#[test]
fn bt3_a_mid_sentence_backticked_mention_does_not_warn() {
    // Prose describing a hypothetical must stay silent — warning on it would
    // train operators to ignore this warning entirely.
    let body = "Closes #4600\n\nIf you would rather I attribute this differently, say so\nand I will switch the reference to `Part of #4574` instead.\n";
    assert_eq!(
        backticked_partial_increment_trailer_refs(body),
        Vec::<u64>::new(),
        "mid-sentence backticked 'Part of #4574' does NOT warn (not a whole-line trailer)"
    );
}

#[test]
fn bt4_a_line_listing_backticked_trailers_as_examples_does_not_warn() {
    let body = "Use `Part of #123` / `Contributes to #456` for a partial increment.";
    assert_eq!(
        backticked_partial_increment_trailer_refs(body),
        Vec::<u64>::new(),
        "docs prose listing backticked trailers as examples does NOT warn"
    );
}

#[test]
fn bt5_a_backticked_trailer_inside_a_fenced_block_does_not_warn() {
    let body = "Write the trailer plainly:\n\n```markdown\n`Part of #123`\n```\n\nPart of #456";
    assert_eq!(
        backticked_partial_increment_trailer_refs(body),
        Vec::<u64>::new(),
        "a backticked trailer inside a fence does NOT warn"
    );
}

#[test]
fn bt6_an_issue_named_by_both_shapes_is_not_warned_about() {
    // The reset fires for it regardless via the real (plain-text) trailer.
    let body = "Part of #123\n\n`Part of #123`";
    assert_eq!(
        partial_increment_refs(body),
        vec![123],
        "the plain-text trailer still declares #123"
    );
    assert_eq!(
        backticked_partial_increment_warnings(body, "1", false),
        "",
        "no warning — #123 is already a parsed declaration"
    );
}

#[test]
fn bt7_marker_prefixed_and_numbered_list_forms() {
    // The numbered case also proves the marker ordinal cannot leak in as an
    // issue number (the PA6 trap), independently of `partial_increment_refs`.
    assert_eq!(
        backticked_partial_increment_trailer_refs("- `Contributes to #42`"),
        vec![42],
        "list marker: '- `Contributes to #42`' warns on #42"
    );
    assert_eq!(
        backticked_partial_increment_trailer_refs("3. `Part of #789`"),
        vec![789],
        "numbered marker: '3. `Part of #789`' yields ONLY 789 (ordinal does not leak)"
    );
}

#[test]
fn bt8_dry_run_prefixes_every_warning_line() {
    let body = "## Summary\n\nImplements the first slice.\n\n`Part of #5240`";
    let warnings = backticked_partial_increment_warnings(body, "5686", true);
    assert!(
        warnings.starts_with("[dry-run] Backticked partial-increment trailer (#5690)"),
        "dry run: warning carries the [dry-run] prefix: {warnings:?}"
    );
}

// --- #1057: a negated closing keyword is not a closing intent ---

#[test]
fn plain_closing_keywords_are_unnegated_references() {
    for body in ["Closes #42", "Fixes #42", "Resolves #42", "fixed #42"] {
        assert!(has_unnegated_closing_ref(body, 42), "{body:?}");
        assert_eq!(closing_ref_negation_status(body, 42), ClosingRefNegationStatus::Unnegated);
    }
}

#[test]
fn the_1057_repro_body_carries_no_real_closing_reference() {
    // PR #1051's body against #909: the author's stated intent, twice, was to
    // leave #909 open — yet GitHub's parser (and `closing_refs`) close it.
    let body = "## Relationship to #909 (not fixed here)\n\n\
                **does not fix #909** -- it structurally sidesteps that bug shape by reading\n\
                the SUMMARY line on purpose. #909 is left open for its owner to close or\n\
                subsume; nothing here depends on it.\n\nCloses #902\n";
    assert_eq!(closing_refs(body), vec![902, 909], "the raw parser is fooled");
    assert!(!has_unnegated_closing_ref(body, 909));
    assert_eq!(closing_ref_negation_status(body, 909), ClosingRefNegationStatus::NegatedOnly);
    assert!(has_unnegated_closing_ref(body, 902));
    assert_eq!(closing_ref_negation_status(body, 902), ClosingRefNegationStatus::Unnegated);
}

#[test]
fn negation_words_and_contractions_suppress_the_reference() {
    for body in [
        "This change does not resolve #42 fully; a follow-up will.",
        "This doesn't fix #42 on its own.",
        "This doesn’t fix #42 on its own.",
        "We never fixed #42 in this pass.",
    ] {
        assert!(!has_unnegated_closing_ref(body, 42), "{body:?}");
        assert_eq!(
            closing_ref_negation_status(body, 42),
            ClosingRefNegationStatus::NegatedOnly,
            "{body:?}"
        );
    }
}

#[test]
fn a_negation_in_another_clause_does_not_suppress_a_genuine_close() {
    let body = "This does not fix #42 alone. Closes #42 together with the follow-up.";
    assert!(has_unnegated_closing_ref(body, 42));
    // Text AFTER a genuine close in the same clause cannot negate it.
    assert!(has_unnegated_closing_ref("Closes #42 but not #43", 42));
}

#[test]
fn non_closing_keywords_and_other_issues_are_not_references() {
    assert!(!has_unnegated_closing_ref("Updates #42 with more detail.", 42));
    assert!(!has_unnegated_closing_ref("See #42", 42));
    assert!(!has_unnegated_closing_ref("Closes #420", 42));
    assert!(!has_unnegated_closing_ref("Discloses #42", 42));
    assert!(!has_unnegated_closing_ref("", 42));
}

// --- Judge review on PR #8823: tri-state status, no-reference vs negated ---

#[test]
fn no_textual_reference_is_a_distinct_status_from_negated() {
    // A body with no textual mention at all (issue linked only through the
    // Development sidebar, say) must NOT read the same as "mentioned and
    // disclaimed" — conflating them reopened issues GitHub had closed
    // correctly through a channel this regex cannot see.
    for body in [
        "",
        "Some body with no mention",
        "Updates #42 with more detail.",
    ] {
        assert_eq!(
            closing_ref_negation_status(body, 42),
            ClosingRefNegationStatus::NoReference,
            "{body:?}"
        );
    }
}

#[test]
fn cross_repo_and_url_closing_forms_are_no_reference_not_negated() {
    // `has_unnegated_closing_ref`'s regex only matches a bare `#N` — it does
    // not understand `owner/repo#N` or a full issue URL. Both must report
    // NoReference (unknown to this predicate), never NegatedOnly.
    for body in [
        "Fixes rjwalters/loom#42",
        "Fixes https://github.com/rjwalters/loom/issues/42",
        "Closes: #42",
    ] {
        assert_eq!(
            closing_ref_negation_status(body, 42),
            ClosingRefNegationStatus::NoReference,
            "{body:?}"
        );
    }
}

#[test]
fn a_distant_negation_word_in_the_same_clause_does_not_suppress_a_later_close() {
    // The non-blocking finding from the #8823 review: a whole-clause
    // negation scan reads this as negated because `isn't` appears somewhere
    // earlier in the clause, even though it has nothing to do with
    // `fixes #42`. The negation window is now limited to the two words
    // immediately before the keyword.
    let body = "If the lock isn't held we now return early, which fixes #42";
    assert_eq!(
        closing_ref_negation_status(body, 42),
        ClosingRefNegationStatus::Unnegated,
        "{body:?}"
    );
}
