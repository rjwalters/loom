//! Unit tests for the discovered-worktree classification. The differential
//! (`tests/merge_pr_discovered_worktree_differential.rs`) compares against the
//! frozen retired shell; these name the individual properties.

use super::*;

#[test]
fn the_primary_checkout_is_noted_and_never_offers_removal_advice() {
    // Even with a sentinel present: primary wins, as the shell's if-order did.
    let (a, lines) = decide("feature/x", "/repo", true, true);
    assert_eq!(a, Action::Note);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].0, Level::Info);
    assert!(lines[0].1.contains("not a removable worktree"));
    assert!(!lines[0].1.contains("git worktree remove"));
    assert!(!lines[0].1.contains("--worktree-path"));
}

#[test]
fn a_managed_worktree_proceeds_to_the_shared_decision_silently() {
    let (a, lines) = decide("feature/x", "/wt", false, true);
    assert_eq!(a, Action::Decide);
    assert!(lines.is_empty());
}

#[test]
fn a_user_owned_worktree_is_never_decided_and_gets_four_warnings() {
    let (a, lines) = decide("feature/x", "/wt", false, false);
    assert_eq!(a, Action::Note);
    assert_eq!(lines.len(), 4);
    assert!(lines.iter().all(|(l, _)| *l == Level::Warning));
    assert!(lines[2].1.contains("--worktree-path '/wt'"));
    assert!(lines[3].1.contains("git worktree remove '/wt'"));
}

#[test]
fn only_decide_can_lead_to_removal() {
    for primary in [false, true] {
        for managed in [false, true] {
            let (a, _) = decide("b", "/p", primary, managed);
            assert_eq!(a == Action::Decide, !primary && managed);
        }
    }
}

#[test]
fn render_is_token_then_level_tab_message() {
    let (a, lines) = decide("b", "/p", true, false);
    assert_eq!(
        render(a, &lines),
        "LOOM-DISCOVERED NOTE\nINFO\tPR branch 'b' is checked out in the primary repository checkout (/p) — not a removable worktree.\n"
    );
}
