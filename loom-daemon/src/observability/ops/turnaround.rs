//! Worker turnaround and idle slots, per host (Issue #8929, part 1).
//!
//! # Slot turnaround: from events the daemon already publishes
//!
//! The reaper publishes `sweep.global.completed` on every terminal
//! transition, and the registry publishes `sweep.global.dispatch` for every
//! admitted sweep. [`spawn_task`] subscribes to both, so it needs no registry
//! hook, no forge call and no change to the frozen work-finder loop.
//! [`SlotLedger`] keeps a FIFO of the instants issue sweeps (the ones that
//! occupy work-finder slots) finished. Each issue-sweep dispatch refills the
//! oldest freed slot and yields one turnaround sample: the seconds from that
//! slot freeing to the dispatch. The samples are exported as the delta pair
//! `loom.dispatch.slot_turnaround` / `.samples`, so the mean is their ratio.
//!
//! Turnaround includes time when there was simply no work. The
//! "idle while work waited" half is [`idle_points`], below. A slot freed
//! before the daemon started is never seen, so the first dispatches after a
//! restart (with an empty ledger) produce no sample rather than a guess.
//!
//! # Idle slots: from the tick seam
//!
//! Each tick reports its end occupancy ([`TickReport::occupancy`]).
//! `loom.dispatch.idle_slots` is `max_concurrent − occupancy`. When that tick
//! also left ready work waiting (a capacity-style deferral: the admission
//! ramp, the saturation brake or the repo slice), the interval until the next
//! tick is credited to `loom.dispatch.idle_slot_seconds` as
//! `min(idle, waiting) × interval` (sample-and-hold, like
//! `loom.pool.exhausted_seconds`). Gaps longer than [`MAX_HOLD_SECS`] (a
//! stopped or suspended daemon) are capped, not credited in full.

use std::collections::VecDeque;
use std::sync::Mutex;

use chrono::{DateTime, Utc};

use crate::event_bus::{EventBus, RecvError};
use crate::telemetry::ops::{MetricName, MetricPoint};
use crate::types::{Event, SweepKind};
use crate::work_finder::TickReport;

/// Most freed-slot instants kept. Far above any real concurrency cap; it only
/// bounds memory if dispatches stop entirely.
pub const MAX_FREED: usize = 64;

/// Longest interval one tick's idle state is held for.
pub const MAX_HOLD_SECS: i64 = 15 * 60;

/// Whether `sweep_id` names an issue sweep (`sweep-issue-<N>-…`, including
/// the `-recovered-` form), the kind that occupies work-finder slots.
#[must_use]
pub fn is_issue_sweep(sweep_id: &str) -> bool {
    sweep_id.starts_with("sweep-issue-")
}

/// FIFO of freed issue-sweep slots.
#[derive(Debug, Default)]
pub struct SlotLedger {
    freed: VecDeque<DateTime<Utc>>,
}

impl SlotLedger {
    /// Apply one bus event observed at `now`. Returns the turnaround in whole
    /// seconds when the event refilled a freed slot.
    pub fn observe(&mut self, event: &Event, now: DateTime<Utc>) -> Option<i64> {
        match event {
            Event::SweepGlobalCompleted { sweep_id, .. } if is_issue_sweep(sweep_id) => {
                if self.freed.len() == MAX_FREED {
                    self.freed.pop_front();
                }
                self.freed.push_back(now);
                None
            }
            Event::SweepGlobalDispatch {
                kind: SweepKind::Issue(_),
                ..
            } => self
                .freed
                .pop_front()
                .map(|freed_at| (now - freed_at).num_seconds().max(0)),
            _ => None,
        }
    }

    /// Freed slots not yet refilled.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.freed.len()
    }
}

/// The points for one turnaround sample.
#[must_use]
pub fn turnaround_points(seconds: i64) -> Vec<MetricPoint> {
    vec![
        MetricPoint::int(MetricName::DispatchSlotTurnaround, seconds),
        MetricPoint::int(MetricName::DispatchSlotTurnaroundSamples, 1),
    ]
}

/// Subscribe to sweep completion and dispatch events and export a
/// turnaround sample per refilled slot. Spawned only when the OTLP ops sink
/// is registered, so a host without an OTLP exporter runs no subscriber.
pub fn spawn_task(bus: &EventBus) -> tokio::task::JoinHandle<()> {
    let mut subscription = bus.subscribe(["sweep.global.completed", "sweep.global.dispatch"]);
    tokio::spawn(async move {
        let mut ledger = SlotLedger::default();
        loop {
            match subscription.recv().await {
                Ok(event) => {
                    if let Some(seconds) = ledger.observe(&event, Utc::now()) {
                        super::emit_metrics(turnaround_points(seconds));
                    }
                }
                Err(RecvError::Closed) => break,
                // A lagged receiver lost some events: the ledger may be short
                // a freed slot, which only drops a sample.
                Err(_) => {}
            }
        }
    })
}

/// One tick's idle state, held until the next tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdleHold {
    pub at: DateTime<Utc>,
    /// Idle slots that ready work could have used: `min(idle, waiting)`.
    pub idle_with_work: usize,
}

/// Ready rows the tick deferred for a capacity-style reason while leaving
/// them dispatchable. `deferred_capacity` is included for completeness; it
/// cannot coexist with idle slots.
#[must_use]
pub fn waiting_ready(report: &TickReport) -> usize {
    report.deferred_capacity
        + report.deferred_ramp_cap
        + report.deferred_saturation
        + report.deferred_out_of_slice
}

/// The tick's idle-slot points, and the hold to keep for the next tick.
/// `previous` is the last tick's hold; its idle slot-seconds are credited up
/// to `now` (capped at [`MAX_HOLD_SECS`]). No occupancy reading, no points.
#[must_use]
pub fn idle_points(
    report: &TickReport,
    max_concurrent: usize,
    previous: Option<IdleHold>,
    now: DateTime<Utc>,
) -> (Vec<MetricPoint>, Option<IdleHold>) {
    let mut points = Vec::new();
    if let Some(hold) = previous.filter(|hold| hold.idle_with_work > 0) {
        let held = (now - hold.at).num_seconds().clamp(0, MAX_HOLD_SECS);
        let slots = i64::try_from(hold.idle_with_work).unwrap_or(i64::MAX);
        points.push(MetricPoint::int(
            MetricName::DispatchIdleSlotSeconds,
            slots.saturating_mul(held),
        ));
    }
    let Some(occupancy) = report.occupancy else {
        return (points, None);
    };
    let idle = max_concurrent.saturating_sub(occupancy);
    points.push(MetricPoint::int(
        MetricName::DispatchIdleSlots,
        i64::try_from(idle).unwrap_or(i64::MAX),
    ));
    let hold = IdleHold {
        at: now,
        idle_with_work: idle.min(waiting_ready(report)),
    };
    (points, Some(hold))
}

static LAST_HOLD: Mutex<Option<IdleHold>> = Mutex::new(None);

/// Export the tick's idle-slot signals. A no-op without an ops sink.
pub fn record_tick(report: &TickReport, max_concurrent: usize) {
    let Some(sink) = super::global_ops_sink() else {
        return;
    };
    let now = Utc::now();
    let mut last = LAST_HOLD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let previous = *last;
    let (points, hold) = idle_points(report, max_concurrent, previous, now);
    *last = hold;
    drop(last);
    sink.emit_metrics_since(points, previous.map(|hold| hold.at));
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "turnaround_tests.rs"]
mod tests;
