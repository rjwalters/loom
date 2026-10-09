//! Ready-queue ingestion (#9326): the last work-finder tick's dispatch plan
//! (#9288) as tracker items in `ready_wait`, and the slot-turnover samples
//! `start-v1` reads.
//!
//! Only ready rows with no running sweep and no PR are the plan's to
//! describe; an item the sweep or a review listing already tracks is left
//! alone. A row this host's planner gives no position but the fleet can
//! still dispatch is placed by [`crate::eta::ready_order`] (#10903) and
//! estimated with its reason in `not_here`. A row enters `ready_wait` at the tracker's first sight of it, which
//! is a lower bound, so leaving the stage never yields a duration — the
//! `ready_wait` history is the slot-turnover journal rows alone, never whole
//! queue waits.

use super::{Effects, ItemKey, StageTrack, Tracker};
use crate::eta::ready_order::{placements, Placed, Placement};
use crate::eta::{AgeSource, DispatchInput, NoEstimateReason, Stage};
use crate::observability::ops::turnaround::Turnover;
use crate::types::{DispatchPlanContext, PlanState, QueueDisposition, RowPlan};
use chrono::{DateTime, Utc};

/// A plan older than this (or three tick intervals, whichever is longer)
/// describes a work finder that stopped ticking: its ready rows are refused
/// `stale_inputs` until a fresh tick arrives.
pub const READY_PLAN_MAX_AGE_SECS: i64 = 15 * 60;

/// The repo slot-turnover samples are journaled under: a turnover is a
/// host-wide event (every repo's sweeps share the slots), so its samples are
/// only ever selected at host level.
pub const SLOT_TURNOVER_REPO: &str = "(host)";

/// The journal event for the tracker's first sight of a ready row. Its
/// `raw.dispatch` is the [`DispatchInput`] it was estimated from (null when
/// refused), which is what makes `start-v1` backtestable
/// ([`crate::eta::backtest::cases_from_journal`]).
pub const READY_FIRST_SEEN: &str = "ready.first_seen";

/// One ready row of the plan, keyed by its repo slug.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadyRow {
    /// `owner/repo`.
    pub repo: String,
    /// Issue.
    pub issue: u32,
    /// Its 1-based rank in the work finder's comparator order over every
    /// row (`ReadyQueueRow::rank`): what places a row the planner gave no
    /// position (#10903).
    pub rank: usize,
    /// Its plan fields.
    pub plan: RowPlan,
    /// What the work finder did with it (`open_pr` is `pr-open-skip`).
    pub disposition: QueueDisposition,
    /// The row's detail (`ReadyQueueRow::detail`): the park label, or the
    /// halt cause of a `workspace_halted` row.
    pub detail: Option<String>,
    /// The issue's own facts from the queue row (#10231).
    pub facts: super::IssueRow,
}

/// The tick a set of [`ReadyRow`]s came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadyPlan {
    /// The per-tick plan block.
    pub context: DispatchPlanContext,
    /// When the tick completed.
    pub at: DateTime<Utc>,
    /// `owner/repo` of each repo whose ready listing failed on the tick.
    pub listing_failed: Vec<String>,
}

/// How old a plan may get before it is stale: [`READY_PLAN_MAX_AGE_SECS`],
/// or three tick intervals when that is longer.
pub(super) fn plan_max_age_secs(context: &DispatchPlanContext) -> i64 {
    let tick = i64::try_from(context.tick_interval_secs.unwrap_or(0)).unwrap_or(i64::MAX);
    READY_PLAN_MAX_AGE_SECS.max(tick.saturating_mul(3))
}

/// A waiting (`next`/`queued`) row's plan position; `None` for every other row.
#[must_use]
pub fn waiting_position(plan: &RowPlan) -> Option<u32> {
    match plan.plan_state {
        PlanState::Next | PlanState::Queued => plan.position,
        _ => None,
    }
}

/// The [`DispatchInput`] a positioned row of a plan is estimated from:
/// `waiting` is every waiting row's position ([`waiting_position`], any order),
/// `context` and `plan_at` the tick's plan block and completion time. `None`
/// for a row with no position. Pure: the one place the tracker (#9326) and
/// the planner preview ([`crate::eta::planner_sim`], #10528) derive it.
#[must_use]
pub fn dispatch_input(
    plan: &RowPlan,
    waiting: &[u32],
    context: &DispatchPlanContext,
    plan_at: DateTime<Utc>,
) -> Option<DispatchInput> {
    let position = plan.position?;
    let gate = plan
        .gate
        .and_then(|g| serde_json::to_value(g).ok())
        .and_then(|v| v.as_str().map(str::to_string));
    let ahead = to_u32(waiting.iter().filter(|&&p| p < position).count());
    Some(slotted(position, plan.plan_state, gate, ahead, context, plan_at))
}

/// The [`DispatchInput`] of a row [`crate::eta::ready_order`] placed
/// (#10903): its own position and `ahead`, no gate, the reason and any hold
/// expiry, and the tick's slots like every other row.
fn placed_input(
    placed: &Placed,
    context: &DispatchPlanContext,
    plan_at: DateTime<Utc>,
) -> DispatchInput {
    let mut input =
        slotted(placed.position, PlanState::Queued, None, placed.ahead, context, plan_at);
    input.not_here = Some(placed.not_here.clone());
    input.held_until = placed.held_until;
    input
}

/// A [`DispatchInput`] from a row's place and the tick's slots.
fn slotted(
    position: u32,
    plan_state: PlanState,
    gate: Option<String>,
    ahead: u32,
    context: &DispatchPlanContext,
    plan_at: DateTime<Utc>,
) -> DispatchInput {
    let slots = &context.slots;
    DispatchInput {
        position,
        plan_state: plan_state_name(plan_state).to_string(),
        gate,
        ahead,
        free_slots: if slots.saturation_held {
            0
        } else {
            to_u32(slots.free.unwrap_or(0))
        },
        max_admissions_per_tick: slots.max_admissions_per_tick.map(to_u32),
        tick_interval_secs: context.tick_interval_secs.unwrap_or(0),
        saturation_held: slots.saturation_held,
        plan_at,
        not_here: None,
        held_until: None,
    }
}

fn to_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

impl Tracker {
    /// The ready rows of the last work-finder tick, observed at `now`.
    ///
    /// Every waiting row with a position gets a [`DispatchInput`], and so
    /// does every row [`crate::eta::ready_order`] places (#10903: this host
    /// cannot dispatch it, the fleet can); every other non-running row is
    /// refused with the reason [`placements`] gives. A ready item that
    /// left a *complete* plan without being dispatched (relabelled, closed)
    /// queues one `issues/{n}` read, offered by the next
    /// [`Tracker::on_listing`] pass like every other outstanding check. An
    /// incomplete plan (a repo's listing failed) says nothing about the rows
    /// it lacks, so nothing departs.
    pub fn on_ready_queue(
        &mut self,
        rows: &[ReadyRow],
        plan: &ReadyPlan,
        now: DateTime<Utc>,
    ) -> Effects {
        let mut effects = Effects::default();
        let stale = (now - plan.at).num_seconds() > plan_max_age_secs(&plan.context);
        self.context.plan = Some(super::features::PlanView::of(rows, plan));
        let mut positions: Vec<u32> = rows
            .iter()
            .filter_map(|r| waiting_position(&r.plan))
            .collect();
        positions.sort_unstable();
        let placed = placements(rows);
        let mut seen = Vec::new();
        for (row, placement) in rows.iter().zip(&placed) {
            let key = ItemKey::new(&row.repo, row.issue);
            // The row's issue facts, observed when the tick completed. Kept
            // on an item that already exists; a new one takes them below.
            if let Some(item) = self.items.get_mut(&key) {
                item.facts.note_issue(&row.facts, plan.at);
            }
            if *placement == Placement::Running {
                // Dispatched: the bus event settles it. Not a departure.
                seen.push(key);
                continue;
            }
            if self
                .items
                .get(&key)
                .is_some_and(|i| i.sweep_running || i.in_review_listing || i.pr_number.is_some())
            {
                continue;
            }
            seen.push(key.clone());
            let (reason, ready) = match placement {
                _ if stale => (Some(NoEstimateReason::StaleInputs), None),
                Placement::Refused(reason) => (Some(*reason), None),
                Placement::Placed(p) => (None, Some(placed_input(p, &plan.context, plan.at))),
                Placement::Running | Placement::Waiting => {
                    match dispatch_input(&row.plan, &positions, &plan.context, plan.at) {
                        Some(input) => (None, Some(input)),
                        None => (Some(NoEstimateReason::NoDispatchPlan), None),
                    }
                }
            };
            let loom = self.loom.clone();
            let dispatch_raw = serde_json::to_value(&ready).unwrap_or(serde_json::Value::Null);
            let item = self.item(&row.repo, row.issue);
            item.facts.note_issue(&row.facts, plan.at);
            let first_sight = !item.in_ready_queue;
            let place = |r: &DispatchInput| (r.position, r.ahead, r.not_here.clone(), r.held_until);
            let changed = first_sight
                || item.refused != reason
                || item.ready.as_ref().map(place) != ready.as_ref().map(place);
            item.in_ready_queue = true;
            item.needs_issue_read = false;
            item.refused = reason;
            item.ready = ready;
            if first_sight {
                item.stage = Some(StageTrack {
                    stage: Stage::ReadyWait,
                    entered_at: now,
                    source: AgeSource::TrackerObserved,
                    exact: false,
                });
                let mut entry =
                    crate::eta::journal::JournalEntry::new(READY_FIRST_SEEN, &row.repo, now, &loom);
                entry.issue = Some(row.issue);
                entry.next_stage = Some(Stage::ReadyWait);
                entry.raw = serde_json::json!({
                    "position": row.plan.position,
                    "plan_state": plan_state_name(row.plan.plan_state),
                    "plan_at": plan.at,
                    "dispatch": dispatch_raw,
                });
                effects.journal.push(entry);
            }
            if changed {
                effects.dirty.push(key);
            }
        }
        if plan.context.complete && !stale {
            for (key, item) in &mut self.items {
                if item.in_ready_queue && !item.sweep_running && !seen.contains(key) {
                    item.in_ready_queue = false;
                    item.ready = None;
                    item.refused = None;
                    if item
                        .stage
                        .as_ref()
                        .is_some_and(|s| s.stage == Stage::ReadyWait)
                    {
                        item.stage = None;
                    }
                    item.needs_issue_read = true;
                }
            }
        }
        effects
    }

    /// A slot-turnover interval (the `ready_wait` history), as the journal
    /// row that records it.
    pub fn on_slot_turnover(&self, turnover: &Turnover) -> Effects {
        let mut row = crate::eta::journal::JournalEntry::new(
            "slot.turnover",
            SLOT_TURNOVER_REPO,
            turnover.to,
            &self.loom,
        );
        row.stage = Some(Stage::ReadyWait);
        row.entered_at = Some(turnover.from);
        row.left_at = Some(turnover.to);
        row.duration_sec = Some(turnover.seconds());
        row.resolution_sec = Some(0);
        row.raw = serde_json::json!({"running_at_start": turnover.running_at_start});
        Effects {
            journal: vec![row],
            ..Effects::default()
        }
    }
}

fn plan_state_name(state: PlanState) -> &'static str {
    match state {
        PlanState::Running => "running",
        PlanState::Next => "next",
        PlanState::Queued => "queued",
        PlanState::Blocked => "blocked",
        PlanState::Unknown => "unknown",
    }
}
