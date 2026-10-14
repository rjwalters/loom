use super::*;

/// #11795: `loom:operator` is an issue-dispatch park. A hold applied after
/// dispatch must stop every re-dispatch route (watchdog, checkpoint resume,
/// epic supervisor), the #4366 shape `loom:operator-only` used to guard; the
/// human overrides by removing the label.
#[test]
fn test_operator_hold_is_an_issue_dispatch_park() {
    // vibesql#6664: a fresh `loom:issue` candidate carrying the hold is never
    // dispatched.
    let held = WorkItem::new(6172, vec!["loom:issue".into(), OPERATOR_HOLD_LABEL.into()]);
    assert!(held.is_skipped());
    assert!(PARK_LABELS.contains(&OPERATOR_HOLD_LABEL));
    // ...but it is not a PR-lane park (#7660: Doctor drains held PRs).
    assert!(!PR_LANE_PARK_LABELS.contains(&OPERATOR_HOLD_LABEL));
}

/// #11795 transition: an item still carrying a retired name is held exactly
/// as its replacement until `labels migrate` relabels it.
#[test]
fn test_unmigrated_hold_names_stay_held() {
    for old in [
        "loom:operator-only",
        "loom:operator-decision",
        "loom:operator-mechanical",
        "loom:operator-blocked",
    ] {
        let item = WorkItem::new(1, vec!["loom:issue".into(), old.into()]);
        assert!(item.is_skipped(), "{old}");
        assert!(PARK_LABELS.contains(&old), "{old}");
        assert!(is_operator_hold(&[old]), "{old}");
        assert!(!is_blocked_hold(&[old]), "{old}");
        assert_eq!(restore_veto(&[old.to_string()], &[]), Some(old), "{old}");
    }
    let cap = WorkItem::new(2, vec!["loom:issue".into(), "loom:needs-capability".into()]);
    assert!(cap.is_skipped());
    assert!(is_blocked_hold(&["loom:needs-capability"]));
    assert!(!is_operator_hold(&["loom:needs-capability"]));
}

fn caps() -> std::collections::BTreeSet<String> {
    ["host-sudo".to_string()].into_iter().collect()
}

/// #11795: the capability lane keys on `loom:operator` + a capability marker
/// + no decision block. Never without a marker.
#[test]
fn test_capability_lane_needs_a_marker_and_no_decision_block() {
    let lane = |labels: Vec<String>, body: &str| {
        !WorkItem::new(6173, labels)
            .with_body(Some(body.to_string()))
            .is_skipped_with_capabilities(&[], &caps())
    };
    let op = || vec!["loom:issue".to_string(), OPERATOR_HOLD_LABEL.to_string()];
    let marker = "<!-- loom:capability=host-sudo -->";
    assert!(lane(op(), marker), "marker, no decision: propose lane");
    assert!(!lane(op(), "no marker here"), "no marker: parked (alerts, merge-risk)");
    assert!(
        !lane(op(), &format!("{marker}\n```decision\n{{}}\n```")),
        "a decision block is a ruling, never mechanical"
    );
    // Unmigrated mechanical item: still routes.
    let old = vec![
        "loom:issue".to_string(),
        "loom:operator-only".to_string(),
        "loom:operator-mechanical".to_string(),
    ];
    assert!(lane(old, marker));
    // Unmigrated decision / operator-blocked by label: never.
    for judgement in ["loom:operator-decision", "loom:operator-blocked"] {
        let mut l = op();
        l.push(judgement.to_string());
        assert!(!lane(l, marker), "{judgement}");
    }
    // Another hold beside it vetoes.
    let mut blocked = op();
    blocked.push("loom:blocked".to_string());
    assert!(!lane(blocked, marker));
    // No host capabilities: the default everywhere, always parked.
    let item = WorkItem::new(6174, op()).with_body(Some(marker.to_string()));
    assert!(item.is_skipped_with_capabilities(&[], &std::collections::BTreeSet::new()));
}
