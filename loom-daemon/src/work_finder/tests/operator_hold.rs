use super::*;

/// #11795: `loom:operator` is a park. A hold applied after dispatch must stop
/// every re-dispatch route (watchdog, checkpoint resume, epic supervisor), the
/// #4366 shape `loom:operator-only` used to guard; the human overrides by
/// removing the label. #11861: there is one park set for issues and PRs, so
/// it parks a PR too.
#[test]
fn test_operator_hold_is_a_park() {
    // vibesql#6664: a fresh `loom:issue` candidate carrying the hold is never
    // dispatched.
    let held = WorkItem::new(6172, vec!["loom:issue".into(), OPERATOR_HOLD_LABEL.into()]);
    assert!(held.is_skipped());
    assert!(PARK_LABELS.contains(&OPERATOR_HOLD_LABEL));
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

/// #11861: the capability lane keys on `loom:blocked` + a capability marker +
/// no named blocker + no decision block, and never on `loom:operator` (no role
/// acts on it). Never without a marker.
#[test]
fn test_capability_lane_needs_a_marker_and_no_decision_block() {
    let lane = |labels: Vec<String>, body: &str| {
        !WorkItem::new(6173, labels)
            .with_body(Some(body.to_string()))
            .is_skipped_with_capabilities(&[], &caps())
    };
    let blocked = || vec!["loom:issue".to_string(), BLOCKED_HOLD_LABEL.to_string()];
    let marker = "<!-- loom:capability=host-sudo -->";
    assert!(lane(blocked(), marker), "marker, no decision: propose lane");
    assert!(!lane(blocked(), "no marker here"), "no marker: parked (deps, merge-risk)");
    assert!(
        !lane(blocked(), &format!("{marker}\n```decision\n{{}}\n```")),
        "a decision block is a ruling, never mechanical"
    );
    // Unmigrated needs-capability item: routes.
    assert!(lane(vec!["loom:issue".into(), "loom:needs-capability".into()], marker));
    // The operator hold, under any name, alone or beside the park: never.
    for hold in [
        OPERATOR_HOLD_LABEL,
        "loom:operator-only",
        "loom:operator-mechanical",
    ] {
        assert!(!lane(vec!["loom:issue".into(), hold.into()], marker), "{hold}");
        let mut l = blocked();
        l.push(hold.to_string());
        assert!(!lane(l, marker), "{hold} beside loom:blocked");
    }
    // No host capabilities: the default everywhere, always parked.
    let item = WorkItem::new(6174, blocked()).with_body(Some(marker.to_string()));
    assert!(item.is_skipped_with_capabilities(&[], &std::collections::BTreeSet::new()));
}
