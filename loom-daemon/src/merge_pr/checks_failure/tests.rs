//! Tests for the failing-check overlap classification.

use super::*;

fn v(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}

#[test]
fn no_overlap_and_nothing_pending_proceeds() {
    assert_eq!(classify(&v(&["Lint"]), &v(&["CI"]), false), Verdict::InformationalOnly);
}

#[test]
fn no_overlap_but_pending_keeps_waiting() {
    assert_eq!(classify(&v(&["Lint"]), &v(&["CI"]), true), Verdict::StillPending);
}

#[test]
fn required_check_in_failing_set_refuses() {
    let verdict = classify(&v(&["CI", "Lint"]), &v(&["CI"]), false);
    assert_eq!(verdict, Verdict::RequiredFailed(v(&["CI"])));
}

#[test]
fn required_check_failing_refuses_even_when_others_pending() {
    // A required failure can never turn green on this SHA, so it refuses
    // regardless of what else is still running.
    let verdict = classify(&v(&["CI"]), &v(&["CI"]), true);
    assert_eq!(verdict, Verdict::RequiredFailed(v(&["CI"])));
}

#[test]
fn overlap_is_sorted_and_deduplicated() {
    let verdict = classify(&v(&["Zeta", "Alpha", "Alpha", "Zeta"]), &v(&["Alpha", "Zeta"]), false);
    assert_eq!(verdict, Verdict::RequiredFailed(v(&["Alpha", "Zeta"])));
}

#[test]
fn empty_failing_set_has_no_overlap() {
    assert_eq!(classify(&[], &v(&["CI"]), false), Verdict::InformationalOnly);
    assert_eq!(classify(&[], &v(&["CI"]), true), Verdict::StillPending);
}

#[test]
fn empty_required_set_never_overlaps() {
    assert_eq!(classify(&v(&["Lint", "Format"]), &[], false), Verdict::InformationalOnly);
}

#[test]
fn exact_string_match_only_no_substring_match() {
    // A failing check named "CI-lint" must not be treated as an overlap with
    // a required context named "CI" — same "whole-line, never a substring"
    // discipline the sibling label/ref guards hold (#8199).
    assert_eq!(classify(&v(&["CI-lint"]), &v(&["CI"]), false), Verdict::InformationalOnly);
}
