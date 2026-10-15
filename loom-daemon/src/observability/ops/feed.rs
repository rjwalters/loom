//! Public-feed narration health (#12152), as delta counters drained on the
//! collector's rate-limit tick next to `loom.stage_entry.lookups`:
//!
//! - `loom.feed.completions{kind, outcome}`: one per `completion` envelope
//!   the narration sink built. `kind` is the trigger that found the merge
//!   ([`Trigger`]: `sweep_exit`, `step`, `reconcile`); `outcome` is
//!   `narrated`, or `seeded` for a fresh host's first reconciliation pass,
//!   which records merges without posting them (#4649).
//! - `loom.feed.reconcile_passes{outcome}`: one per merge-reconciliation
//!   tick that had a workspace to visit ([`PassOutcome`]: `ran`, `deferred`
//!   while the rate-limit breaker cools, `failed` when the forge read
//!   failed).
//!
//! Before this, the only proxy for "is merged work reaching the feed" was
//! `sweep.completed`, which a step (#11898) never emits, so the step-dispatch
//! cutover read as a feed outage and a quiet backstop read as nothing at all.
//! Both labels are already allowlisted, so the collector's `keep_keys` is
//! unchanged. Nothing accumulates without an ops sink.

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::telemetry::ops::{MetricName, MetricPoint};

/// What found the merge a completion narrates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Trigger {
    /// An issue sweep's `sweep.issue.{N}.exited` (#4426).
    SweepExit,
    /// A Champion merge step's `step.item.{N}.ended` (#12152).
    Step,
    /// The periodic merge-reconciliation pass (#4583).
    Reconcile,
}

impl Trigger {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SweepExit => "sweep_exit",
            Self::Step => "step",
            Self::Reconcile => "reconcile",
        }
    }
}

/// How one reconciliation tick went.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PassOutcome {
    /// The forge read answered.
    Ran,
    /// The rate-limit breaker was cooling; the pass waits for it (#12152).
    Deferred,
    /// The forge read failed; the watermark holds for the next pass.
    Failed,
}

impl PassOutcome {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ran => "ran",
            Self::Deferred => "deferred",
            Self::Failed => "failed",
        }
    }
}

static COMPLETIONS: Mutex<BTreeMap<(Trigger, bool), u64>> = Mutex::new(BTreeMap::new());
static PASSES: Mutex<BTreeMap<PassOutcome, u64>> = Mutex::new(BTreeMap::new());

/// Count `n` completions found by `trigger` (`seeded`: recorded, not posted).
pub fn record_completions(trigger: Trigger, seeded: bool, n: usize) {
    if n == 0 || !super::spans_exported() {
        return;
    }
    *COMPLETIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry((trigger, seeded))
        .or_default() += u64::try_from(n).unwrap_or(u64::MAX);
}

/// Count one reconciliation tick.
pub fn record_reconcile_pass(outcome: PassOutcome) {
    if !super::spans_exported() {
        return;
    }
    *PASSES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry(outcome)
        .or_default() += 1;
}

/// Drain both counters into points (delta semantics).
#[must_use]
pub fn drain_points() -> Vec<MetricPoint> {
    let completions = std::mem::take(
        &mut *COMPLETIONS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    let passes = std::mem::take(
        &mut *PASSES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    let count = |n: u64| i64::try_from(n).unwrap_or(i64::MAX);
    completions
        .into_iter()
        .map(|((trigger, seeded), n)| {
            MetricPoint::int(MetricName::FeedCompletions, count(n))
                .label("kind", trigger.as_str())
                .label("outcome", if seeded { "seeded" } else { "narrated" })
        })
        .chain(passes.into_iter().map(|(outcome, n)| {
            MetricPoint::int(MetricName::FeedReconcilePasses, count(n))
                .label("outcome", outcome.as_str())
        }))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::ops::MetricValue::Int;

    #[test]
    #[serial_test::serial]
    fn completions_and_passes_drain_once_by_label() {
        let (points, _) = crate::observability::ops::capture::capture(|| {
            let _ = drain_points();
            record_completions(Trigger::Step, false, 1);
            record_completions(Trigger::Step, false, 1);
            record_completions(Trigger::Reconcile, true, 3);
            record_completions(Trigger::SweepExit, false, 0);
            record_reconcile_pass(PassOutcome::Deferred);
            record_reconcile_pass(PassOutcome::Deferred);
            record_reconcile_pass(PassOutcome::Ran);
            let points = drain_points();
            assert!(drain_points().is_empty(), "delta: nothing new");
            points
        });
        let find = |name: MetricName, labels: &[(&str, &str)]| {
            points
                .iter()
                .find(|p| {
                    p.name == name
                        && labels
                            .iter()
                            .all(|(k, v)| p.labels.get(*k).map(String::as_str) == Some(*v))
                })
                .map(|p| p.value)
        };
        let c = MetricName::FeedCompletions;
        assert_eq!(find(c, &[("kind", "step"), ("outcome", "narrated")]), Some(Int(2)));
        assert_eq!(find(c, &[("kind", "reconcile"), ("outcome", "seeded")]), Some(Int(3)));
        assert_eq!(find(c, &[("kind", "sweep_exit")]), None, "zero is not a point");
        let p = MetricName::FeedReconcilePasses;
        assert_eq!(find(p, &[("outcome", "deferred")]), Some(Int(2)));
        assert_eq!(find(p, &[("outcome", "ran")]), Some(Int(1)));
        assert_eq!(find(p, &[("outcome", "failed")]), None);
    }
}
