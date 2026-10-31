//! `loom.discovery.recheck{reason, outcome}` (#12318), a delta counter drained
//! on the collector's rate-limit tick: the rechecks the daemon asked the
//! state mirror for after a claim refusal showed a stale draw. `reason` is
//! the refusal token, `outcome` is `sent`, `error` or `skipped`.
//!
//! Nothing accumulates without an ops sink (one atomic check per call).

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::telemetry::ops::{MetricName, MetricPoint};

static STORE: Mutex<BTreeMap<(&'static str, &'static str), u64>> = Mutex::new(BTreeMap::new());

/// Count one recheck decision.
pub fn record(reason: &'static str, outcome: &'static str) {
    if !super::spans_exported() {
        return;
    }
    let mut g = STORE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *g.entry((reason, outcome)).or_default() += 1;
}

/// Drain every series into points (delta semantics).
#[must_use]
pub fn drain_points() -> Vec<MetricPoint> {
    let drained = std::mem::take(
        &mut *STORE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    drained
        .into_iter()
        .map(|((reason, outcome), n)| {
            MetricPoint::int(MetricName::DiscoveryRecheck, i64::try_from(n).unwrap_or(i64::MAX))
                .label("reason", reason)
                .label("outcome", outcome)
        })
        .collect()
}
