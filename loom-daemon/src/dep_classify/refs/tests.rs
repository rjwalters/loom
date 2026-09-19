//! Tests for dependency-reference parsing (epic #7810, PR 3).
//!
//! As with `subset`, the unit tests pin the rules and the differential tests
//! prove the port matches `detect-dependency-cycle.sh`'s `parse_dependency_refs`.

use super::*;

const REPO: &str = "o/r";

#[test]
fn a_bare_reference_takes_the_default_repo() {
    assert_eq!(parse_dependency_refs("Blocked by #3", REPO), vec!["o/r#3"]);
}

#[test]
fn a_qualified_reference_keeps_its_own_repo() {
    assert_eq!(parse_dependency_refs("Depends on other/repo#7", REPO), vec!["other/repo#7"]);
}

#[test]
fn a_url_is_reduced_to_owner_repo_and_number() {
    assert_eq!(
        parse_dependency_refs("Requires https://github.com/acme/widgets/issues/42", REPO),
        vec!["acme/widgets#42"]
    );
}

#[test]
fn all_three_phrases_are_recognised() {
    for phrase in ["Blocked by", "Depends on", "Requires"] {
        let body = format!("{phrase} #5");
        assert_eq!(
            parse_dependency_refs(&body, REPO),
            vec!["o/r#5"],
            "phrase should have matched: {phrase}"
        );
    }
}

#[test]
fn the_phrase_match_is_case_sensitive() {
    // `grep -E` without `-i`. Preserved deliberately: changing it would silently
    // widen what counts as a declared dependency across every existing body.
    assert!(parse_dependency_refs("blocked by #3", REPO).is_empty());
    assert!(parse_dependency_refs("BLOCKED BY #3", REPO).is_empty());
}

#[test]
fn markdown_emphasis_between_phrase_and_reference_is_tolerated() {
    // The `[*_:\s]*` class exists for `**Blocked by**: #3`.
    assert_eq!(parse_dependency_refs("**Blocked by**: #3", REPO), vec!["o/r#3"]);
    assert_eq!(parse_dependency_refs("_Requires_ #4", REPO), vec!["o/r#4"]);
}

#[test]
fn every_reference_on_a_matching_line_is_captured_not_just_the_first() {
    // The shell's second grep scans the WHOLE line. `#99` here is arguably
    // over-capture, but it is the established behaviour and fixtures depend on
    // it. Pinned so a future "tidy-up" cannot quietly narrow it.
    assert_eq!(
        parse_dependency_refs("Blocked by #3 (see also #99)", REPO),
        vec!["o/r#3", "o/r#99"]
    );
}

#[test]
fn a_line_without_a_phrase_contributes_nothing() {
    assert!(parse_dependency_refs("Mentions #3 in passing", REPO).is_empty());
}

#[test]
fn results_are_sorted_and_deduplicated() {
    let body = "Blocked by #9\nDepends on #2\nRequires #9\n";
    assert_eq!(parse_dependency_refs(body, REPO), vec!["o/r#2", "o/r#9"]);
}

#[test]
fn a_url_too_short_to_carry_owner_and_repo_is_dropped() {
    // The shell's `[[ "$rest" == */* ]] &&` emits nothing rather than erroring.
    let got = parse_dependency_refs("Blocked by https://example.com/issues/42", REPO);
    assert!(got.is_empty(), "expected no refs, got {got:?}");
}

#[test]
fn an_empty_body_yields_nothing() {
    assert!(parse_dependency_refs("", REPO).is_empty());
}

// ---------------------------------------------------------------------------
// extract_refs — the UNGATED parser, used on findings
// ---------------------------------------------------------------------------

#[test]
fn extract_refs_needs_no_dependency_phrase() {
    // The whole reason this is a second parser. The text it reads has already
    // been classified as dependency-only, so every reference on it is a
    // blocker; there is nothing left to gate on.
    assert_eq!(
        extract_refs("- Technical Feasibility: see #9 and #12.", "o/r"),
        vec!["o/r#12".to_string(), "o/r#9".to_string()]
    );
    assert!(
        parse_dependency_refs("- Technical Feasibility: see #9 and #12.", "o/r").is_empty(),
        "the gated parser finds nothing here — that is the difference"
    );
}

#[test]
fn extract_refs_is_case_insensitive_because_it_has_no_phrase_to_match() {
    // A Champion verdict routinely writes "blocked by #9" in lower case.
    // parse_dependency_refs drops that line (grep -E, no -i); dropping it here
    // would silently turn a real blocker into `no-recorded-blocker` and
    // escalate a proposal that was only waiting.
    let lower = "- Technical Feasibility: blocked by #9 (the harness PR), still open.";
    assert_eq!(extract_refs(lower, "o/r"), vec!["o/r#9".to_string()]);
    assert!(
        parse_dependency_refs(lower, "o/r").is_empty(),
        "case-sensitivity is real, which is why the findings side cannot use it"
    );
}

#[test]
fn extract_refs_accepts_a_pull_url_which_the_gated_parser_does_not() {
    // A verdict cites the PR that will unblock the work, and a MERGED PR
    // resolves the block as surely as a closed issue.
    let text = "- blocked on https://github.com/o/x/pull/7";
    assert_eq!(extract_refs(text, "o/r"), vec!["o/x#7".to_string()]);
    assert!(parse_dependency_refs(text, "o/r").is_empty());
}

#[test]
fn extract_refs_normalises_the_same_three_shapes() {
    assert_eq!(extract_refs("see #3", "o/r"), vec!["o/r#3".to_string()]);
    assert_eq!(extract_refs("see a/b#9", "o/r"), vec!["a/b#9".to_string()]);
    assert_eq!(
        extract_refs("see https://github.com/o/x/issues/56", "o/r"),
        vec!["o/x#56".to_string()]
    );
}

#[test]
fn extract_refs_sorts_and_deduplicates() {
    assert_eq!(
        extract_refs("#5 then #3 then #5 again", "o/r"),
        vec!["o/r#3".to_string(), "o/r#5".to_string()]
    );
}

#[test]
fn a_url_too_short_to_name_a_repo_is_dropped_not_guessed() {
    assert!(extract_refs("https://example.com/issues/5", "o/r").is_empty());
}

// ---------------------------------------------------------------------------
// The `(Epic #M …)` annotation exclusion (#8251)
// ---------------------------------------------------------------------------

#[test]
fn an_epic_parenthetical_is_annotation_not_a_blocker() {
    // The live shape, from kicad-tools#5520: the verdict names the real blocker
    // and then says, for a human reader, which epic phase that blocker is. The
    // epic stays open for its whole phase lifecycle by design, so capturing it
    // wedged the phase child in DEFER forever — even after #5519 closed.
    let bullet = "- Technical Feasibility: Blocked by #5519 (Epic #5510 Phase 1a — \
                  `RoutingPlan`, sidecar writer, `emit_routing_plan`), still open.";
    assert_eq!(extract_refs(bullet, "o/r"), vec!["o/r#5519".to_string()]);
}

#[test]
fn a_genuine_multi_blocker_bullet_still_yields_every_blocker() {
    // The exclusion keys off the literal `Epic #` marker, never off position:
    // "drop everything after the first ref" would silently lose a real blocker.
    assert_eq!(
        extract_refs("- Blocked by #3 and #4", "o/r"),
        vec!["o/r#3".to_string(), "o/r#4".to_string()]
    );
}

#[test]
fn a_parenthetical_that_is_not_an_epic_annotation_still_over_captures() {
    // Deliberately narrow: only `Epic #`-introduced groups are annotation. The
    // established whole-text scan is otherwise untouched, so `(see also #99)`
    // keeps yielding #99 exactly as every existing fixture reads it.
    assert_eq!(
        extract_refs("- Blocked by #3 (see also #99)", "o/r"),
        vec!["o/r#3".to_string(), "o/r#99".to_string()]
    );
    // And the marker must introduce the group, not merely appear inside it.
    assert_eq!(
        extract_refs("- Blocked by #3 (see Epic #99)", "o/r"),
        vec!["o/r#3".to_string(), "o/r#99".to_string()]
    );
}

#[test]
fn the_exclusion_is_scoped_to_the_parenthetical_span() {
    // Refs before and after the annotation are unaffected, and a ref nested
    // deeper inside the annotation is still annotation.
    assert_eq!(
        extract_refs("- Blocked by #3 (Epic #10 Phase 2 (tracked in #11)) and #4", "o/r"),
        vec!["o/r#3".to_string(), "o/r#4".to_string()]
    );
}

#[test]
fn an_epic_annotation_may_cite_a_qualified_ref_or_a_url() {
    assert_eq!(
        extract_refs(
            "- Blocked by #3 (Epic #10 — see a/b#12 and https://github.com/a/b/issues/13)",
            "o/r"
        ),
        vec!["o/r#3".to_string()]
    );
}

#[test]
fn the_exclusion_is_per_line_so_an_unbalanced_paren_cannot_swallow_later_bullets() {
    // A stray `(` on one bullet must not pair with a `)` several bullets later
    // and silently erase real blockers in between.
    let findings = "- Blocked by #3 (Epic #10 Phase 1a\n- Blocked by #4 (still open)\n";
    assert_eq!(
        extract_refs(findings, "o/r"),
        vec![
            "o/r#10".to_string(),
            "o/r#3".to_string(),
            "o/r#4".to_string()
        ],
        "an unterminated group excludes nothing — only a closed `(Epic # …)` does"
    );
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
