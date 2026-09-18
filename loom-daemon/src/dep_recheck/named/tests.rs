//! Tests for `named-dependency` (epic #7810, PR 4).

use super::*;

fn dep(number: i64, checked: bool, state: Option<&str>) -> Dep {
    Dep {
        number,
        checked,
        state: state.map(str::to_string),
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
    assert_eq!(parse_entries(body), vec![(3, false)]);
}

#[test]
fn the_section_ends_at_the_next_same_or_shallower_heading() {
    let body = "## Dependencies\n\n- [ ] #3: real\n\n## Notes\n\n- [ ] #99: not a dependency\n";
    assert_eq!(parse_entries(body), vec![(3, false)]);
}

#[test]
fn an_h1_also_ends_the_section() {
    let body = "## Dependencies\n\n- [ ] #3: real\n\n# Appendix\n\n- [ ] #99: no\n";
    assert_eq!(parse_entries(body), vec![(3, false)]);
}

#[test]
fn both_h2_and_h3_are_recognised_as_the_heading() {
    // #7503: Curators file both shapes, and missing one silently produces a
    // false VERDICT=clear.
    for h in ["## Dependencies", "### Dependencies"] {
        assert_eq!(parse_entries(&format!("{h}\n- [ ] #3: x")), vec![(3, false)], "{h}");
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
    assert_eq!(parse_entries(body), vec![(3, false), (4, true), (5, true)]);
}

#[test]
fn a_pr_or_issue_token_before_the_reference_is_tolerated() {
    // #7501. Dropping these silently produces a false VERDICT=clear, which can
    // unblock a Builder that is genuinely blocked — the worse direction.
    let body = "## Dependencies\n- [ ] PR #3: x\n- [ ] Issue #4: y\n- [ ] issue #5: z\n";
    assert_eq!(parse_entries(body), vec![(3, false), (4, false), (5, false)]);
}

#[test]
fn both_bullet_markers_and_leading_indentation_are_accepted() {
    // #8011: the pre-port shell's `_extract_named_deps` matched only a literal
    // `-` bullet; accepting `*` too (valid GitHub task-list syntax) is kept
    // intentionally rather than narrowed back — see `item_re`'s doc.
    let body = "## Dependencies\n  - [ ] #3: x\n* [ ] #4: y\n";
    assert_eq!(parse_entries(body), vec![(3, false), (4, false)]);
}

#[test]
fn an_asterisk_bullet_alone_still_computes_the_verdict_correctly() {
    // #8011's own repro: a `*`-only checklist with one still-OPEN reference.
    let body = "### Dependencies\n* [ ] #123: asterisk bullet\n";
    assert_eq!(parse_entries(body), vec![(123, false)]);
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
        assert_eq!(parse_entries(&body), vec![(6333, false)], "{phrase}");
    }
}

#[test]
fn a_phrased_item_pointing_at_an_open_reference_blocks() {
    // The whole point: the phrasing must reach a verdict, not just parse.
    let body = "## Dependencies\n- [ ] Blocked by #6333: prerequisite feature\n";
    assert_eq!(parse_entries(body), vec![(6333, false)]);
    assert_eq!(compute(&[dep(6333, false, Some("OPEN"))]).verdict, "blocked");
}

#[test]
fn a_phrased_item_pointing_at_a_finished_reference_is_clear() {
    // The other direction, so the fix cannot be "always blocked": once the
    // named reference is done, the same body reports clear.
    let body = "## Dependencies\n- [ ] Blocked by #6333: prerequisite feature\n";
    assert_eq!(parse_entries(body), vec![(6333, false)]);
    for state in ["CLOSED", "MERGED"] {
        assert_eq!(compute(&[dep(6333, false, Some(state))]).verdict, "clear", "{state}");
    }
}

#[test]
fn a_phrase_and_a_pr_token_may_both_precede_the_reference() {
    // The #7501 token and the #8119 phrase are independent optional parts:
    // either, both, or neither. Case is irrelevant to both.
    let body = "## Dependencies\n- [ ] Blocked by PR #3: x\n- [ ] depends on issue #4: y\n";
    assert_eq!(parse_entries(body), vec![(3, false), (4, false)]);
}

#[test]
fn a_phrase_checklist_item_still_honours_its_checkbox() {
    // A phrase does not make an item unconditionally blocking — a ticked box
    // is still the human saying "resolved".
    let body = "## Dependencies\n- [x] Blocked by #3: done\n- [ ] Requires #4: pending\n";
    assert_eq!(parse_entries(body), vec![(3, true), (4, false)]);
}

#[test]
fn a_phrase_later_in_an_items_prose_does_not_add_a_second_reference() {
    // The phrase is only read where a reference token would be — directly
    // after the checkbox. Otherwise `- [ ] #3: blocked by #99` would park work
    // on #99, a number nobody declared as a prerequisite.
    let body = "## Dependencies\n- [ ] #3: blocked by #99 in the original design\n";
    assert_eq!(parse_entries(body), vec![(3, false)]);
}

#[test]
fn prose_naming_a_reference_without_a_checkbox_is_not_an_entry() {
    let body = "## Dependencies\n\nThis depends on #3 in spirit.\n";
    assert_eq!(parse_entries(body), vec![]);
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
