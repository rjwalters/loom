//! Tests for `named-dependency` (epic #7810, PR 4).

use super::*;

fn dep(number: i64, checked: bool, state: Option<&str>) -> Dep {
    Dep {
        number,
        repo: None,
        checked,
        state: state.map(str::to_string),
    }
}

/// The #8502 cross-repo shape: `owner/repo#N`.
fn dep_in(repo: &str, number: i64, checked: bool, state: Option<&str>) -> Dep {
    Dep {
        repo: Some(repo.to_string()),
        ..dep(number, checked, state)
    }
}

// ---------------------------------------------------------------------------
// The verdict
// ---------------------------------------------------------------------------

#[test]
fn an_unchecked_open_dependency_blocks() {
    assert_eq!(verdict(&[dep(3, false, Some("OPEN"))]), "blocked");
}

#[test]
fn both_ways_of_being_finished_count_as_resolved() {
    // curator.md's "When Dependencies Complete": a closed-without-merging
    // reference is as resolved as a merged one.
    assert_eq!(verdict(&[dep(3, false, Some("MERGED"))]), "clear");
    assert_eq!(verdict(&[dep(3, false, Some("CLOSED"))]), "clear");
}

#[test]
fn a_checked_box_is_resolved_whatever_state_it_carries() {
    // Whoever edited the checklist said so; no live lookup is performed, and
    // any state that rides along is never consulted.
    assert_eq!(verdict(&[dep(3, true, Some("OPEN"))]), "clear");
    assert_eq!(deps_lines(&[dep(3, true, Some("OPEN"))]), "3:checked");
}

#[test]
fn no_dependencies_at_all_is_clear() {
    let o = compute(&[]);
    assert_eq!(o.verdict, "clear");
    assert_eq!(o.deps, "");
}

#[test]
fn one_open_dependency_among_resolved_ones_still_blocks() {
    assert_eq!(
        verdict(&[
            dep(3, true, None),
            dep(4, false, Some("MERGED")),
            dep(5, false, Some("OPEN")),
        ]),
        "blocked"
    );
}

// ---------------------------------------------------------------------------
// Hashing
// ---------------------------------------------------------------------------

#[test]
fn dependency_lines_sort_lexicographically_not_numerically() {
    let o = compute(&[dep(9, false, Some("OPEN")), dep(10, false, Some("OPEN"))]);
    assert_eq!(o.deps.lines().next(), Some("10:OPEN"), "{}", o.deps);
}

#[test]
fn input_order_does_not_change_the_hash() {
    let a = compute(&[dep(3, false, Some("OPEN")), dep(9, true, None)]);
    let b = compute(&[dep(9, true, None), dep(3, false, Some("OPEN"))]);
    assert_eq!(a.conclusion_hash, b.conclusion_hash);
}

#[test]
fn a_dependency_closing_changes_the_hash() {
    let a = compute(&[dep(3, false, Some("OPEN"))]);
    let b = compute(&[dep(3, false, Some("CLOSED"))]);
    assert_ne!(a.conclusion_hash, b.conclusion_hash);
}

// ---------------------------------------------------------------------------
// Section scoping
// ---------------------------------------------------------------------------

#[test]
fn only_the_dependencies_section_is_parsed() {
    // An unrelated checklist elsewhere in the issue must not become a
    // dependency — that would park work on arbitrary numbers.
    let body = "## Acceptance\n\n- [ ] #99: an acceptance item\n\n\
                ## Dependencies\n\n- [ ] #3: the real prerequisite\n";
    assert_eq!(parse_entries(body), vec![dep(3, false, None)]);
}

#[test]
fn the_section_ends_at_the_next_same_or_shallower_heading() {
    let body = "## Dependencies\n\n- [ ] #3: real\n\n## Notes\n\n- [ ] #99: not a dependency\n";
    assert_eq!(parse_entries(body), vec![dep(3, false, None)]);
}

#[test]
fn an_h1_also_ends_the_section() {
    let body = "## Dependencies\n\n- [ ] #3: real\n\n# Appendix\n\n- [ ] #99: no\n";
    assert_eq!(parse_entries(body), vec![dep(3, false, None)]);
}

#[test]
fn both_h2_and_h3_are_recognised_as_the_heading() {
    // #7503: Curators file both shapes, and missing one silently produces a
    // false VERDICT=clear.
    for h in ["## Dependencies", "### Dependencies"] {
        assert_eq!(parse_entries(&format!("{h}\n- [ ] #3: x")), vec![dep(3, false, None)], "{h}");
    }
}

#[test]
fn a_heading_with_trailing_words_is_not_the_dependencies_section() {
    // The shell anchors the pattern at end-of-line. `## Dependencies (deferred)`
    // is a different section, and treating it as this one would read its items
    // as live prerequisites.
    assert_eq!(parse_entries("## Dependencies (deferred)\n- [ ] #3: x"), vec![]);
}

#[test]
fn a_body_with_no_dependencies_section_yields_nothing() {
    assert_eq!(parse_entries("## Summary\n\nNo checklist here.\n"), vec![]);
    assert_eq!(compute(&[]).verdict, "clear");
}

// ---------------------------------------------------------------------------
// Item syntax
// ---------------------------------------------------------------------------

#[test]
fn checked_and_unchecked_boxes_are_distinguished() {
    let body = "## Dependencies\n- [ ] #3: pending\n- [x] #4: done\n- [X] #5: done\n";
    assert_eq!(
        parse_entries(body),
        vec![dep(3, false, None), dep(4, true, None), dep(5, true, None)]
    );
}

#[test]
fn a_pr_or_issue_token_before_the_reference_is_tolerated() {
    // #7501. Dropping these silently produces a false VERDICT=clear, which can
    // unblock a Builder that is genuinely blocked — the worse direction.
    let body = "## Dependencies\n- [ ] PR #3: x\n- [ ] Issue #4: y\n- [ ] issue #5: z\n";
    assert_eq!(
        parse_entries(body),
        vec![
            dep(3, false, None),
            dep(4, false, None),
            dep(5, false, None)
        ]
    );
}

#[test]
fn both_bullet_markers_and_leading_indentation_are_accepted() {
    // #8011: the pre-port shell's `_extract_named_deps` matched only a literal
    // `-` bullet; accepting `*` too (valid GitHub task-list syntax) is kept
    // intentionally rather than narrowed back — see `item_re`'s doc.
    let body = "## Dependencies\n  - [ ] #3: x\n* [ ] #4: y\n";
    assert_eq!(parse_entries(body), vec![dep(3, false, None), dep(4, false, None)]);
}

#[test]
fn an_asterisk_bullet_alone_still_computes_the_verdict_correctly() {
    // #8011's own repro: a `*`-only checklist with one still-OPEN reference.
    let body = "### Dependencies\n* [ ] #123: asterisk bullet\n";
    assert_eq!(parse_entries(body), vec![dep(123, false, None)]);
    let o = compute(&[dep(123, false, Some("OPEN"))]);
    assert_eq!(o.verdict, "blocked");
}

#[test]
fn a_dependency_phrase_before_the_reference_is_recognised() {
    // #8119. `extract`'s `phrase_re` already reads every one of these as a
    // reference; this matcher used to drop them, so the two subcommands
    // disagreed and the one deciding the verdict was the blind one.
    for phrase in ["Blocked by", "Depends on", "Requires", "**Epic**"] {
        let body = format!("## Dependencies\n- [ ] {phrase} #6333: prerequisite feature\n");
        assert_eq!(parse_entries(&body), vec![dep(6333, false, None)], "{phrase}");
    }
}

#[test]
fn a_phrased_item_pointing_at_an_open_reference_blocks() {
    // The whole point: the phrasing must reach a verdict, not just parse.
    let body = "## Dependencies\n- [ ] Blocked by #6333: prerequisite feature\n";
    assert_eq!(parse_entries(body), vec![dep(6333, false, None)]);
    assert_eq!(compute(&[dep(6333, false, Some("OPEN"))]).verdict, "blocked");
}

#[test]
fn a_phrased_item_pointing_at_a_finished_reference_is_clear() {
    // The other direction, so the fix cannot be "always blocked": once the
    // named reference is done, the same body reports clear.
    let body = "## Dependencies\n- [ ] Blocked by #6333: prerequisite feature\n";
    assert_eq!(parse_entries(body), vec![dep(6333, false, None)]);
    for state in ["CLOSED", "MERGED"] {
        assert_eq!(compute(&[dep(6333, false, Some(state))]).verdict, "clear", "{state}");
    }
}

#[test]
fn a_phrase_and_a_pr_token_may_both_precede_the_reference() {
    // The #7501 token and the #8119 phrase are independent optional parts:
    // either, both, or neither. Case is irrelevant to both.
    let body = "## Dependencies\n- [ ] Blocked by PR #3: x\n- [ ] depends on issue #4: y\n";
    assert_eq!(parse_entries(body), vec![dep(3, false, None), dep(4, false, None)]);
}

#[test]
fn a_phrase_checklist_item_still_honours_its_checkbox() {
    // A phrase does not make an item unconditionally blocking — a ticked box
    // is still the human saying "resolved".
    let body = "## Dependencies\n- [x] Blocked by #3: done\n- [ ] Requires #4: pending\n";
    assert_eq!(parse_entries(body), vec![dep(3, true, None), dep(4, false, None)]);
}

#[test]
fn a_phrase_later_in_an_items_prose_does_not_add_a_second_reference() {
    // The phrase is only read where a reference token would be — directly
    // after the checkbox. Otherwise `- [ ] #3: blocked by #99` would park work
    // on #99, a number nobody declared as a prerequisite.
    let body = "## Dependencies\n- [ ] #3: blocked by #99 in the original design\n";
    assert_eq!(parse_entries(body), vec![dep(3, false, None)]);
}

#[test]
fn prose_naming_a_reference_without_a_checkbox_is_not_an_entry() {
    let body = "## Dependencies\n\nThis depends on #3 in spirit.\n";
    assert_eq!(parse_entries(body), vec![]);
}

// ---------------------------------------------------------------------------
// Cross-repo references (#8502)
// ---------------------------------------------------------------------------

#[test]
fn a_cross_repo_reference_is_parsed_and_carries_its_repo() {
    // #8502's live repro, byte-for-byte: 2AMLogic/2am#532's checklist named an
    // upstream prerequisite as `owner/repo#N`, which the pre-fix regex could
    // not match AT ALL — not mis-parsed, invisible. DEPS came back empty and
    // VERDICT=clear while rjwalters/loom#8257 was genuinely still OPEN.
    let body = "## Dependencies\n\n- [ ] rjwalters/loom#8257: \"dashboard: add an \
                `ephemeral_compute` record type\" — still **OPEN** as of 2026-09-20\n";
    assert_eq!(parse_entries(body), vec![dep_in("rjwalters/loom", 8257, false, None)]);
}

#[test]
fn an_open_cross_repo_dependency_blocks_and_a_closed_one_clears() {
    // The whole acceptance criterion in one test: blocked while the upstream
    // reference is OPEN, clear only once it is not — so the fix cannot be
    // "a cross-repo item always blocks".
    let d = dep_in("rjwalters/loom", 8257, false, Some("OPEN"));
    assert_eq!(verdict(std::slice::from_ref(&d)), "blocked");
    assert_eq!(deps_lines(std::slice::from_ref(&d)), "rjwalters/loom#8257:OPEN");
    for state in ["CLOSED", "MERGED"] {
        let closed = dep_in("rjwalters/loom", 8257, false, Some(state));
        assert_eq!(verdict(std::slice::from_ref(&closed)), "clear", "{state}");
    }
}

#[test]
fn a_cross_repo_dependency_changing_state_changes_the_hash() {
    // Exactly as a same-repo one already does — the cross-repo rendering has
    // to carry the state into the hash, or a Curator would never see the
    // upstream issue closing.
    let open = compute(&[dep_in("rjwalters/loom", 8257, false, Some("OPEN"))]);
    let closed = compute(&[dep_in("rjwalters/loom", 8257, false, Some("CLOSED"))]);
    assert_ne!(open.conclusion_hash, closed.conclusion_hash);
}

#[test]
fn the_same_number_in_two_repos_is_two_distinct_dependencies() {
    // The reason the rendered line is qualified rather than reduced to a bare
    // number: `owner/a#5` and `owner/b#5` must not collapse into one entry,
    // and one of them closing must move the hash.
    let both = compute(&[
        dep_in("owner/a", 5, false, Some("OPEN")),
        dep_in("owner/b", 5, false, Some("OPEN")),
    ]);
    assert_eq!(both.deps, "owner/a#5:OPEN\nowner/b#5:OPEN");
    let one_closed = compute(&[
        dep_in("owner/a", 5, false, Some("CLOSED")),
        dep_in("owner/b", 5, false, Some("OPEN")),
    ]);
    assert_eq!(one_closed.verdict, "blocked");
    assert_ne!(both.conclusion_hash, one_closed.conclusion_hash);
}

#[test]
fn a_same_repo_dependency_still_renders_a_bare_number() {
    // Backward compatibility is load-bearing: qualifying an existing same-repo
    // line would move CONCLUSION_HASH for every issue already being re-checked
    // and manufacture the very churn this subcommand exists to stop (#7281).
    assert_eq!(deps_lines(&[dep(3, false, Some("OPEN"))]), "3:OPEN");
    assert_eq!(deps_lines(&[dep(3, true, None)]), "3:checked");
}

#[test]
fn a_cross_repo_reference_combines_with_the_phrase_and_token_forms() {
    // #8119's phrases and #7501's `PR `/`Issue ` token are independent of the
    // #8502 prefix: any combination of the three must reach the same entry.
    let body = "## Dependencies\n\
                - [ ] Blocked by owner/repo#1: a\n\
                - [ ] Depends on PR owner/repo#2: b\n\
                - [ ] requires issue owner/repo#3: c\n\
                - [ ] Issue owner/repo#4: d\n\
                - [x] owner/repo#5: already done\n";
    assert_eq!(
        parse_entries(body),
        vec![
            dep_in("owner/repo", 1, false, None),
            dep_in("owner/repo", 2, false, None),
            dep_in("owner/repo", 3, false, None),
            dep_in("owner/repo", 4, false, None),
            dep_in("owner/repo", 5, true, None),
        ]
    );
}

#[test]
fn repository_names_with_dots_hyphens_and_underscores_are_accepted() {
    // Real repo names are not all `[a-z]+`: `rjwalters/loom`, `2AMLogic/2am`,
    // `some-org/my_tool.v2` are all legal forge spellings.
    let body = "## Dependencies\n\
                - [ ] 2AMLogic/2am#532: a\n\
                - [ ] some-org/my_tool.v2#7: b\n";
    assert_eq!(
        parse_entries(body),
        vec![
            dep_in("2AMLogic/2am", 532, false, None),
            dep_in("some-org/my_tool.v2", 7, false, None)
        ]
    );
}

#[test]
fn a_bare_reference_is_still_repo_less_not_falsely_qualified() {
    // The prefix is OPTIONAL. A bare `#N` must keep `repo: None` so the live
    // lookup falls back to the invoking repo — qualifying it with a guess
    // would send every existing same-repo lookup somewhere else.
    assert_eq!(parse_entries("## Dependencies\n- [ ] #3: x"), vec![dep(3, false, None)]);
    assert_eq!(dep(3, false, None).repo, None);
}

#[test]
fn a_cross_repo_entry_decodes_from_stdin() {
    // The `--stdin` document is the fixture shape for this subcommand, so the
    // new field has to round-trip through it too; omitting it stays same-repo.
    let i: Input = serde_json::from_str(
        r#"{"deps":[{"number":8257,"repo":"rjwalters/loom","checked":false,"state":"OPEN"},
                    {"number":3,"checked":false,"state":"CLOSED"}]}"#,
    )
    .unwrap();
    assert_eq!(compute(&i.deps).deps, "3:CLOSED\nrjwalters/loom#8257:OPEN");
    assert_eq!(verdict(&i.deps), "blocked");
}

// ---------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------

#[test]
fn a_checked_entry_may_omit_state_entirely_or_set_it_null() {
    for doc in [
        r#"{"deps":[{"number":3,"checked":true}]}"#,
        r#"{"deps":[{"number":3,"checked":true,"state":null}]}"#,
    ] {
        let i: Input = serde_json::from_str(doc).expect(doc);
        assert_eq!(compute(&i.deps).deps, "3:checked", "{doc}");
    }
}

#[test]
fn an_unchecked_entry_with_no_state_renders_null_and_does_not_block() {
    // Matches the shell's `"\(.number):\(.state)"` on a null — and a state
    // nobody reported is not OPEN, so it cannot manufacture a block.
    let i: Input = serde_json::from_str(r#"{"deps":[{"number":3,"checked":false}]}"#).unwrap();
    assert_eq!(compute(&i.deps).deps, "3:null");
    assert_eq!(verdict(&i.deps), "clear");
}
