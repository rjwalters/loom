//! Ready-queue ingestion (#9326): the last work-finder tick's dispatch plan
//! (#9288) as tracker items in `ready_wait`, and the slot-turnover samples
//! `start-v1` reads.
//!
//! Only ready rows with no running sweep and no PR are the plan's to
//! describe; an item the sweep or a review listing already tracks is left
//! alone. A row enters `ready_wait` at the tracker's first sight of it, which
//! is a lower bound, so leaving the stage never yields a duration — the
//! `ready_wait` history is the slot-turnover journal rows alone, never whole
//! queue waits.

use super::{Effects, ItemKey, StageTrack, Tracker};
use crate::eta::labels::ready_row_reason;
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
    /// Its plan fields.
    pub plan: RowPlan,
    /// What the work finder did with it (`open_pr` is `pr-open-skip`).
    pub disposition: QueueDisposition,
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

fn waiting(plan: &RowPlan) -> Option<u32> {
    match plan.plan_state {
        PlanState::Next | PlanState::Queued => plan.position,
        _ => None,
    }
}

fn to_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

impl Tracker {
    /// The ready rows of the last work-finder tick, observed at `now`.
    ///
    /// Every waiting row with a position gets a [`DispatchInput`]; every
    /// other non-running row is refused `no_dispatch_plan`. A ready item that
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
        let tick = plan.context.tick_interval_secs.unwrap_or(0);
        let stale = (now - plan.at).num_seconds() > plan_max_age_secs(&plan.context);
        self.context.plan = Some(super::features::PlanView::of(rows, plan));
        let slots = &plan.context.slots;
        let mut positions: Vec<u32> = rows.iter().filter_map(|r| waiting(&r.plan)).collect();
        positions.sort_unstable();
        let mut seen = Vec::new();
        for row in rows {
            let key = ItemKey::new(&row.repo, row.issue);
            if row.plan.plan_state == PlanState::Running {
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
            let reason = if stale {
                Some(NoEstimateReason::StaleInputs)
            } else {
                ready_row_reason(&row.plan)
            };
            let ready = match (reason, row.plan.position) {
                (None, Some(position)) => Some(DispatchInput {
                    position,
                    plan_state: plan_state_name(row.plan.plan_state).to_string(),
                    gate: row
                        .plan
                        .gate
                        .and_then(|g| serde_json::to_value(g).ok())
                        .and_then(|v| v.as_str().map(str::to_string)),
                    ahead: to_u32(positions.iter().filter(|&&p| p < position).count()),
                    free_slots: if slots.saturation_held {
                        0
                    } else {
                        to_u32(slots.free.unwrap_or(0))
                    },
                    max_admissions_per_tick: slots.max_admissions_per_tick.map(to_u32),
                    tick_interval_secs: tick,
                    saturation_held: slots.saturation_held,
                    plan_at: plan.at,
                }),
                _ => None,
            };
            let loom = self.loom.clone();
            let dispatch_raw = serde_json::to_value(&ready).unwrap_or(serde_json::Value::Null);
            let item = self.item(&row.repo, row.issue);
            let first_sight = !item.in_ready_queue;
            let changed = first_sight
                || item.refused != reason
                || item.ready.as_ref().map(|r| (r.position, r.ahead))
                    != ready.as_ref().map(|r| (r.position, r.ahead));
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
