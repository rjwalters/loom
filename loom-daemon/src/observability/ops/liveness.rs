//! Long-running task liveness gauges (Issue #10414).
//!
//! Every [`SAMPLE_INTERVAL`] this exports `loom.daemon.task_alive{task}` once per
//! registered loop ([`crate::task_liveness`]): `1` while the loop beat within
//! its staleness window, `0` once it went silent or marked itself dead. Faults
//! a loop survives or dies of (a panicked tick, a cycle that overran its bound,
//! a loop that exited) are counted as `loom.daemon.task_faults{task, reason}`
//! at the moment they happen.
//!
//! The gauges are sampled on their own ticker ([`spawn_task`],
//! [`SAMPLE_INTERVAL`]), not in the collector's pass. A stuck collector pass,
//! which is also where the ETA pass runs, therefore shows as
//! `task_alive{task=eta_pass} = 0` instead of hiding itself. This sampler is
//! not in the registry. If it stops, every `task_alive` series goes silent.
//! Alert on that silence as well as on a `0`.

use crate::task_liveness::TaskLivenessEntry;
use crate::telemetry::ops::{MetricName, MetricPoint};

/// Why a loop recorded a fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// An iteration panicked; the loop caught it.
    Panic,
    /// An iteration ran past the loop's own bound.
    Overrun,
    /// The loop exited for good.
    Exit,
    /// A non-authority host reached the ETA sink (#10498).
    EtaNonAuthorityEmit,
    /// The ETA authority's pass covers fewer repos than the fleet roster
    /// (#10897): `eta.authority.coverage`.
    EtaAuthorityCoverage,
}

impl Fault {
    /// The `reason` label value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Panic => "panic",
            Self::Overrun => "overrun",
            Self::Exit => "exit",
            Self::EtaNonAuthorityEmit => "eta_non_authority_emit",
            Self::EtaAuthorityCoverage => "eta_authority_coverage",
        }
    }
}

/// One `loom.daemon.task_alive` gauge per entry.
#[must_use]
pub fn points(entries: &[TaskLivenessEntry]) -> Vec<MetricPoint> {
    entries
        .iter()
        .map(|e| {
            MetricPoint::int(MetricName::DaemonTaskAlive, i64::from(e.alive))
                .label("task", e.task.clone())
        })
        .collect()
}

/// How often the gauges are sampled.
pub const SAMPLE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// Export the global registry's gauges; a no-op without the ops sink.
pub fn record() {
    super::emit_metrics(points(&crate::task_liveness::snapshot()));
}

/// Sample the gauges every [`SAMPLE_INTERVAL`]. Pure async, no blocking
/// call, so nothing a sampled loop does can stall it.
pub fn spawn_task() -> tokio::task::JoinHandle<()> {
    tokio::spawn(async {
        let mut ticker = tokio::time::interval(SAMPLE_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            record();
        }
    })
}

/// Count one fault of `task` now; a no-op without the ops sink.
pub fn fault(task: &str, fault: Fault) {
    super::emit_metrics(vec![MetricPoint::int(MetricName::DaemonTaskFaults, 1)
        .label("task", task)
        .label("reason", fault.as_str())]);
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::telemetry::ops::{MetricKind, MetricValue};

    fn entry(task: &str, alive: bool) -> TaskLivenessEntry {
        TaskLivenessEntry {
            task: task.to_string(),
            alive,
            last_beat: None,
            silent_secs: 0,
            interval_secs: 300,
            stale_after_secs: 660,
            dead_reason: None,
        }
    }

    #[test]
    fn one_gauge_per_task_valued_one_or_zero() {
        let points = points(&[entry("auto_update", true), entry("eta_pass", false)]);
        assert_eq!(points.len(), 2);
        assert_eq!(points[0].name, MetricName::DaemonTaskAlive);
        assert_eq!(MetricName::DaemonTaskAlive.kind(), MetricKind::Gauge);
        assert_eq!(points[0].value, MetricValue::Int(1));
        assert_eq!(points[0].labels["task"], "auto_update");
        assert_eq!(points[1].value, MetricValue::Int(0));
        assert_eq!(points[1].labels["task"], "eta_pass");
    }

    #[test]
    fn a_fault_is_a_delta_counter_labelled_task_and_reason() {
        let ((), captured) = crate::observability::ops::capture::capture(|| {
            fault("eta_fleet_refresh", Fault::Overrun);
        });
        let point = captured
            .metrics
            .iter()
            .find(|p| p.name == MetricName::DaemonTaskFaults)
            .unwrap();
        assert_eq!(MetricName::DaemonTaskFaults.kind(), MetricKind::DeltaCounter);
        assert_eq!(point.labels["task"], "eta_fleet_refresh");
        assert_eq!(point.labels["reason"], "overrun");
    }
}
