//! `eta.dispatch_state` rows (#12918): every classified item, pooled or
//! weight-0, with its exclusion, gate and ladder marker.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};

use super::*;
use crate::step_dispatch::item_draw::{weigh, DrawInput, Exclusion, ItemWeights, WeighContext};
use crate::step_dispatch::outcome_ledger::StepCooldowns;
use crate::step_dispatch::tick::{exclude_builders, exclude_claimed};
use crate::step_role::StepRole;
use crate::sweep_registry::step_spawn::draw_claims::DrawClaims;
use crate::work_finder::step_item::{ItemState, Lane, PassReason, Step, Verdict};

fn t(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

fn now() -> DateTime<Utc> {
    t("2026-10-20T12:00:00Z")
}

fn input(issue: Option<u32>, pr: Option<u32>, verdict: Verdict) -> DrawInput {
    DrawInput {
        item: Item {
            issue,
            pr,
            state: ItemState::Ready,
            verdict,
            lane: Lane::Normal,
            send_back: false,
            story_points: None,
        },
        labels: Vec::new(),
        created_at: Some(t("2026-10-01T00:00:00Z")),
        stage_entry: None,
    }
}

fn workspace(idx: usize, inputs: &[DrawInput], cooldowns: StepCooldowns) -> StepWorkspace {
    let ctx = WeighContext {
        now: now(),
        build_backoff_engaged: false,
        pool_exhausted_roles: BTreeSet::new(),
        workspace_idx: idx,
        cooldowns,
    };
    StepWorkspace {
        idx,
        repo: format!("o/r{idx}"),
        weight: 1,
        in_flight: 0,
        weighing: weigh(inputs, &ctx, &ItemWeights::default()),
        halted: false,
        created_at: Default::default(),
        merge_slots: 1,
        merge_concurrency: 1,
        merges_in_flight: 0,
    }
}

fn find(rows: &[StateRow], number: u32) -> &StateRow {
    rows.iter().find(|r| r.number == number).unwrap()
}

#[test]
fn pooled_and_weight_zero_items_each_get_a_row_with_their_gate() {
    let next = t("2026-10-20T14:00:00Z");
    let mut cooldowns = StepCooldowns::default();
    cooldowns.insert(40, StepRole::ChampionMerge, next);
    let ws = workspace(
        0,
        &[
            // Pooled: a Builder step on issue 1.
            input(Some(1), None, Verdict::Step(Step::Builder)),
            // Cooling down: a not-yet-due merge, keyed on its PR.
            input(Some(4), Some(40), Verdict::Step(Step::ChampionMerge)),
            // A pass.
            input(Some(9), None, Verdict::Pass(PassReason::Frozen)),
        ],
        cooldowns,
    );
    let mut reevals = HashMap::new();
    reevals.insert(
        0,
        HashMap::from([(
            40,
            Reeval {
                step: StepRole::ChampionMerge,
                rung: 2,
                next,
            },
        )]),
    );
    let rows = state_rows(&[ws], &reevals, &PoolKnobs::default());
    assert_eq!(rows.len(), 3, "{rows:#?}");

    let pooled = find(&rows, 1);
    assert!(pooled.in_pool);
    assert_eq!((pooled.artifact, pooled.step.as_deref()), (Artifact::Issue, Some("builder")));
    assert!(pooled.exclusion.is_none() && pooled.gate_until.is_none());
    assert!(pooled.iw.is_some() && pooled.weight.is_some());
    assert!(pooled.reeval_rung.is_none() && pooled.reeval_next.is_none());

    // Cooling: a PR row with the cooldown's end and the marker's rung/next.
    let cooling = find(&rows, 40);
    assert!(!cooling.in_pool);
    assert_eq!(
        (cooling.artifact, cooling.step.as_deref()),
        (Artifact::Pr, Some("champion_merge"))
    );
    assert_eq!(cooling.exclusion.as_deref(), Some("cooling_down"));
    assert_eq!(cooling.gate_until, Some(next));
    assert_eq!((cooling.reeval_rung, cooling.reeval_next), (Some(2), Some(next)));
    assert!(cooling.iw.is_none() && cooling.weight.is_none());

    // A pass has no step and no gate end: absent, never 0.
    let pass = find(&rows, 9);
    assert!(!pass.in_pool);
    assert_eq!((pass.step.as_deref(), pass.exclusion.as_deref()), (None, Some("frozen")));
    assert!(pass.gate_until.is_none());
}

#[test]
fn an_issue_promote_marker_is_read_for_the_issue_row() {
    let next = t("2026-10-20T18:00:00Z");
    let mut cooldowns = StepCooldowns::default();
    cooldowns.insert(2, StepRole::ChampionPromote, next);
    let ws = workspace(
        3,
        &[input(
            Some(2),
            None,
            Verdict::Step(Step::ChampionReevaluate),
        )],
        cooldowns,
    );
    let reevals = HashMap::from([(
        3,
        HashMap::from([(
            2,
            Reeval {
                step: StepRole::ChampionPromote,
                rung: 4,
                next,
            },
        )]),
    )]);
    let rows = state_rows(&[ws], &reevals, &PoolKnobs::default());
    let row = find(&rows, 2);
    assert_eq!(row.artifact, Artifact::Issue);
    assert_eq!(row.ws_idx, 3);
    assert_eq!(row.exclusion.as_deref(), Some("cooling_down"));
    assert_eq!(
        (row.gate_until, row.reeval_rung, row.reeval_next),
        (Some(next), Some(4), Some(next))
    );
}

#[test]
fn items_the_shaping_took_out_of_the_pool_keep_their_exclusion() {
    let mut ws = workspace(
        0,
        &[
            input(Some(1), None, Verdict::Step(Step::Builder)),
            input(Some(2), None, Verdict::Step(Step::Curator)),
            input(Some(3), Some(30), Verdict::Step(Step::ChampionMerge)),
            input(Some(5), Some(50), Verdict::Step(Step::ChampionMerge)),
        ],
        StepCooldowns::default(),
    );
    exclude_claimed(
        &mut ws.weighing,
        &DrawClaims {
            items: [2].into_iter().collect(),
            merges_in_flight: vec![(50, "peer".into())],
            ..DrawClaims::default()
        },
    );
    exclude_builders(&mut ws.weighing);
    let rows = state_rows(&[ws], &HashMap::new(), &PoolKnobs::default());
    let exclusion = |n| find(&rows, n).exclusion.as_deref().map(str::to_string);
    assert_eq!(exclusion(2).as_deref(), Some("claimed"));
    assert_eq!(exclusion(30).as_deref(), Some("merge_in_flight"));
    assert_eq!(exclusion(1).as_deref(), Some("shared_pr"));
    assert!(find(&rows, 50).in_pool, "the merge in flight is not deferred by itself");
    assert_eq!(Exclusion::MergeInFlight.as_str(), "merge_in_flight");
}

#[test]
fn a_halted_workspace_contributes_no_rows_and_there_is_no_row_cap() {
    let many: Vec<DrawInput> = (1..=600)
        .map(|n| input(Some(n), None, Verdict::Step(Step::Builder)))
        .collect();
    let live = workspace(0, &many, StepCooldowns::default());
    let mut halted = workspace(1, &many, StepCooldowns::default());
    halted.halted = true;
    let rows = state_rows(&[live, halted], &HashMap::new(), &PoolKnobs::default());
    assert_eq!(rows.len(), 600);
    assert!(rows.iter().all(|r| r.ws_idx == 0));
}
