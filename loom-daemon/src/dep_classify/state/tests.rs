//! Tests for reference-state classification (epic #7810, PR 3).
//!
//! `ref_state` itself talks to a forge, so the end-to-end proof is the shell
//! suite's `gh` stub. What is tested here is the part that decides — the
//! mapping and the split — with the lookup injected, which the shell could only
//! reach through a stub on `PATH`.

use super::*;

#[test]
fn forge_states_map_as_the_shell_case_did() {
    assert_eq!(RefState::from_forge("OPEN"), RefState::Open);
    assert_eq!(RefState::from_forge("CLOSED"), RefState::Resolved);
    assert_eq!(RefState::from_forge("MERGED"), RefState::Resolved);
}

#[test]
fn an_unrecognised_state_is_unknown_not_resolved() {
    // Fails safe. A future forge state must not clear a block by default —
    // that would un-escalate a proposal whose blocker is still live.
    assert_eq!(RefState::from_forge("DRAFT"), RefState::Unknown);
    assert_eq!(RefState::from_forge(""), RefState::Unknown);
    assert_eq!(RefState::from_forge("open"), RefState::Unknown, "case-sensitive");
}

#[test]
fn nodes_are_split_three_ways_preserving_order() {
    let nodes = "o/r#1\no/r#2\no/r#3\no/r#4\n";
    let got = classify_refs_with(nodes, |n| match n {
        "o/r#1" | "o/r#4" => RefState::Open,
        "o/r#2" => RefState::Resolved,
        _ => RefState::Unknown,
    });
    assert_eq!(got.open, vec!["o/r#1", "o/r#4"]);
    assert_eq!(got.resolved, vec!["o/r#2"]);
    assert_eq!(got.unknown, vec!["o/r#3"]);
}

#[test]
fn blank_lines_are_skipped() {
    let got = classify_refs_with("o/r#1\n\n   \no/r#2\n", |_| RefState::Open);
    assert_eq!(got.open, vec!["o/r#1", "o/r#2"]);
}

#[test]
fn an_empty_set_classifies_to_nothing() {
    let got = classify_refs_with("", |_| RefState::Open);
    assert_eq!(got, ClassifiedRefs::default());
    assert_eq!(got.open_joined(), "");
}

#[test]
fn joined_output_is_space_separated_as_the_shell_stored_it() {
    let got = classify_refs_with("o/r#1\no/r#2\n", |_| RefState::Open);
    assert_eq!(got.open_joined(), "o/r#1 o/r#2");
    assert_eq!(got.resolved_joined(), "");
}

#[test]
fn an_unknown_reference_never_lands_in_resolved() {
    // The property the whole three-way split exists to protect: a blocker we
    // could not read must not clear a defer.
    let got = classify_refs_with("o/r#1\n", |_| RefState::Unknown);
    assert!(got.resolved.is_empty(), "got {got:?}");
    assert_eq!(got.unknown, vec!["o/r#1"]);
}

#[test]
fn a_node_without_a_hash_is_unknown_rather_than_panicking() {
    // Defensive: `rsplit_once('#')` returning None must degrade, not crash. A
    // malformed node reaching here means an upstream parser changed, and a
    // panic in Champion's read path is worse than an Unknown.
    let dir = std::env::temp_dir();
    assert_eq!(ref_state("not-a-node", &dir, false), RefState::Unknown);
}
