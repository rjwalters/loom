//! The shared restore veto (#11480) and the `loom:decision-malformed` skip.
//!
//! A sibling file, not appended to `tests.rs`, to keep that module small.

use super::*;

fn owned(labels: &[&str]) -> Vec<String> {
    labels.iter().map(|l| (*l).to_string()).collect()
}

#[test]
fn every_skip_and_lane_veto_label_vetoes_except_the_claim() {
    for veto in [
        "loom:blocked",
        "loom:operator-only",
        "loom:operator-decision",
        "loom:operator",
        "loom:epic",
        "loom:decision-malformed",
        "loom:needs-capability",
    ] {
        assert_eq!(restore_veto(&owned(&["loom:building", veto]), &[]), Some(veto));
    }
    assert_eq!(restore_veto(&owned(&["loom:building", "loom:important"]), &[]), None);
}

#[test]
fn the_callers_own_label_never_vetoes_its_removal() {
    let labels = owned(&["loom:blocked"]);
    assert_eq!(restore_veto(&labels, &["loom:blocked"]), None);
    let labels = owned(&["loom:blocked", "loom:operator-only"]);
    assert_eq!(restore_veto(&labels, &["loom:blocked"]), Some("loom:operator-only"));
}

#[test]
fn matching_is_exact() {
    assert_eq!(restore_veto(&owned(&["loom:epic-phase", "loom:blocked-unnamed"]), &[]), None);
}

#[test]
fn a_bounced_decision_issue_is_a_skipped_candidate() {
    // loom-ui#2727: `loom:decision-malformed` replaces `loom:operator-decision`,
    // which was the only skip label on the row.
    let row = WorkItem::new(2727, owned(&["loom:issue", "loom:decision-malformed"]));
    assert!(row.is_skipped());
    assert!(row.is_skipped_with_capabilities(&[], &BTreeSet::new()));
}
