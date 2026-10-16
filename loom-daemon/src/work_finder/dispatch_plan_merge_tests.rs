//! Coverage for `merge_plans` (Issue #9310).
//!
//! The ordering cases come from the shared fixture
//! `loom-daemon/tests/fixtures/dispatch-plan-merge.json`, which the
//! `2AMLogic/loom-ui` port of the same rule pins too — so a change to the
//! rule on one side fails the other side's test until it is ported. Behaviour the fixture cannot express (that the merge is
//! order-independent, and that it is total on empty input) is tested here.

use super::*;
use crate::types::{HostPlanRow, PlanShard, PlanState};
use crate::work_finder::step_item::Step;

/// The fixture, verbatim. `include_str!` (not a runtime read) so the test
/// binary carries it and a moved fixture is a compile error, the same way
/// `sweep-identity.json` is consumed.
const FIXTURE: &str = include_str!("../../tests/fixtures/dispatch-plan-merge.json");

#[derive(serde::Deserialize)]
struct Fixture {
    cases: Vec<Case>,
}

#[derive(serde::Deserialize)]
struct Case {
    name: String,
    hosts: Vec<HostPlan>,
    expected: Vec<ExpectedItem>,
}

/// One expected fleet item, named by host id rather than by repeating every
/// plan field: the merge picks *which* observation is primary, and the
/// observation itself is carried through unchanged.
#[derive(serde::Deserialize, Debug, PartialEq, Eq)]
struct ExpectedItem {
    repo: String,
    issue: u32,
    /// The item's step by wire name; an expected item without one is a
    /// `builder` item (Issue #11910), as a plan row without one is.
    #[serde(default = "builder")]
    step: String,
    plan_state: PlanState,
    primary: String,
    others: Vec<String>,
}

fn builder() -> String {
    "builder".into()
}

impl ExpectedItem {
    fn of(item: &FleetPlanItem) -> Self {
        ExpectedItem {
            repo: item.repo.clone(),
            issue: item.issue,
            step: item.step.name(),
            plan_state: item.plan_state,
            primary: item.primary.host_id.clone(),
            others: item.others.iter().map(|o| o.host_id.clone()).collect(),
        }
    }
}

fn fixture() -> Fixture {
    serde_json::from_str(FIXTURE).expect("the shared merge fixture parses")
}

#[test]
fn every_fixture_case_merges_to_its_pinned_fleet_plan() {
    for case in fixture().cases {
        let merged = merge_plans(&case.hosts);
        let actual: Vec<ExpectedItem> = merged.items.iter().map(ExpectedItem::of).collect();
        assert_eq!(actual, case.expected, "case: {}", case.name);
    }
}

/// The primary carries its host's own row through unchanged — the merge
/// picks an observation, it never synthesizes one.
#[test]
fn the_primary_keeps_the_hosts_own_row_verbatim() {
    let case = fixture()
        .cases
        .into_iter()
        .find(|c| c.expected.iter().any(|e| e.issue == 100))
        .expect("the headline case is in the fixture");
    let merged = merge_plans(&case.hosts);
    let item = merged
        .items
        .iter()
        .find(|i| i.issue == 100)
        .expect("issue 100 merged");
    assert_eq!(item.primary.host_id, "host-a");
    assert_eq!(item.primary.plan.position, Some(1));
    assert_eq!(item.primary.plan.owning_shard, Some(0));
    // The loser is kept whole too, so the UI can show every host's reason.
    assert_eq!(item.others[0].plan.plan_state, PlanState::Blocked);
    assert_eq!(merged.hosts, vec!["host-a".to_string(), "host-b".to_string()]);
}

/// Host id is the last tie-break everywhere, so the input slice's order
/// cannot change the answer.
#[test]
fn the_merge_does_not_depend_on_the_order_of_the_host_slice() {
    for case in fixture().cases {
        let forward = merge_plans(&case.hosts);
        let mut reversed = case.hosts.clone();
        reversed.reverse();
        assert_eq!(merge_plans(&reversed), forward, "case: {}", case.name);
    }
}

#[test]
fn an_empty_fleet_and_an_empty_host_merge_to_an_empty_plan() {
    assert_eq!(merge_plans(&[]), FleetPlan::default());
    let idle = HostPlan {
        host_id: "host-a".into(),
        shard: PlanShard::default(),
        rows: Vec::new(),
    };
    let merged = merge_plans(std::slice::from_ref(&idle));
    assert!(merged.items.is_empty());
    // An idle host is still a member of the fleet, not an absent one.
    assert_eq!(merged.hosts, vec!["host-a".to_string()]);
}

/// Two hosts listing the same issue number in *different* repos are two
/// items: the fold key is `repo#issue`, not the bare number.
#[test]
fn the_fold_key_is_repo_and_issue_not_the_issue_number_alone() {
    let row = |repo: &str, position: u32| HostPlanRow {
        repo: repo.into(),
        issue: 42,
        plan: RowPlan {
            position: Some(position),
            plan_state: PlanState::Queued,
            ..RowPlan::default()
        },
    };
    let hosts = vec![
        HostPlan {
            host_id: "host-a".into(),
            shard: PlanShard::default(),
            rows: vec![row("acme/one", 1)],
        },
        HostPlan {
            host_id: "host-b".into(),
            shard: PlanShard::default(),
            rows: vec![row("acme/two", 1)],
        },
    ];
    let merged = merge_plans(&hosts);
    assert_eq!(merged.items.len(), 2);
    assert!(merged.items.iter().all(|i| i.others.is_empty()));
}

/// Issue #11910: the merged item carries the resolved step, and an older
/// host's row (no `step`) folds into the same item as a newer host's explicit
/// `builder` row rather than standing beside it.
#[test]
fn an_absent_step_and_an_explicit_builder_step_are_one_item() {
    let row = |step: Option<PlanStep>| HostPlanRow {
        repo: "acme/one".into(),
        issue: 42,
        plan: RowPlan {
            position: Some(1),
            plan_state: PlanState::Queued,
            step,
            ..RowPlan::default()
        },
    };
    let hosts = vec![
        HostPlan {
            host_id: "host-old".into(),
            shard: PlanShard::default(),
            rows: vec![row(None)],
        },
        HostPlan {
            host_id: "host-new".into(),
            shard: PlanShard::default(),
            rows: vec![row(Some(PlanStep::Known(Step::Builder)))],
        },
    ];
    let merged = merge_plans(&hosts);
    assert_eq!(merged.items.len(), 1);
    assert_eq!(merged.items[0].step, PlanStep::Known(Step::Builder));
    assert_eq!(merged.items[0].others.len(), 1);
    // Each observation is carried verbatim: the old host's row still has no
    // `step` of its own.
    let old = std::iter::once(&merged.items[0].primary)
        .chain(&merged.items[0].others)
        .find(|o| o.host_id == "host-old")
        .unwrap();
    assert_eq!(old.plan.step, None);
}
