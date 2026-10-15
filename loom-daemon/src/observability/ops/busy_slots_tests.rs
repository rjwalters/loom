//! Busy slots by role (Issue #11847): the slot accounting must close, so a
//! dashboard can add busy-by-role to idle and get the ceiling back.

use std::collections::BTreeMap;

use super::{busy_points, role_share, SWEEP_ROLE};
use crate::role_runner::DEFAULT_ROLES;
use crate::telemetry::ops::{bounded_labels, MetricName, MetricPoint, MetricValue};
use crate::work_finder::TickReport;

fn value(point: &MetricPoint) -> i64 {
    match point.value {
        MetricValue::Int(v) => v,
        MetricValue::Double(_) => panic!("busy slots are integers"),
    }
}

fn sum(points: &[MetricPoint], name: MetricName, keep: impl Fn(&str) -> bool) -> i64 {
    points
        .iter()
        .filter(|p| p.name == name)
        .filter(|p| keep(p.labels.get("role").map_or("", String::as_str)))
        .map(value)
        .sum()
}

#[test]
fn sweep_busy_plus_idle_slots_is_the_sweep_ceiling() {
    let max_concurrent = 5;
    let report = TickReport {
        occupancy: Some(3),
        ..TickReport::default()
    };
    let (idle, _) = crate::observability::ops::turnaround::idle_points(
        &report,
        max_concurrent,
        None,
        chrono::Utc::now(),
    );
    let busy = busy_points(report.occupancy, None);
    assert_eq!(
        busy,
        vec![MetricPoint::int(MetricName::DispatchBusySlots, 3).label("role", SWEEP_ROLE)]
    );
    let total = sum(&busy, MetricName::DispatchBusySlots, |_| true)
        + sum(&idle, MetricName::DispatchIdleSlots, |_| true);
    assert_eq!(total, 5);
}

#[test]
fn role_busy_by_role_plus_role_idle_is_the_role_ceiling() {
    let running = BTreeMap::from([("judge", 2), ("curator", 1)]);
    let points = busy_points(Some(1), Some((&running, 6)));
    let role = |p: &MetricPoint| p.labels.get("role").cloned();
    let judge = points
        .iter()
        .find(|p| role(p).as_deref() == Some("judge"))
        .unwrap();
    assert_eq!(value(judge), 2);
    let busy = sum(&points, MetricName::DispatchBusySlots, |r| r != SWEEP_ROLE);
    let idle = sum(&points, MetricName::DispatchRoleIdleSlots, |_| true);
    let ceiling = sum(&points, MetricName::DispatchRoleMaxConcurrent, |_| true);
    assert_eq!((busy, idle, ceiling), (3, 3, 6));
    // Every built-in role reports, 0 included: measured, not absent.
    for spec in DEFAULT_ROLES {
        assert!(
            points.iter().any(|p| role(p).as_deref() == Some(spec.name)),
            "{} has no busy_slots series",
            spec.name
        );
    }
    for point in &points {
        assert_eq!(bounded_labels(&point.labels), point.labels);
    }
}

#[test]
fn an_unknown_role_is_counted_and_idle_never_goes_negative() {
    let running = BTreeMap::from([("custom-role", 4)]);
    let points = busy_points(None, Some((&running, 2)));
    assert_eq!(sum(&points, MetricName::DispatchBusySlots, |r| r == "custom-role"), 4);
    assert_eq!(sum(&points, MetricName::DispatchRoleIdleSlots, |_| true), 0);
    assert!(
        !points
            .iter()
            .any(|p| p.labels.get("role").is_some_and(|r| r == SWEEP_ROLE)),
        "no occupancy reading: no sweep point"
    );
}

#[test]
fn nothing_measured_emits_nothing() {
    assert!(busy_points(None, None).is_empty());
}

/// One host budget (#11911): the role runner's share is what the registry
/// occupancy leaves, so role busy + role idle + occupancy is the budget.
#[test]
fn the_role_share_is_the_budget_less_the_occupancy() {
    assert_eq!(role_share(8, Some(5)), 3);
    assert_eq!(role_share(8, None), 8, "no occupancy reading: the whole budget");
    assert_eq!(role_share(2, Some(5)), 0, "never negative");
    let running = BTreeMap::from([("champion", 1)]);
    let points = busy_points(Some(5), Some((&running, role_share(8, Some(5)))));
    let occupancy = sum(&points, MetricName::DispatchBusySlots, |r| r == SWEEP_ROLE);
    let roles = sum(&points, MetricName::DispatchBusySlots, |r| r != SWEEP_ROLE);
    let idle = sum(&points, MetricName::DispatchRoleIdleSlots, |_| true);
    assert_eq!(occupancy + roles + idle, 8);
}

/// #11906: a step's slots, broken down by its role under `pool="step"`,
/// with a zero for every step role and labels that survive the policy.
#[test]
fn step_slots_are_counted_by_role_under_the_step_pool() {
    let running = BTreeMap::from([("judge", 2), ("champion", 1)]);
    let points = super::step_points(&running);
    let by_role: BTreeMap<String, i64> = points
        .iter()
        .map(|p| {
            assert_eq!(p.labels.get("pool").map(String::as_str), Some(super::STEP_POOL));
            assert_eq!(bounded_labels(&p.labels), p.labels, "labels survive the policy");
            (p.labels["role"].clone(), value(p))
        })
        .collect();
    assert_eq!(by_role["judge"], 2);
    assert_eq!(by_role["champion"], 1);
    for role in ["curator", "builder", "doctor"] {
        assert_eq!(by_role[role], 0, "{role} reads 0, not absent");
    }
    assert_eq!(by_role.len(), 5);
}
