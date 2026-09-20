use super::*;

#[test]
fn test_operator_hold_skips_candidates_but_is_never_a_park() {
    // vibesql#6664: a sweep that decides "a human is needed" releases its
    // claim (restoring `loom:issue`) and applies `loom:operator` in one
    // motion. The work finder must not start a fresh `--claim-owned` build on
    // that candidate — the incident showed the daemon re-dispatching onto the
    // fresh hold 3x in 13 minutes.
    let held = WorkItem::new(6172, vec!["loom:issue".into(), OPERATOR_HOLD_LABEL.into()]);
    assert!(
        held.is_skipped(),
        "a `loom:issue` candidate carrying `loom:operator` must not dispatch a new builder"
    );

    // ...but the hold is NOT a park: dispatch step 2.7's guard consults
    // PARK_LABELS only, so the re-evaluation routes (watchdog re-dispatch,
    // reaper checkpoint-resume, explicit `loom-daemon dispatch <N>`) keep
    // reaching held items. `label-state-machine.md`'s re-evaluability
    // contract depends on this — graduating the hold into PARK_LABELS would
    // refuse the human's own override path.
    assert!(
        !PARK_LABELS.contains(&OPERATOR_HOLD_LABEL),
        "`loom:operator` is re-evaluable by design; it must never become a PARK_LABELS \
             entry (that is what OPERATOR_HOLD_LABEL in SKIP_LABELS alone expresses)"
    );

    // The #6893 mechanical lane cannot relax the hold either — an item
    // carrying `loom:operator` alongside the mechanical sub-kind stays
    // skipped regardless of held capabilities, matching the documented
    // "loom:operator vetoes the exemption outright" (work_finder.rs,
    // is_skipped_with_capabilities) and LANE_VETO_LABELS.
    let mut held_caps = std::collections::BTreeSet::new();
    held_caps.insert("sql".to_string());
    let lane_item = WorkItem::new(
        6173,
        vec![
            "loom:issue".into(),
            crate::capability::OPERATOR_ONLY_LABEL.into(),
            "loom:operator-mechanical".into(),
            OPERATOR_HOLD_LABEL.into(),
        ],
    );
    assert!(
        lane_item.is_skipped_with_capabilities(&[], &held_caps),
        "a generic `loom:operator` hold vetoes the capability lane (#6893)"
    );
}
