#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::eta::fleet_events::SOURCE_FORGE;
use chrono::Duration;

const REPO: &str = "rjwalters/loom";

fn t(secs: i64) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-09-01T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
        + Duration::seconds(secs)
}

fn ev(
    item: u32,
    kind: ItemKind,
    what: EventKind,
    label: Option<&str>,
    secs: i64,
    seq: u64,
) -> RawEvent {
    RawEvent::new(
        REPO,
        item,
        kind,
        what,
        label.map(str::to_string),
        t(secs),
        SOURCE_FORGE,
        seq,
        t(9_999_999),
    )
}

fn opened(item: u32, kind: ItemKind, secs: i64) -> RawEvent {
    ev(item, kind, EventKind::Opened, None, secs, 0)
}

fn add(item: u32, kind: ItemKind, label: &str, secs: i64, seq: u64) -> RawEvent {
    ev(item, kind, EventKind::LabelAdded, Some(label), secs, seq)
}

fn remove(item: u32, kind: ItemKind, label: &str, secs: i64, seq: u64) -> RawEvent {
    ev(item, kind, EventKind::LabelRemoved, Some(label), secs, seq)
}

/// A small fleet: issue 1 ready then building; issue 2 curated; PR 10 in
/// review then approved; PR 11 approved and held for a human; PR 12 merged;
/// issue 3 blocked on an operator.
fn fleet() -> Vec<RawEvent> {
    use ItemKind::{Issue, Pr};
    vec![
        opened(1, Issue, 0),
        add(1, Issue, "loom:issue", 100, 1),
        remove(1, Issue, "loom:issue", 500, 2),
        add(1, Issue, "loom:building", 500, 3),
        opened(2, Issue, 50),
        add(2, Issue, "loom:curated", 200, 4),
        opened(3, Issue, 60),
        add(3, Issue, "loom:operator-only", 300, 5),
        opened(10, Pr, 600),
        add(10, Pr, "loom:review-requested", 600, 6),
        remove(10, Pr, "loom:review-requested", 900, 7),
        add(10, Pr, "loom:pr", 900, 8),
        opened(11, Pr, 610),
        add(11, Pr, "loom:pr", 700, 9),
        add(11, Pr, "loom:operator", 800, 10),
        opened(12, Pr, 620),
        add(12, Pr, "loom:pr", 650, 11),
        ev(12, Pr, EventKind::Merged, None, 700, 12),
        ev(12, Pr, EventKind::Closed, None, 700, 13),
    ]
}

fn item(state: &FleetState, kind: ItemKind, number: u32) -> &ItemState {
    state
        .items
        .iter()
        .find(|i| i.kind == kind && i.number == number)
        .unwrap_or_else(|| panic!("{kind:?} {number} not open"))
}

#[test]
fn reconstructs_stages_counts_and_time_in_stage() {
    let state = fleet_state(&fleet(), REPO, t(1000));
    assert_eq!(state.schema, STATE_SCHEMA);
    assert_eq!(state.open_issues, 3);
    assert_eq!(state.open_prs, 2, "PR 12 merged");
    assert_eq!(state.building, 1);
    assert_eq!(state.operator_holds, 2);
    assert_eq!(state.held_for_human, 1);
    assert_eq!(state.pr_open_skip_lockout, None);

    let building = item(&state, ItemKind::Issue, 1);
    assert_eq!(building.stage, ItemStage::Building);
    assert_eq!(building.stage_entered_at, t(500));
    assert_eq!(building.time_in_stage_sec, 500);
    assert_eq!(building.opened_at, t(0));
    assert_eq!(item(&state, ItemKind::Issue, 2).stage, ItemStage::Curated);
    let held = item(&state, ItemKind::Issue, 3);
    assert_eq!(held.stage, ItemStage::Blocked);
    assert!(held.operator_hold);
    let approved = item(&state, ItemKind::Pr, 10);
    assert_eq!(approved.stage, ItemStage::MergeWait);
    assert_eq!(approved.stage_entered_at, t(900));
    let human = item(&state, ItemKind::Pr, 11);
    assert_eq!(human.stage, ItemStage::HeldForHuman);
    assert_eq!(human.stage_entered_at, t(800));
    assert_eq!(state.stage_counts["held_for_human"], 1);
    assert_eq!(state.stage_counts["merge_wait"], 1);
}

#[test]
fn an_earlier_instant_sees_the_earlier_fleet() {
    let state = fleet_state(&fleet(), REPO, t(550));
    assert_eq!(state.open_prs, 0, "no PR opened before 550");
    assert_eq!(item(&state, ItemKind::Issue, 1).stage, ItemStage::Building);
    let state = fleet_state(&fleet(), REPO, t(500));
    // The 500s transition is not strictly before `as_of`, so it is unseen.
    let ready = item(&state, ItemKind::Issue, 1);
    assert_eq!(ready.stage, ItemStage::ReadyWait);
    assert_eq!(ready.time_in_stage_sec, 400);
}

#[test]
fn events_at_or_after_the_instant_cannot_change_the_answer() {
    let as_of = t(1000);
    let baseline = serde_json::to_string(&fleet_state(&fleet(), REPO, as_of)).unwrap();

    let mut perturbed = fleet();
    // Exactly at `as_of`, and after it: closes, merges, new items, relabels.
    perturbed.push(ev(1, ItemKind::Issue, EventKind::Closed, None, 1000, 500));
    perturbed.push(remove(10, ItemKind::Pr, "loom:pr", 1000, 501));
    perturbed.push(ev(11, ItemKind::Pr, EventKind::Merged, None, 1001, 502));
    perturbed.push(opened(99, ItemKind::Issue, 1000));
    perturbed.push(add(99, ItemKind::Issue, "loom:building", 5000, 503));
    perturbed.push(add(2, ItemKind::Issue, "loom:blocked", 86_400, 504));
    // And mutate a future event already present into something else.
    perturbed.push(remove(3, ItemKind::Issue, "loom:operator-only", 1000, 505));
    let after = serde_json::to_string(&fleet_state(&perturbed, REPO, as_of)).unwrap();
    assert_eq!(baseline, after);
}

#[test]
fn input_order_does_not_matter() {
    let forward = fleet();
    let mut reversed = forward.clone();
    reversed.reverse();
    let mut doubled = forward.clone();
    doubled.extend(forward.iter().cloned());
    let a = serde_json::to_string(&fleet_state(&forward, REPO, t(1000))).unwrap();
    assert_eq!(a, serde_json::to_string(&fleet_state(&reversed, REPO, t(1000))).unwrap());
    assert_eq!(a, serde_json::to_string(&fleet_state(&doubled, REPO, t(1000))).unwrap());
}

#[test]
fn same_second_label_flips_resolve_by_sequence() {
    use ItemKind::Issue;
    // Remove-then-add and add-then-remove in the same second: only `seq`
    // decides, and it decides the same way every run.
    let events = vec![
        opened(5, Issue, 0),
        add(5, Issue, "loom:issue", 10, 1),
        add(5, Issue, "loom:building", 20, 3),
        remove(5, Issue, "loom:issue", 20, 2),
    ];
    let state = fleet_state(&events, REPO, t(30));
    let it = item(&state, Issue, 5);
    assert_eq!(it.labels, vec!["loom:building".to_string()]);
    assert_eq!(it.stage, ItemStage::Building);
    // Swap the sequences: the add now lands first, then the removal.
    let events = vec![
        opened(5, Issue, 0),
        add(5, Issue, "loom:building", 20, 2),
        remove(5, Issue, "loom:building", 20, 3),
    ];
    let state = fleet_state(&events, REPO, t(30));
    assert_eq!(item(&state, Issue, 5).stage, ItemStage::Untriaged);
}

#[test]
fn reopened_items_are_open_again_and_other_repos_are_ignored() {
    use ItemKind::Issue;
    let mut events = vec![
        opened(7, Issue, 0),
        ev(7, Issue, EventKind::Closed, None, 10, 1),
        ev(7, Issue, EventKind::Reopened, None, 20, 2),
    ];
    let mut foreign = opened(8, Issue, 0);
    foreign.repo = "someone/else".to_string();
    events.push(foreign);
    let state = fleet_state(&events, REPO, t(30));
    assert_eq!(state.open_issues, 1);
    assert_eq!(item(&state, Issue, 7).stage_entered_at, t(20));
    assert_eq!(state.events_read, 3);
}

#[test]
fn an_approved_pr_without_an_operator_label_is_merge_wait_and_the_star_is_not_a_hold() {
    let labels: BTreeSet<String> = ["loom:pr", "loom:operator-priority"]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    assert_eq!(item_stage(ItemKind::Pr, &labels), ItemStage::MergeWait);
    assert!(!operator_hold(&labels));
}
