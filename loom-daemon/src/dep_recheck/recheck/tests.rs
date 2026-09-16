//! Tests for the `dep-recheck` fingerprint (epic #7810, PR 4).

use super::*;

fn pr(number: i64, state: &str, labels: &[&str], mergeable: &str, status: &str) -> Pr {
    Pr {
        number,
        state: state.to_string(),
        labels: labels.iter().map(|s| (*s).to_string()).collect(),
        mergeable: mergeable.to_string(),
        merge_state_status: status.to_string(),
    }
}

fn open_clean(number: i64) -> Pr {
    pr(number, "OPEN", &[], "MERGEABLE", "CLEAN")
}

fn hash_of(prs: &[Pr]) -> String {
    compute(prs, None, "", "").conclusion_hash
}

#[test]
fn the_same_state_hashes_the_same_twice() {
    let prs = [pr(7, "OPEN", &[], "CONFLICTING", "DIRTY")];
    assert_eq!(hash_of(&prs), hash_of(&prs));
}

#[test]
fn no_prs_is_clear_with_empty_blockers() {
    let o = compute(&[], None, "", "");
    assert_eq!(o.verdict, "clear");
    assert_eq!(o.blockers, "");
}

// ---------------------------------------------------------------------------
// #7281: UNKNOWN fails safe to conflicting
// ---------------------------------------------------------------------------

#[test]
fn an_unknown_mergeable_is_treated_as_conflicting() {
    // GitHub reports UNKNOWN while it is still computing. Reading it as "not
    // conflicting" let a PR clear for one pass and re-block the next, flipping
    // VERDICT with nothing about the PR actually changing.
    let unknown = [pr(7, "OPEN", &[], "UNKNOWN", "UNKNOWN")];
    let conflicting = [pr(7, "OPEN", &[], "CONFLICTING", "DIRTY")];
    assert_eq!(verdict(&unknown), "blocked");
    assert_eq!(
        hash_of(&unknown),
        hash_of(&conflicting),
        "a value flickering through UNKNOWN and back must not change the hash"
    );
}

#[test]
fn an_unknown_on_either_field_alone_is_enough() {
    assert_eq!(verdict(&[pr(7, "OPEN", &[], "UNKNOWN", "CLEAN")]), "blocked");
    assert_eq!(verdict(&[pr(7, "OPEN", &[], "MERGEABLE", "UNKNOWN")]), "blocked");
}

#[test]
fn the_blockers_line_applies_the_same_unknown_rule_as_the_verdict() {
    // Both components must flip at the same boundary, or the hash churns even
    // when VERDICT holds steady — which is the bug, not the verdict itself.
    assert!(blockers(&[pr(7, "OPEN", &[], "UNKNOWN", "CLEAN")]).ends_with(":conflicting"));
}

#[test]
fn a_mergeable_open_pr_with_no_block_label_does_not_block() {
    assert_eq!(verdict(&[open_clean(7)]), "clear");
}

// ---------------------------------------------------------------------------
// #7362: the label component is narrow
// ---------------------------------------------------------------------------

#[test]
fn review_cycle_label_churn_does_not_change_the_hash() {
    // The #6805 incident: 28+ near-duplicate comments in 36 hours, every one
    // reporting the same unchanged verdict, because BLOCKERS folded in the
    // full label set.
    let a = [pr(7, "OPEN", &["loom:pr"], "MERGEABLE", "CLEAN")];
    let b = [pr(
        7,
        "OPEN",
        &["loom:reviewing", "loom:treating", "loom:operator"],
        "MERGEABLE",
        "CLEAN",
    )];
    assert_eq!(hash_of(&a), hash_of(&b));
}

#[test]
fn a_superseding_block_label_does_change_the_hash_and_the_verdict() {
    let plain = [open_clean(7)];
    for label in ["loom:changes-requested", "loom:blocked"] {
        let blocked = [pr(7, "OPEN", &[label], "MERGEABLE", "CLEAN")];
        assert_eq!(verdict(&blocked), "blocked", "{label}");
        assert_ne!(hash_of(&plain), hash_of(&blocked), "{label}");
    }
}

#[test]
fn a_closed_pr_never_blocks_whatever_labels_it_carries() {
    assert_eq!(
        verdict(&[pr(
            7,
            "MERGED",
            &["loom:changes-requested"],
            "CONFLICTING",
            "DIRTY"
        )]),
        "clear"
    );
}

// ---------------------------------------------------------------------------
// Ordering
// ---------------------------------------------------------------------------

#[test]
fn pr_input_order_does_not_change_the_hash() {
    let a = [open_clean(3), open_clean(9)];
    let b = [open_clean(9), open_clean(3)];
    assert_eq!(hash_of(&a), hash_of(&b));
}

#[test]
fn blocker_lines_sort_lexicographically_not_numerically() {
    // `jq sort_by(.number) | sort` — the trailing `sort` wins, and it is a
    // string sort. "More sensible" numeric ordering would change the hash and
    // invalidate every persisted marker.
    let out = blockers(&[open_clean(9), open_clean(10)]);
    assert_eq!(
        out.lines().next(),
        Some("10:OPEN:no-block-label:mergeable"),
        "10 sorts before 9: {out}"
    );
}

// ---------------------------------------------------------------------------
// The caller-supplied pass-throughs
// ---------------------------------------------------------------------------

#[test]
fn the_verdict_override_replaces_the_computed_one() {
    // For the case the script cannot infer: no linked PR at all, where the
    // verdict comes from curator.md's secondary heuristic.
    assert_eq!(compute(&[], Some("blocked"), "", "").verdict, "blocked");
}

#[test]
fn an_empty_override_is_a_no_op_and_the_computation_stands() {
    let prs = [pr(7, "OPEN", &[], "CONFLICTING", "DIRTY")];
    assert_eq!(compute(&prs, Some(""), "", "").verdict, "blocked");
    assert_eq!(compute(&prs, None, "", "").verdict, "blocked");
}

#[test]
fn the_block_reason_folds_into_the_hash_verbatim() {
    let a = compute(&[], Some("blocked"), "doctor cycle exhausted", "");
    let b = compute(&[], Some("blocked"), "Sweep coordination: blocking", "");
    assert_ne!(a.conclusion_hash, b.conclusion_hash);
}

#[test]
fn the_orthogonal_identity_changes_the_hash_when_present_and_not_when_empty() {
    // #6516: an escalation to a diagnosed-but-orthogonal blocker must not be
    // suppressed as "unchanged" — and every existing fingerprint must be
    // unaffected when the field is empty, which is the ordinary case.
    let prs = [open_clean(7)];
    let plain = compute(&prs, None, "", "");
    let with = compute(&prs, None, "", "epic-open-but-complete:owner/repo#14");
    let empty = compute(&prs, None, "", "");
    assert_ne!(plain.conclusion_hash, with.conclusion_hash);
    assert_eq!(plain.conclusion_hash, empty.conclusion_hash);
}

// ---------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------

#[test]
fn a_stdin_document_decodes_with_optional_fields_absent() {
    let i: Input = serde_json::from_str(r#"{"prs":[{"number":7}]}"#).expect("decodes");
    assert_eq!(i.prs[0].number, 7);
    assert!(i.prs[0].state.is_empty());
    // Absent merge state is neither CONFLICTING nor UNKNOWN, so it does not
    // manufacture a block on a PR the fixture said nothing about.
    assert_eq!(verdict(&i.prs), "clear");
}

#[test]
fn the_merge_state_status_field_decodes_by_its_camel_case_name() {
    let i: Input =
        serde_json::from_str(r#"{"prs":[{"number":7,"state":"OPEN","mergeStateStatus":"DIRTY"}]}"#)
            .expect("decodes");
    assert_eq!(verdict(&i.prs), "blocked", "a rename here silently clears every conflicting PR");
}
