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

/// An epic, a sub-epic phase (a container too), a plain phase child and an
/// ordinary issue, all carrying `loom:issue`.
fn rows() -> Vec<WorkItem> {
    vec![
        labelled(1, &[EPIC]),
        labelled(2, &[EPIC_PHASE, EPIC]),
        labelled(3, &[EPIC_PHASE]),
        issue(4),
    ]
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
fn a_tick_dispatches_the_phase_child_and_counts_the_epics_as_labeled_skips() {
    let mut dispatcher = RecordingDispatcher::default();
    let report = tick(&mut FakeSource::once(rows()), &mut dispatcher, 10, false).unwrap();
    assert_eq!(dispatcher.dispatched, vec![3, 4], "neither epic row is dispatched");
    assert_eq!(report.skipped_labeled, 2);
    assert_eq!(report.dispatched, 2);
    assert_eq!(report.errors, 0);
}

#[test]
fn a_multi_workspace_tick_shows_the_epic_as_a_skipped_row_naming_the_label() {
    let mut multi = vec![(FakeSource::once(rows()), RecordingDispatcher::default())];
    let report = tick_multi(&mut multi, &[1], 10, &[false]);
    assert_eq!(multi[0].1.dispatched, vec![3, 4]);
    assert_eq!(report.skipped_labeled, 2);

    // The row stays in the ready queue with its reason, so an operator can
    // see why the epic is not being built. It does not silently vanish.
    let queue = ready_queue::finish(&report.queue, &[]);
    for epic in [1, 2] {
        let row = queue
            .iter()
            .find(|r| r.issue == epic)
            .unwrap_or_else(|| panic!("#{epic} must keep its ready-queue row: {queue:?}"));
        assert_eq!(row.disposition, Qd::Parked, "{row:?}");
        assert_eq!(row.detail.as_deref(), Some(EPIC), "{row:?}");
    }
    for built in [3, 4] {
        let row = queue.iter().find(|r| r.issue == built).unwrap();
        assert_eq!(row.disposition, Qd::Dispatched, "{row:?}");
    }
}

#[test]
fn repeated_ticks_never_dispatch_the_epic() {
    // The pre-#11399 loop re-offered the epic every time the reaper restored
    // `loom:issue`. With the skip there is no first dispatch to restore from.
    let mut dispatcher = RecordingDispatcher::default();
    for _ in 0..4 {
        let mut source = FakeSource::once(vec![labelled(1, &[EPIC])]);
        let report = tick(&mut source, &mut dispatcher, 10, false).unwrap();
        assert_eq!(report.skipped_labeled, 1);
    }
    assert!(dispatcher.dispatched.is_empty());
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
    // #6893: the lane may only cancel `loom:operator-only`. An epic is another
    // skip reason, so the item stays skipped even with every capability held.
    let held: BTreeSet<String> = BTreeSet::from(["host-sudo".to_string()]);
    let item = |extra: &[&str]| {
        let mut l = vec!["loom:operator-only", "loom:operator-mechanical"];
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
fn a_promoted_proposal_on_a_loom_issue_row_stays_dispatchable() {
    // Only `loom:epic` is a skip. The other Champion-path labels can sit on a
    // promoted `loom:issue` legitimately and must keep dispatching.
    let rows: Vec<WorkItem> = ["loom:architect", "loom:hermit", "loom:auditor"]
        .iter()
        .zip(30..)
        .map(|(label, n)| labelled(n, &[label]))
        .collect();
    assert!(rows.iter().all(|r| !r.is_skipped()));
    let mut dispatcher = RecordingDispatcher::default();
    let report = tick(&mut FakeSource::once(rows), &mut dispatcher, 10, false).unwrap();
    assert_eq!(dispatcher.dispatched, vec![30, 31, 32]);
    assert_eq!(report.skipped_labeled, 0);
}

#[test]
fn the_epic_supervisor_still_plans_builds_for_an_epics_ready_children() {
    // The supervisor lists `loom:epic` issues itself and dispatches their
    // open `loom:epic-phase` + `loom:issue` children through the registry. It
    // never goes through the finder's candidate filter, so the skip above
    // changes nothing for it.
    let body = "# Epic\n\n### Phase 1: one\ndetails\n\n### Phase 2: two\ndetails\n";
    let snapshot = EpicSnapshot::new(9, body, vec![PhaseChild::new(1, true)], vec![101, 102]);
    assert_eq!(plan_epic_transition(&snapshot), EpicTransition::BuildChildren(vec![101, 102]));
}
