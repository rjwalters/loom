//! Tests for the `operator-premise` fingerprint (epic #7810, PR 4).

use super::*;

fn r(number: i64, state: &str) -> Ref {
    Ref {
        number,
        state: state.to_string(),
    }
}

#[test]
fn an_all_open_reference_set_is_open_with_no_hash() {
    // The non-event: the premise still holds, so there is nothing to report and
    // nothing to compare against next pass.
    let o = compute(&[r(3, "OPEN"), r(9, "OPEN")]);
    assert_eq!(o.verdict, "open");
    assert!(o.conclusion_hash.is_empty(), "an empty hash is the state, not a missing value");
}

#[test]
fn one_closed_reference_makes_the_premise_stale() {
    let o = compute(&[r(3, "OPEN"), r(9, "CLOSED")]);
    assert_eq!(o.verdict, "stale-premise");
    assert_eq!(o.conclusion_hash.len(), 16);
}

#[test]
fn a_merged_reference_also_makes_it_stale() {
    assert_eq!(compute(&[r(9, "MERGED")]).verdict, "stale-premise");
}

#[test]
fn an_unrecognised_state_counts_as_not_open() {
    // The comparison is `!= "OPEN"`, not a closed-state allowlist. A reference
    // the forge describes some new way is reported for a human to look at
    // rather than silently treated as still-open.
    assert_eq!(compute(&[r(9, "")]).verdict, "stale-premise");
    assert_eq!(compute(&[r(9, "DRAFT")]).verdict, "stale-premise");
}

#[test]
fn no_references_at_all_is_open() {
    let o = compute(&[]);
    assert_eq!(o.verdict, "open");
    assert_eq!(o.refs, "");
    assert!(o.conclusion_hash.is_empty());
}

#[test]
fn reference_input_order_does_not_change_the_hash() {
    let a = compute(&[r(3, "CLOSED"), r(9, "OPEN")]);
    let b = compute(&[r(9, "OPEN"), r(3, "CLOSED")]);
    assert_eq!(a.conclusion_hash, b.conclusion_hash);
}

#[test]
fn reference_lines_sort_lexicographically_not_numerically() {
    // Same trailing `| sort` as dep-recheck's. Changing it would invalidate
    // every persisted marker.
    let o = compute(&[r(9, "CLOSED"), r(10, "OPEN")]);
    assert_eq!(o.refs.lines().next(), Some("10:OPEN"), "{}", o.refs);
}

#[test]
fn a_changed_reference_state_changes_the_hash() {
    let a = compute(&[r(9, "CLOSED")]);
    let b = compute(&[r(9, "MERGED")]);
    assert_ne!(a.conclusion_hash, b.conclusion_hash);
}

#[test]
fn a_stdin_document_decodes_with_state_absent() {
    let i: Input = serde_json::from_str(r#"{"refs":[{"number":9}]}"#).expect("decodes");
    assert_eq!(compute(&i.refs).verdict, "stale-premise");
}
