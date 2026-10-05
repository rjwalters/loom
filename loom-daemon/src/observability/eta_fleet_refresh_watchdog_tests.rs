//! `observability::eta_fleet_refresh` (#10414): what one guarded tick reports.
//! A finished cycle beats liveness. A panicked cycle counts a fault and still
//! beats, because the loop is alive. An overrunning cycle counts an overrun and
//! beats nothing, so `task_alive` drops once its window passes.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::observability::ops::capture::capture;
use crate::telemetry::ops::MetricName;

fn faults(tick: CycleTick<bool>) -> Vec<String> {
    let ((), captured) = capture(|| report_tick(tick));
    captured
        .metrics
        .iter()
        .filter(|p| p.name == MetricName::DaemonTaskFaults)
        .map(|p| {
            assert_eq!(p.labels["task"], ETA_FLEET_REFRESH);
            p.labels["reason"].clone()
        })
        .collect()
}

#[test]
fn a_finished_cycle_counts_no_fault() {
    assert!(faults(CycleTick::Finished(Ok(true))).is_empty());
}

#[test]
fn a_panicked_cycle_counts_a_panic() {
    assert_eq!(faults(CycleTick::Finished(Ok(false))), ["panic"]);
}

#[test]
fn an_overrunning_cycle_counts_an_overrun_and_a_held_one_counts_nothing_more() {
    assert_eq!(
        faults(CycleTick::Overran {
            running_for: Duration::from_secs(3601)
        }),
        ["overrun"]
    );
    assert!(faults(CycleTick::StillRunning {
        running_for: Duration::from_secs(7200)
    })
    .is_empty());
}

#[test]
fn the_cycle_bound_is_one_interval() {
    assert_eq!(cycle_bound(Duration::from_secs(3600)), Duration::from_secs(3600));
}
