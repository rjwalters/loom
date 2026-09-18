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
