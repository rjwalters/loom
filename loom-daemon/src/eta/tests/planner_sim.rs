//! The planner preview (#10528, slice b): a fixture roster replanned under a
//! proposed planner config, before/after `start` / `land` ETAs.

use super::as_of;
use super::ready::history_ready;
use crate::eta::heuristics::{LandV1, StartV1};
use crate::eta::planner_sim::{
    changed_keys, preview, replan, Estimators, PlannerKnobs, Preview, Regime, Roster, RosterRow,
};
use crate::types::{DispatchPlanContext, PlanGate, PlanSlots, PlanState, RowPlan};
use chrono::Duration;
use serde_json::{json, Value};

const REPO: &str = "rjwalters/loom";

fn config(max_concurrent: u64, admissions: u64) -> Value {
    json!({
        "terminals": [],
        "autonomous": {"workFinder": {
            "maxConcurrent": max_concurrent,
            "maxAdmissionsPerTick": admissions,
            "intervalSecs": 60
        }}
    })
}

fn plan(state: PlanState, position: Option<u32>, gate: Option<PlanGate>) -> RowPlan {
    RowPlan {
        plan_state: state,
        position,
        gate,
        ..RowPlan::default()
    }
}

/// Four slots, all busy; two admissions per tick; five capacity-deferred rows
/// (two `next`, three `queued`) and one blocked row.
fn roster() -> Roster {
    let cap = Some(PlanGate::Capacity);
    let rows = vec![
        plan(PlanState::Next, Some(1), cap),
        plan(PlanState::Next, Some(2), cap),
        plan(PlanState::Queued, Some(3), cap),
        plan(PlanState::Queued, Some(4), cap),
        plan(PlanState::Blocked, None, None),
        plan(PlanState::Queued, Some(5), cap),
    ];
    Roster {
        context: DispatchPlanContext {
            slots: PlanSlots {
                max_concurrent: 4,
                occupancy: Some(4),
                free: Some(0),
                max_admissions_per_tick: Some(2),
                ..PlanSlots::default()
            },
            tick_interval_secs: Some(60),
            complete: true,
            ..DispatchPlanContext::default()
        },
        at: as_of() - Duration::seconds(30),
        rows: rows
            .into_iter()
            .zip(100u32..)
            .map(|(plan, issue)| RosterRow {
                repo: REPO.to_string(),
                issue,
                plan,
            })
            .collect(),
    }
}

fn run(before: &Value, after: &Value) -> Preview {
    let estimators = Estimators {
        start: &StartV1,
        land: &LandV1,
    };
    preview(
        &roster(),
        &Regime::of("0.19.866", before),
        &Regime::of("0.19.866", after),
        &estimators,
        &history_ready(),
        as_of(),
    )
}

#[test]
fn an_unchanged_config_previews_identical_columns() {
    let p = run(&config(4, 2), &config(4, 2));
    assert_eq!(p.before_version, p.after_version);
    assert!(p.simulated.is_empty() && p.unsimulated.is_empty());
    assert_eq!(p.rows.len(), 6);
    for row in &p.rows {
        assert_eq!(row.before, row.after, "#{} moved with no change", row.issue);
    }
    // Every positioned row is estimated; the blocked one is refused.
    let estimated = p
        .rows
        .iter()
        .filter(|r| r.before.start.p50_sec.is_some())
        .count();
    assert_eq!(estimated, 5);
    let blocked = p.rows.last().unwrap();
    assert_eq!((blocked.issue, blocked.before.position), (104, None));
    assert!(blocked.before.start.no_estimate_reason.is_some());
}

#[test]
fn more_slots_bring_start_and_land_forward() {
    let p = run(&config(4, 2), &config(6, 2));
    assert_ne!(p.before_version, p.after_version, "the columns carry distinct stamps");
    assert_eq!(p.simulated, vec!["maxConcurrent".to_string()]);
    for row in p.rows.iter().filter(|r| r.before.position.is_some()) {
        let (b, a) = (row.before.turnovers.unwrap(), row.after.turnovers.unwrap());
        assert_eq!(a, b.saturating_sub(2), "#{}: two more free slots", row.issue);
        let (bs, as_) = (row.before.start.p50_sec.unwrap(), row.after.start.p50_sec.unwrap());
        assert!(as_ <= bs, "#{}: start {as_} > {bs}", row.issue);
        let (bl, al) = (row.before.land.p50_sec.unwrap(), row.after.land.p50_sec.unwrap());
        assert!(al <= bl, "#{}: land {al} > {bl}", row.issue);
    }
    // The back of the queue needs five turnovers today and three after.
    let last = p.rows.iter().find(|r| r.issue == 105).unwrap();
    assert_eq!((last.before.turnovers, last.after.turnovers), (Some(5), Some(3)));
    assert!(last.after.start.p50_sec < last.before.start.p50_sec);
}

#[test]
fn fewer_slots_push_start_back() {
    let p = run(&config(6, 2), &config(4, 2));
    // The live tick ran with four busy slots: the proposal (4) matches the
    // tick, but differs from the current config (6), so it is applied.
    let front = &p.rows[0];
    assert_eq!(front.before, front.after);
    let narrower = run(&config(4, 2), &config(2, 2));
    for row in narrower.rows.iter().filter(|r| r.before.position.is_some()) {
        assert_eq!(row.after.turnovers, row.before.turnovers, "no slot is free either way");
    }
}

#[test]
fn the_admission_cap_re_marks_next_and_moves_the_admission_delay() {
    let base = roster();
    let rows: Vec<RowPlan> = base.rows.iter().map(|r| r.plan.clone()).collect();
    let (ctx, after) = replan(
        &base.context,
        &rows,
        &PlannerKnobs::from_config(&config(4, 2)),
        &PlannerKnobs::from_config(&config(4, 3)),
    );
    assert_eq!(ctx.slots.max_admissions_per_tick, Some(3));
    let states: Vec<PlanState> = after.iter().map(|r| r.plan_state).collect();
    assert_eq!(
        states,
        vec![
            PlanState::Next,
            PlanState::Next,
            PlanState::Next,
            PlanState::Queued,
            PlanState::Blocked,
            PlanState::Queued,
        ]
    );
    // Five free slots: no turnover is left, only the admission batch.
    let p = run(&config(4, 1), &config(9, 4));
    let last = p.rows.iter().find(|r| r.issue == 105).unwrap();
    assert_eq!(last.after.turnovers, Some(0));
    assert!(last.after.start.p50_sec < last.before.start.p50_sec);
    assert_eq!(
        p.simulated,
        vec![
            "maxConcurrent".to_string(),
            "maxAdmissionsPerTick".to_string()
        ]
    );
}

#[test]
fn occupancy_is_recovered_from_free_when_the_tick_did_not_record_it() {
    let mut base = roster();
    base.context.slots.occupancy = None;
    base.context.slots.free = Some(1);
    let rows: Vec<RowPlan> = base.rows.iter().map(|r| r.plan.clone()).collect();
    let (ctx, _) = replan(
        &base.context,
        &rows,
        &PlannerKnobs::from_config(&config(4, 2)),
        &PlannerKnobs::from_config(&config(5, 2)),
    );
    assert_eq!((ctx.slots.max_concurrent, ctx.slots.free), (5, Some(2)));
}

#[test]
fn unmodelled_planner_changes_are_reported_not_simulated() {
    let before = config(4, 2);
    let mut after = config(4, 2);
    after["autonomous"]["workFinder"]["maxConcurrentPerRepo"] = json!(1);
    after["autonomous"]["mergeSequencing"] = json!({"enabled": true});
    let (simulated, unsimulated) =
        changed_keys(&Regime::of("1", &before), &Regime::of("1", &after));
    assert!(simulated.is_empty());
    assert_eq!(
        unsimulated,
        vec![
            "workFinder.maxConcurrentPerRepo".to_string(),
            "mergeSequencing".to_string()
        ]
    );
    let p = run(&before, &after);
    assert_ne!(p.before_version, p.after_version);
    for row in &p.rows {
        assert_eq!(row.before, row.after);
    }
}

#[test]
fn the_preview_is_deterministic() {
    assert_eq!(run(&config(4, 2), &config(6, 3)), run(&config(4, 2), &config(6, 3)));
}
