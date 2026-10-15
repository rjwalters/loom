//! `loom:epic` is a work-finder skip label (#11399).
//!
//! An epic is a tracking container: Champion (or the epic supervisor)
//! decomposes it into `loom:epic-phase` children, and only those are build
//! work. Before #11399 a `loom:issue` row that also carried `loom:epic` was
//! dispatched; the session could only skip it, the reaper restored
//! `loom:issue`, and about three sessions were spent before the PR-less retry
//! bound (#7972) parked the epic under a `loom:blocked` that described
//! nothing real.
//!
//! A sibling file, not appended to `tests.rs`: that module is over the
//! 1000-line ratchet threshold and frozen at its current size
//! (`.loom/docs/file-size-policy.md`).

use super::*;

use crate::epic_state::PhaseChild;
use crate::epic_supervisor::{plan_epic_transition, EpicSnapshot, EpicTransition};
use crate::label_registry::Registry;
use crate::work_finder::operator_priority::excluded_from_side_listing;

const EPIC: &str = labels::EPIC_LABEL;
const EPIC_PHASE: &str = "loom:epic-phase";

fn labelled(n: u32, extra: &[&str]) -> WorkItem {
    let mut labels = vec!["loom:issue".to_string()];
    labels.extend(extra.iter().map(|l| (*l).to_string()));
    WorkItem::new(n, labels)
}

#[test]
fn an_epic_carrying_loom_issue_is_a_skipped_candidate() {
    assert!(labelled(1, &[EPIC]).is_skipped(), "default config, no extraSkipLabels");
    assert!(labelled(1, &[EPIC]).is_skipped_with_extra(&[]));
    assert!(
        labelled(2, &[EPIC_PHASE, EPIC]).is_skipped(),
        "a sub-epic phase is a container too"
    );
}

#[test]
fn an_epic_phase_child_is_not_skipped() {
    // Membership is an exact string match: `loom:epic-phase` shares a prefix
    // with `loom:epic` and must stay dispatchable.
    assert!(!SKIP_LABELS.contains(&EPIC_PHASE));
    assert!(!labelled(3, &[EPIC_PHASE]).is_skipped());
    assert!(!labelled(3, &[EPIC_PHASE]).is_skipped_with_capabilities(&[], &BTreeSet::new()));
}

#[test]
fn the_epic_skip_is_not_a_park_and_the_registry_still_validates() {
    // The dispatch-time guard (#4444) keys on PARK_LABELS only, so an
    // operator's explicit `loom-daemon dispatch <N>` on an epic still runs.
    // `sweep_registry::dispatch::operator_hold_tests` covers that route.
    assert!(SKIP_LABELS.contains(&EPIC));
    assert!(!PARK_LABELS.contains(&EPIC));
    let registry = Registry::embedded();
    registry.validate().unwrap();
    let epic = registry.get(EPIC).unwrap();
    assert!(epic.skip && !epic.park && !epic.hold && !epic.operator_hold && !epic.hard_exclusion);
    assert!(epic.champion_path, "an epic keeps its Champion path");
    let phase = registry.get(EPIC_PHASE).unwrap();
    assert!(!phase.skip && !phase.park);
}

#[test]
fn the_mechanical_capability_lane_cannot_dispatch_an_epic() {
    // #6893: the lane may only cancel `loom:blocked` (#11861). An epic is
    // another skip reason, so the item stays skipped even with every
    // capability held.
    let held: BTreeSet<String> = BTreeSet::from(["host-sudo".to_string()]);
    let item = |extra: &[&str]| {
        let mut l = vec!["loom:blocked"];
        l.extend_from_slice(extra);
        labelled(7, &l).with_body(Some("<!-- loom:capability=host-sudo -->".to_string()))
    };
    assert!(
        !item(&[]).is_skipped_with_capabilities(&[], &held),
        "control: without the epic label this host's lane takes the item"
    );
    assert!(item(&[EPIC]).is_skipped_with_capabilities(&[], &held));
}

#[test]
fn a_priority_epic_is_still_dropped_from_the_side_listing() {
    let important_epic = WorkItem::new(20, vec!["loom:very-important".to_string(), EPIC.into()]);
    assert!(excluded_from_side_listing(&important_epic));
}

#[test]
fn the_epic_supervisor_builds_nothing_for_an_epics_ready_children() {
    // An active epic's open `loom:epic-phase` + `loom:issue` children are
    // Builder steps the work finder draws (#11909); the supervisor's own
    // per-child sweep dispatch is gone (#11911), so it plans nothing for them.
    let body = "# Epic\n\n### Phase 1: one\ndetails\n\n### Phase 2: two\ndetails\n";
    let snapshot = EpicSnapshot::new(9, body, vec![PhaseChild::new(1, true)]);
    assert_eq!(plan_epic_transition(&snapshot), EpicTransition::Noop);
}
