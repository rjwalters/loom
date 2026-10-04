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

#[test]
fn a_ready_label_outranks_retained_curation_labels() {
    for retained in [
        &["loom:issue", "loom:curated"][..],
        &["loom:issue", "loom:triage"][..],
        &["loom:issue", "loom:curated", "loom:triage"][..],
    ] {
        let labels: BTreeSet<String> = retained.iter().map(|s| (*s).to_string()).collect();
        assert_eq!(item_stage(ItemKind::Issue, &labels), ItemStage::ReadyWait, "{retained:?}");
    }
    let building: BTreeSet<String> = ["loom:issue", "loom:curated", "loom:building"]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    assert_eq!(item_stage(ItemKind::Issue, &building), ItemStage::Building);
}

#[test]
fn a_ready_issue_with_a_retained_curated_label_counts_in_ready_wait() {
    use ItemKind::Issue;
    let events = vec![
        opened(5, Issue, 0),
        add(5, Issue, "loom:triage", 10, 1),
        add(5, Issue, "loom:curated", 20, 2),
        add(5, Issue, "loom:issue", 30, 3),
    ];
    let state = fleet_state(&events, REPO, t(100));
    assert_eq!(item(&state, Issue, 5).stage, ItemStage::ReadyWait);
    assert_eq!(state.stage_counts.get("ready_wait").copied(), Some(1));
}

fn closes(pr: u32, issue: Option<u32>, secs: i64, seq: u64) -> RawEvent {
    let family = issue.map(|_| "closes");
    ev(pr, ItemKind::Pr, EventKind::ClosingRef, family, secs, seq).with_target(issue)
}

/// Issue 4 ready from 100s; PR 20 (closes 4) opened at 300s, merged at 2000s;
/// PR 21 (closes nothing) opened at 310s.
fn lockout_fleet() -> Vec<RawEvent> {
    use ItemKind::{Issue, Pr};
    vec![
        opened(4, Issue, 0),
        add(4, Issue, "loom:issue", 100, 1),
        opened(20, Pr, 300),
        closes(20, Some(4), 300, 9020),
        opened(21, Pr, 310),
        closes(21, None, 310, 9021),
        ev(20, Pr, EventKind::Merged, None, 2000, 0),
        ev(20, Pr, EventKind::Closed, None, 2000, 0),
    ]
}

#[test]
fn an_open_pr_closing_a_ready_issue_is_a_lockout() {
    let state = fleet_state(&lockout_fleet(), REPO, t(1000));
    assert_eq!(state.pr_open_skip_lockout, Some(true));
    assert_eq!(item(&state, ItemKind::Issue, 4).open_pr, Some(20));
    assert_eq!(item(&state, ItemKind::Pr, 20).open_pr, None);
    // Once the PR has merged it locks nothing.
    let state = fleet_state(&lockout_fleet(), REPO, t(3000));
    assert_eq!(state.pr_open_skip_lockout, Some(false));
    assert_eq!(item(&state, ItemKind::Issue, 4).open_pr, None);
}

#[test]
fn an_open_part_of_pr_on_a_ready_issue_is_a_lockout() {
    // The guard counts `Part of #N` (#8940), so the reconstruction must too:
    // epic phase PRs are exactly this shape.
    use ItemKind::{Issue, Pr};
    let events = vec![
        opened(4, Issue, 0),
        add(4, Issue, "loom:issue", 100, 1),
        opened(30, Pr, 300),
        ev(30, Pr, EventKind::ClosingRef, Some("part_of"), 300, 9030).with_target(Some(4)),
    ];
    let state = fleet_state(&events, REPO, t(1000));
    assert_eq!(state.pr_open_skip_lockout, Some(true));
    assert_eq!(item(&state, Issue, 4).open_pr, Some(30));
    // The `closing_ref` label is a phrase family, not an item label.
    assert!(item(&state, Pr, 30).labels.is_empty());
}

#[test]
fn part_of_and_colon_bodies_parsed_from_the_listing_lock_out() {
    use super::super::fleet_events_pulls::parse_pulls;
    for body in ["**Part of:** #4", "Closes: #4", "Part of #4"] {
        let page = serde_json::json!([{
            "id": 9040, "number": 40, "created_at": t(300).to_rfc3339(),
            "closed_at": null, "merged_at": null, "body": body,
        }])
        .to_string();
        let (mut events, _) = parse_pulls(REPO, &page, t(5000)).unwrap();
        events.push(opened(4, ItemKind::Issue, 0));
        events.push(add(4, ItemKind::Issue, "loom:issue", 100, 1));
        let state = fleet_state(&events, REPO, t(1000));
        assert_eq!(state.pr_open_skip_lockout, Some(true), "body: {body:?}");
        assert_eq!(item(&state, ItemKind::Issue, 4).open_pr, Some(40), "body: {body:?}");
    }
}

#[test]
fn the_lockout_is_unknown_until_a_closing_ref_precedes_the_instant() {
    // Before PR 20 existed no closing reference was knowable.
    let state = fleet_state(&lockout_fleet(), REPO, t(200));
    assert_eq!(state.pr_open_skip_lockout, None);
    // A cache that never read the pulls listing cannot say, even later.
    let issue_events_only: Vec<RawEvent> = lockout_fleet()
        .into_iter()
        .filter(|e| e.kind != EventKind::ClosingRef)
        .collect();
    let state = fleet_state(&issue_events_only, REPO, t(1000));
    assert_eq!(state.pr_open_skip_lockout, None);
}

#[test]
fn a_building_issue_with_an_open_pr_is_not_a_lockout() {
    let mut events = lockout_fleet();
    events.push(remove(4, ItemKind::Issue, "loom:issue", 400, 2));
    events.push(add(4, ItemKind::Issue, "loom:building", 400, 3));
    let state = fleet_state(&events, REPO, t(1000));
    assert_eq!(item(&state, ItemKind::Issue, 4).open_pr, Some(20));
    assert_eq!(state.pr_open_skip_lockout, Some(false));
}

#[test]
fn closing_refs_at_or_after_the_instant_cannot_change_the_answer() {
    let as_of = t(1000);
    let baseline = serde_json::to_string(&fleet_state(&lockout_fleet(), REPO, as_of)).unwrap();
    let mut perturbed = lockout_fleet();
    perturbed.push(opened(22, ItemKind::Pr, 1000));
    perturbed.push(closes(22, Some(4), 1000, 9022));
    perturbed.push(closes(21, Some(4), 1500, 9023));
    perturbed.push(ev(20, ItemKind::Pr, EventKind::Closed, None, 1000, 77));
    let after = serde_json::to_string(&fleet_state(&perturbed, REPO, as_of)).unwrap();
    assert_eq!(baseline, after);
    // And an issue-events-only cache stays `null` whatever arrives later.
    let bare: Vec<RawEvent> = fleet();
    let mut late = fleet();
    late.push(closes(10, Some(2), 1000, 9100));
    assert_eq!(
        serde_json::to_string(&fleet_state(&bare, REPO, as_of)).unwrap(),
        serde_json::to_string(&fleet_state(&late, REPO, as_of)).unwrap()
    );
}

/// The labels the PR-stage parity test (#10278) enumerates every subset of:
/// the verdicts, every operator label, the other holds, and the two
/// non-hold flags.
const PARITY_UNIVERSE: [&str; 14] = [
    "loom:pr",
    "loom:review-requested",
    "loom:changes-requested",
    "loom:treating",
    "loom:operator",
    "loom:operator-only",
    "loom:operator-decision",
    "loom:operator-mechanical",
    "loom:operator-blocked",
    "loom:operator-objective",
    "loom:blocked",
    "loom:needs-capability",
    "loom:sequenced",
    "loom:operator-priority",
];

fn label_set(labels: &[&str]) -> BTreeSet<String> {
    labels.iter().map(|s| (*s).to_string()).collect()
}

/// `fleet_state` has no held rule of its own (#10278): over all 2^14 label
/// sets, a PR's stage is `stage_from_pr_labels` mapped name for name, and the
/// operator-hold flag is `pr_flags`' `FLAG_OP_HOLD`.
#[test]
fn pr_stage_and_operator_hold_match_labels_on_every_label_set() {
    use crate::eta::labels::{pr_flags, FLAG_OP_HOLD};
    use crate::eta::NoEstimateReason;
    for mask in 0u32..(1 << PARITY_UNIVERSE.len()) {
        let set: BTreeSet<String> = PARITY_UNIVERSE
            .iter()
            .enumerate()
            .filter(|(i, _)| mask & (1 << i) != 0)
            .map(|(_, l)| (*l).to_string())
            .collect();
        let list: Vec<String> = set.iter().cloned().collect();
        let stage = item_stage(ItemKind::Pr, &set);
        let labelled = stage_from_pr_labels(&list);
        assert_eq!(stage == ItemStage::HeldForHuman, labelled == Ok(Stage::MergeHold), "{set:?}");
        let expected = match labelled {
            Ok(Stage::ReviewWait) => ItemStage::ReviewWait,
            Ok(Stage::Doctor) => ItemStage::Doctor,
            Ok(Stage::MergeWait) => ItemStage::MergeWait,
            Ok(Stage::MergeHold) => ItemStage::HeldForHuman,
            Err(NoEstimateReason::Blocked) => ItemStage::Blocked,
            _ => ItemStage::Unknown,
        };
        assert_eq!(stage, expected, "{set:?}");
        assert_eq!(operator_hold(&set), pr_flags(&list) & FLAG_OP_HOLD != 0, "{set:?}");
    }
}

/// The label sets on which the old `fleet_state` rule and `merge_hold`
/// disagreed (#10278), pinned with explicit stages.
#[test]
fn a_second_hold_or_verdict_beside_an_operator_hold_is_blocked() {
    for (labels, stage) in [
        (&["loom:pr", "loom:operator", "loom:blocked"][..], ItemStage::Blocked),
        (&["loom:pr", "loom:operator", "loom:needs-capability"][..], ItemStage::Blocked),
        (&["loom:pr", "loom:changes-requested", "loom:operator"][..], ItemStage::Blocked),
        (&["loom:pr", "loom:operator-mechanical"][..], ItemStage::Blocked),
        (&["loom:pr", "loom:operator", "loom:sequenced"][..], ItemStage::HeldForHuman),
        (
            &["loom:pr", "loom:operator-only", "loom:operator-mechanical"][..],
            ItemStage::HeldForHuman,
        ),
        (&["loom:pr", "loom:operator-priority"][..], ItemStage::MergeWait),
        (&["loom:treating"][..], ItemStage::Doctor),
    ] {
        assert_eq!(item_stage(ItemKind::Pr, &label_set(labels)), stage, "{labels:?}");
    }
}

/// An issue is `Blocked` exactly when serving refuses it as blocked (#10278).
#[test]
fn issue_blocked_matches_unstarted_issue_reason_on_every_label_set() {
    use crate::eta::labels::unstarted_issue_reason;
    use crate::eta::NoEstimateReason;
    const UNIVERSE: [&str; 15] = [
        "loom:issue",
        "loom:building",
        "loom:curated",
        "loom:curating",
        "loom:triage",
        "loom:operator",
        "loom:operator-only",
        "loom:operator-decision",
        "loom:operator-mechanical",
        "loom:operator-blocked",
        "loom:operator-objective",
        "loom:blocked",
        "loom:needs-capability",
        "loom:sequenced",
        "loom:operator-priority",
    ];
    for mask in 0u32..(1 << UNIVERSE.len()) {
        let set: BTreeSet<String> = UNIVERSE
            .iter()
            .enumerate()
            .filter(|(i, _)| mask & (1 << i) != 0)
            .map(|(_, l)| (*l).to_string())
            .collect();
        let list: Vec<String> = set.iter().cloned().collect();
        assert_eq!(
            item_stage(ItemKind::Issue, &set) == ItemStage::Blocked,
            unstarted_issue_reason(&list, None) == Some(NoEstimateReason::Blocked),
            "{set:?}"
        );
    }
}

/// An operator sub-kind without its `loom:operator-only` base no longer
/// blocks an issue; with the base it still does (#10278).
#[test]
fn an_issue_sub_kind_blocks_only_beside_its_operator_only_base() {
    for (labels, stage) in [
        (&["loom:issue", "loom:operator-blocked"][..], ItemStage::ReadyWait),
        (
            &["loom:issue", "loom:operator-only", "loom:operator-blocked"][..],
            ItemStage::Blocked,
        ),
    ] {
        assert_eq!(item_stage(ItemKind::Issue, &label_set(labels)), stage, "{labels:?}");
    }
}

#[test]
fn lifting_the_operator_hold_returns_an_approved_pr_to_merge_wait() {
    use ItemKind::Pr;
    let events = vec![
        opened(20, Pr, 0),
        add(20, Pr, "loom:pr", 10, 1),
        add(20, Pr, "loom:operator", 20, 2),
        remove(20, Pr, "loom:operator", 30, 3),
    ];
    let held = fleet_state(&events, REPO, t(25));
    assert_eq!(item(&held, Pr, 20).stage, ItemStage::HeldForHuman);
    assert_eq!((held.held_for_human, held.operator_holds), (1, 1));
    let lifted = fleet_state(&events, REPO, t(40));
    let pr = item(&lifted, Pr, 20);
    assert_eq!((pr.stage, pr.stage_entered_at), (ItemStage::MergeWait, t(30)));
    assert_eq!((lifted.held_for_human, lifted.operator_holds), (0, 0));
}
