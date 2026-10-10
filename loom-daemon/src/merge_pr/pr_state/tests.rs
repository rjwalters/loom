//! Unit tests for the terminal-state gate: one per row of the retired shell's
//! decision table.

use super::*;

#[test]
fn merged_wins_over_closed() {
    assert_eq!(classify("closed", "true"), PrState::Merged);
    assert_eq!(classify("open", "true"), PrState::Merged);
}

#[test]
fn closed_unmerged_is_closed() {
    assert_eq!(classify("closed", "false"), PrState::Closed);
    assert_eq!(classify("closed", "null"), PrState::Closed);
    assert_eq!(classify("closed", ""), PrState::Closed);
}

#[test]
fn everything_else_is_open() {
    assert_eq!(classify("open", "false"), PrState::Open);
    assert_eq!(classify("null", "null"), PrState::Open);
    assert_eq!(classify("", ""), PrState::Open);
}

#[test]
fn comparison_is_exact_and_case_sensitive() {
    assert_eq!(classify("Closed", "false"), PrState::Open);
    assert_eq!(classify("closed ", "false"), PrState::Open);
    assert_eq!(classify("open", "True"), PrState::Open);
    assert_eq!(classify("open", "true\n"), PrState::Open);
}

#[test]
fn tokens_are_distinct_protocol_lines() {
    assert_eq!(PrState::Merged.token(), "LOOM-PR-STATE MERGED");
    assert_eq!(PrState::Closed.token(), "LOOM-PR-STATE CLOSED");
    assert_eq!(PrState::Open.token(), "LOOM-PR-STATE OPEN");
}
