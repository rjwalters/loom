//! Tests for dependency-finding classification (epic #7810, PR 3).

use super::*;

#[test]
fn a_phrase_immediately_followed_by_a_reference_is_a_dependency() {
    assert!(is_dependency_finding("Blocked by #3"));
    assert!(is_dependency_finding("- Depends on acme/widgets#7"));
}

#[test]
fn a_reference_shortly_before_a_bare_phrase_is_a_dependency() {
    // The lead window: the bare forms follow their subject.
    assert!(is_dependency_finding("#3 is blocking this work"));
    assert!(is_dependency_finding("acme/w#12 blocks the rollout"));
}

#[test]
fn a_phrase_with_no_reference_anywhere_is_not_a_dependency() {
    assert!(!is_dependency_finding("Blocked by a pending design decision"));
}

#[test]
fn a_reference_with_no_phrase_anywhere_is_not_a_dependency() {
    assert!(!is_dependency_finding("See #3 for background"));
}

#[test]
fn co_occurrence_alone_is_not_enough() {
    // #7756: the case that motivated proximity at all. "prerequisite" explains
    // why a soak has not started; it does not cite #7430 as a blocker. Without
    // a window this reads as a dependency and defers the issue forever.
    let bullet = "#7430 (which was a prerequisite for any meaningful soak) merged only \
                  minutes before this evaluation, so no soak observation window has \
                  started yet and the result is therefore not yet observable";
    assert!(
        !is_dependency_finding(bullet),
        "a phrase far from the reference must not read as a dependency"
    );
}

#[test]
fn the_phrase_match_is_case_insensitive() {
    // Unlike parse_dependency_refs, this one IS case-insensitive (grep -qiE).
    // The two functions genuinely differ; a rewrite that harmonised them would
    // change behaviour in one of them.
    assert!(is_dependency_finding("blocked by #3"));
    assert!(is_dependency_finding("BLOCKED BY #3"));
}

#[test]
fn a_url_reference_counts() {
    assert!(is_dependency_finding("Blocked by https://github.com/acme/widgets/issues/42"));
    assert!(is_dependency_finding("Requires https://github.com/acme/widgets/pull/9"));
}

#[test]
fn findings_are_dependency_only_requires_every_line_to_qualify() {
    let all_deps = "Blocked by #3\nDepends on #4\n";
    assert!(findings_are_dependency_only(all_deps));

    let mixed = "Blocked by #3\nThe approach is wrong on the merits\n";
    assert!(
        !findings_are_dependency_only(mixed),
        "one merits finding must disqualify the whole set"
    );
}

#[test]
fn blank_lines_are_skipped_but_an_all_blank_set_does_not_qualify() {
    assert!(findings_are_dependency_only("Blocked by #3\n\n   \nDepends on #4\n"));
    // "No findings" is NOT "only dependency findings" — treating it as such
    // would un-escalate a proposal nobody actually re-evaluated.
    assert!(!findings_are_dependency_only(""));
    assert!(!findings_are_dependency_only("\n   \n\n"));
}

/// #7877: the real finding the old single 60-character window misclassified.
///
/// Verbatim from the Champion verdict on #7854. "Blocked by" and `#7853` are 84
/// characters apart — a description and a restatement sit between them — so the
/// old window read a self-clearing timing dependency as a merits finding and
/// nearly escalated the issue to `loom:operator-only`. The explicit-phrase
/// window now covers it.
///
/// This assertion is the deliberate flip of the known-failure assertion the
/// port (epic #7810, PR 3) landed in its place.
#[test]
fn issue_7877_a_far_but_explicit_citation_is_a_dependency() {
    let bullet = "**Technical feasibility**: this issue's own Dependencies section states it \
                  is \"Blocked by the sibling Phase 4 issue (run-job seam contract + host \
                  executor)\" — that issue is #7853, which is currently OPEN";
    assert!(
        is_dependency_finding(bullet),
        "an explicit \"Blocked by\" whose citation is restated further along the sentence is \
         still a timing finding (#7877)"
    );
}

#[test]
fn the_wide_window_applies_only_to_the_explicit_phrase_family() {
    // Same 84-character distance as the #7877 case, but reached through a weak,
    // narrative phrase rather than a prepositional one. "requires" and
    // "prerequisite" are the forms ordinary prose uses without citing a
    // blocker, so they keep the conservative window — widening them is how the
    // #7756/#7431 false positive would come back.
    let weak = "**Technical feasibility**: this proposal requires a redesign of the sibling \
                Phase 4 surface (run-job seam contract + host executor), much like the one \
                landed in #7853";
    assert!(
        !is_dependency_finding(weak),
        "a weak narrative phrase far from a reference must still not read as a dependency"
    );

    // And the weak family is unchanged at close range: still a dependency.
    assert!(is_dependency_finding("Requires #7853 to land first"));
}

#[test]
fn every_explicit_phrase_gets_the_wide_window() {
    // ~84 characters of interposed prose, the #7877 shape, for each member of
    // the explicit family.
    let filler = "the sibling Phase 4 issue (run-job seam contract + host executor) — that \
                  issue is ";
    for phrase in [
        "Blocked by",
        "depends on",
        "dependent on",
        "dependency on",
        "dependencies of",
        "waiting on",
        "waits on",
        "cannot start until",
        "cannot proceed until",
        "cannot begin work until",
        "not startable until",
        "must wait for",
        "must wait until",
    ] {
        let bullet = format!("This issue is {phrase} {filler}#7853");
        assert!(
            is_dependency_finding(&bullet),
            "explicit phrase {phrase:?} should reach a citation 84 chars away"
        );
    }
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
