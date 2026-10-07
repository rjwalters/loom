//! Planner preview (#10528, slice b): what a proposed planner config would do
//! to the served `start` / `land` ETAs of the live ready roster, before it
//! ships (`loom-daemon eta simulate --planner <config>`).
//!
//! The roster is the last work-finder tick's dispatch plan (#9288), the same
//! rows the tracker turns into `ready_wait` items (#9326). Each side of the
//! preview is a *regime*: a planner config's knobs plus its
//! [`planner_version`] stamp (slice a), which labels the side's column.
//!
//! # What is simulated
//!
//! [`replan`] re-derives the tick's plan under the proposed knobs, changing
//! only what a knob that **differs** from the current config moves:
//!
//! - `maxConcurrent`: the slot count, so free slots become `cap − occupancy`
//!   (occupancy recovered from `free` when the tick did not record it).
//! - `maxAdmissionsPerTick`: the per-tick admission cap, and which capacity-
//!   or ramp-deferred rows are `next` (the first `cap` in plan order, as
//!   `dispatch_plan::promote_next` marks them).
//! - `intervalSecs`: the tick interval.
//!
//! Every row of both sides then goes through the one
//! [`crate::eta::tracker::dispatch_input`] the tracker serves from, and the
//! current `start` / `land` heuristics estimate it against the same history at
//! the same instant. The Monte Carlo seed depends on the subject, kind,
//! heuristic and instant only, so both sides draw identical uniforms: every
//! delta is the planner change, never sampling noise. A proposed config equal
//! to the current one yields identical columns.
//!
//! # What is not (yet)
//!
//! Plan *order* is the work finder's comparator, which no config knob
//! changes, so positions never move. `maxConcurrentPerRepo` is reported in
//! [`Preview::unsimulated`] rather than modelled (it needs per-repo occupancy
//! the plan does not carry), as is any other changed planner key. The live
//! tick's dynamic cap clamp is not re-run: a changed `maxConcurrent` is taken
//! as the effective cap. Started items (PR stages) are out of scope here; the
//! planner-in-the-loop for PR stages is a later slice of #10528.

use super::explanation::Explanation;
use super::planner_version::{planner_version, PlannerConfigView};
use super::tracker::{dispatch_input, waiting_position};
use super::{
    AgeSource, CurrentStage, CurrentState, DispatchInput, EstimateInput, Heuristic,
    NoEstimateReason, Provenance, Stage, Subject,
};
use crate::eta::history::StageSamples;
use crate::types::{DispatchPlanContext, PlanGate, PlanState, RowPlan};
use crate::work_finder::config::parse_effective;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;

/// The planner knobs a preview models, as one config sets them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PlannerKnobs {
    /// `autonomous.workFinder.maxConcurrent`.
    pub max_concurrent: Option<usize>,
    /// `autonomous.workFinder.maxAdmissionsPerTick`.
    pub max_admissions_per_tick: Option<usize>,
    /// `autonomous.workFinder.intervalSecs`.
    pub interval_secs: Option<u64>,
    /// `autonomous.workFinder.maxConcurrentPerRepo` (reported, not modelled).
    pub max_concurrent_per_repo: Option<usize>,
}

impl PlannerKnobs {
    /// The knobs a whole `.loom/config.json` document sets, parsed exactly as
    /// the work finder parses them (a zero or invalid value is absent).
    #[must_use]
    pub fn from_config(config: &Value) -> Self {
        let wf = parse_effective(config);
        PlannerKnobs {
            max_concurrent: wf.max_concurrent,
            max_admissions_per_tick: wf.max_admissions_per_tick,
            interval_secs: wf.interval_secs,
            max_concurrent_per_repo: wf.max_concurrent_per_repo,
        }
    }
}

/// One side of a preview: a planner config's knobs and its stamp.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Regime {
    /// [`planner_version`] of the config.
    pub planner_version: String,
    /// Its knobs.
    pub knobs: PlannerKnobs,
    /// Planner-relevant view (for the unsimulated-change report).
    #[serde(skip)]
    pub view: PlannerConfigView,
}

impl Regime {
    /// The regime a whole config document describes, stamped with `version`.
    #[must_use]
    pub fn of(version: &str, config: &Value) -> Self {
        let view = PlannerConfigView::from_config(config);
        Regime {
            planner_version: planner_version(version, &view),
            knobs: PlannerKnobs::from_config(config),
            view,
        }
    }
}

/// The value `after` overrides: set there, and different from `before`.
fn changed<T: PartialEq + Copy>(before: Option<T>, after: Option<T>) -> Option<T> {
    after.filter(|a| before != Some(*a))
}

/// Config keys `after` changes relative to `before`, in a fixed order:
/// `(simulated, unsimulated)`. A planner-relevant change outside the modelled
/// knobs (`maxConcurrentPerRepo`, sequencing, anything else in the
/// `workFinder` block) is unsimulated.
#[must_use]
pub fn changed_keys(before: &Regime, after: &Regime) -> (Vec<String>, Vec<String>) {
    let (b, a) = (&before.knobs, &after.knobs);
    let mut simulated = Vec::new();
    if changed(b.max_concurrent, a.max_concurrent).is_some() {
        simulated.push("maxConcurrent".to_string());
    }
    if changed(b.max_admissions_per_tick, a.max_admissions_per_tick).is_some() {
        simulated.push("maxAdmissionsPerTick".to_string());
    }
    if changed(b.interval_secs, a.interval_secs).is_some() {
        simulated.push("intervalSecs".to_string());
    }
    let modelled = ["maxConcurrent", "maxAdmissionsPerTick", "intervalSecs"];
    let mut unsimulated = Vec::new();
    let empty = serde_json::Map::new();
    let obj = |v: &Option<Value>| v.as_ref().and_then(Value::as_object).cloned();
    let (bw, aw) = (obj(&before.view.work_finder), obj(&after.view.work_finder));
    let (bw, aw) = (bw.as_ref().unwrap_or(&empty), aw.as_ref().unwrap_or(&empty));
    let mut keys: Vec<&String> = bw.keys().chain(aw.keys()).collect();
    keys.sort();
    keys.dedup();
    for key in keys {
        if !modelled.contains(&key.as_str()) && bw.get(key) != aw.get(key) {
            unsimulated.push(format!("workFinder.{key}"));
        }
    }
    if before.view.merge_sequencing != after.view.merge_sequencing {
        unsimulated.push("mergeSequencing".to_string());
    }
    (simulated, unsimulated)
}

/// The tick's plan as it would have been under `after`'s knobs: only a knob
/// `after` changes relative to `before` moves anything (see the module doc).
#[must_use]
pub fn replan(
    context: &DispatchPlanContext,
    rows: &[RowPlan],
    before: &PlannerKnobs,
    after: &PlannerKnobs,
) -> (DispatchPlanContext, Vec<RowPlan>) {
    let mut context = context.clone();
    let mut rows = rows.to_vec();
    let slots = &mut context.slots;
    if let Some(cap) = changed(before.max_concurrent, after.max_concurrent) {
        let occupancy = slots.occupancy.or_else(|| {
            slots
                .free
                .map(|free| slots.max_concurrent.saturating_sub(free))
        });
        slots.max_concurrent = cap;
        slots.free = occupancy.map(|o| cap.saturating_sub(o));
    }
    if let Some(admissions) = changed(before.max_admissions_per_tick, after.max_admissions_per_tick)
    {
        slots.max_admissions_per_tick = Some(admissions);
        promote_next(&mut rows, admissions);
    }
    if let Some(secs) = changed(before.interval_secs, after.interval_secs) {
        context.tick_interval_secs = Some(secs);
    }
    (context, rows)
}

/// Re-mark capacity/ramp-deferred rows: the first `ramp` in plan order are
/// `next`, the rest `queued` (the work finder's own `promote_next` rule).
fn promote_next(rows: &mut [RowPlan], ramp: usize) {
    let mut eligible: Vec<(u32, usize)> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| matches!(r.gate, Some(PlanGate::Capacity | PlanGate::Ramp)))
        .filter(|(_, r)| matches!(r.plan_state, PlanState::Next | PlanState::Queued))
        .filter_map(|(i, r)| r.position.map(|p| (p, i)))
        .collect();
    eligible.sort_unstable();
    for (n, (_, i)) in eligible.into_iter().enumerate() {
        rows[i].plan_state = if n < ramp {
            PlanState::Next
        } else {
            PlanState::Queued
        };
    }
}

/// One ready row of the live roster.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RosterRow {
    /// `owner/repo`.
    pub repo: String,
    /// Issue.
    pub issue: u32,
    /// Its plan fields.
    pub plan: RowPlan,
}

/// The live roster: one tick's plan block and rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Roster {
    /// The tick's plan block.
    pub context: DispatchPlanContext,
    /// When the tick completed.
    pub at: DateTime<Utc>,
    /// Its ready rows.
    pub rows: Vec<RosterRow>,
}

/// One estimate's summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EtaCell {
    /// p50 seconds from `as_of`.
    pub p50_sec: Option<i64>,
    /// p25 seconds.
    pub p25_sec: Option<i64>,
    /// p75 seconds.
    pub p75_sec: Option<i64>,
    /// Why there is none.
    pub no_estimate_reason: Option<NoEstimateReason>,
}

impl EtaCell {
    fn of(e: &Explanation) -> Self {
        EtaCell {
            p50_sec: e.result.as_ref().map(|r| r.p50_sec),
            p25_sec: e.result.as_ref().map(|r| r.p25_sec),
            p75_sec: e.result.as_ref().map(|r| r.p75_sec),
            no_estimate_reason: e.no_estimate_reason,
        }
    }
}

/// One side of one row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SideRow {
    /// Plan state (`next`/`queued`/…).
    pub plan_state: PlanState,
    /// Plan position.
    pub position: Option<u32>,
    /// Slot turnovers still needed ([`DispatchInput::turnovers`]).
    pub turnovers: Option<u32>,
    /// `start` estimate.
    pub start: EtaCell,
    /// `land` estimate.
    pub land: EtaCell,
}

/// One roster row, before and after.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PreviewRow {
    /// `owner/repo`.
    pub repo: String,
    /// Issue.
    pub issue: u32,
    /// Under the current config.
    pub before: SideRow,
    /// Under the proposed config.
    pub after: SideRow,
}

/// A whole preview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Preview {
    /// The instant both sides were estimated at.
    pub as_of: DateTime<Utc>,
    /// The tick the roster came from.
    pub plan_at: DateTime<Utc>,
    /// The current regime's stamp.
    pub before_version: String,
    /// The proposed regime's stamp.
    pub after_version: String,
    /// Changed knobs the preview models.
    pub simulated: Vec<String>,
    /// Changed planner keys it does not model.
    pub unsimulated: Vec<String>,
    /// `start` heuristic id.
    pub start_heuristic: String,
    /// `land` heuristic id.
    pub land_heuristic: String,
    /// One per roster row, in plan order (unpositioned rows last).
    pub rows: Vec<PreviewRow>,
}

/// The current heuristics a preview estimates with.
pub struct Estimators<'a> {
    /// The `start` heuristic.
    pub start: &'a dyn Heuristic,
    /// The `land` heuristic.
    pub land: &'a dyn Heuristic,
}

fn ready_input(
    row: &RosterRow,
    dispatch: Option<DispatchInput>,
    as_of: DateTime<Utc>,
) -> EstimateInput {
    EstimateInput {
        subject: Subject::new(&row.repo, None, row.issue),
        as_of,
        current: CurrentState::At(CurrentStage {
            stage: Stage::ReadyWait,
            entered_at: None,
            age_sec: 0,
            age_source: AgeSource::TrackerObserved,
            rework_rounds: 0,
            episode_entered_at: None,
        }),
        features: super::explanation::Features::default(),
        features_omitted: Vec::new(),
        provenance: Provenance::current(),
        dispatch,
        stalls: Vec::new(),
        held: None,
        queue: Vec::new(),
        dependencies: None,
    }
}

fn side(
    roster: &Roster,
    context: &DispatchPlanContext,
    plans: &[RowPlan],
    estimators: &Estimators<'_>,
    history: &StageSamples,
    as_of: DateTime<Utc>,
) -> Vec<SideRow> {
    let waiting: Vec<u32> = plans.iter().filter_map(waiting_position).collect();
    roster
        .rows
        .iter()
        .zip(plans)
        .map(|(row, plan)| {
            let dispatch = waiting_position(plan)
                .and_then(|_| dispatch_input(plan, &waiting, context, roster.at));
            let turnovers = dispatch.as_ref().map(DispatchInput::turnovers);
            let input = ready_input(row, dispatch, as_of);
            SideRow {
                plan_state: plan.plan_state,
                position: plan.position,
                turnovers,
                start: EtaCell::of(&estimators.start.estimate(&input, history)),
                land: EtaCell::of(&estimators.land.estimate(&input, history)),
            }
        })
        .collect()
}

/// The before/after preview of `roster` under `before` and `after`. Pure and
/// deterministic: same inputs, same preview.
#[must_use]
pub fn preview(
    roster: &Roster,
    before: &Regime,
    after: &Regime,
    estimators: &Estimators<'_>,
    history: &StageSamples,
    as_of: DateTime<Utc>,
) -> Preview {
    let plans: Vec<RowPlan> = roster.rows.iter().map(|r| r.plan.clone()).collect();
    let (after_context, after_plans) = replan(&roster.context, &plans, &before.knobs, &after.knobs);
    let before_rows = side(roster, &roster.context, &plans, estimators, history, as_of);
    let after_rows = side(roster, &after_context, &after_plans, estimators, history, as_of);
    let mut rows: Vec<PreviewRow> = roster
        .rows
        .iter()
        .zip(before_rows.into_iter().zip(after_rows))
        .map(|(row, (before, after))| PreviewRow {
            repo: row.repo.clone(),
            issue: row.issue,
            before,
            after,
        })
        .collect();
    rows.sort_by_key(|r| (r.before.position.is_none(), r.before.position, r.issue));
    let (simulated, unsimulated) = changed_keys(before, after);
    Preview {
        as_of,
        plan_at: roster.at,
        before_version: before.planner_version.clone(),
        after_version: after.planner_version.clone(),
        simulated,
        unsimulated,
        start_heuristic: estimators.start.id().to_string(),
        land_heuristic: estimators.land.id().to_string(),
        rows,
    }
}
