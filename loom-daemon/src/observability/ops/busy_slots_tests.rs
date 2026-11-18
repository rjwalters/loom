//! Busy slots by role (Issue #11847): the slot accounting must close, so a
//! dashboard can add busy-by-role to idle and get the ceiling back.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;

use uuid::Uuid;

use super::{busy_points, role_ceiling, SWEEP_ROLE};
use crate::observability::ops::{capture, register_global_ops_sink, OpsSink};
use crate::observability::queue::DurableQueue;
use crate::role_runner::{RoleRunnerConfig, DEFAULT_ROLES};
use crate::telemetry::ops::{
    bounded_labels, MetricName, MetricPoint, MetricValue, STEP_OCCUPANCY_PROCESS_ID,
};
use crate::work_finder::TickReport;

const FIXTURE_PROCESS: Uuid = Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0001);

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
    assert_eq!(judge.labels, BTreeMap::from([("role".into(), "judge".into())]));
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

#[test]
fn the_role_ceiling_is_the_largest_enabled_roots() {
    let root = |enabled: Option<bool>, max: Option<usize>| RoleRunnerConfig {
        enabled,
        max_concurrent: max,
        ..RoleRunnerConfig::default()
    };
    if std::env::var_os(crate::role_runner::ROLE_RUNNER_ENABLE_ENV).is_some()
        || std::env::var_os(crate::role_runner::ROLE_RUNNER_MAX_CONCURRENT_ENV).is_some()
    {
        return; // the env tier would override the configs under test
    }
    assert_eq!(role_ceiling(&[]), 0);
    assert_eq!(role_ceiling(&[root(Some(false), Some(9))]), 0, "disabled roots hold no slots");
    assert_eq!(
        role_ceiling(&[
            root(Some(true), Some(3)),
            root(Some(true), Some(5)),
            root(None, Some(9))
        ]),
        5
    );
}

/// #11906: a step's slots, broken down by its role under `pool="step"`,
/// with a zero for every step role and labels that survive the policy.
#[test]
fn step_slots_are_counted_by_role_under_the_step_pool() {
    let t = chrono::Utc::now();
    let running = BTreeMap::from([("judge", vec![t, t]), ("champion", vec![t])]);
    let points = super::step_points(&running, FIXTURE_PROCESS);
    let by_role: BTreeMap<String, i64> = points
        .iter()
        .map(|p| {
            assert_eq!(p.labels.get("pool").map(String::as_str), Some(super::STEP_POOL));
            let mut ordinary = p.labels.clone();
            assert_eq!(
                ordinary.remove(STEP_OCCUPANCY_PROCESS_ID),
                Some(FIXTURE_PROCESS.to_string())
            );
            assert_eq!(bounded_labels(&ordinary), ordinary, "ordinary labels survive the policy");
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

#[test]
fn record_tick_binds_slots_to_the_reporting_process() {
    // Isolate the real global sink, registry map and OnceLock from other tests.
    const CHILD: &str = "LOOM_TEST_STEP_OCCUPANCY_PROCESS_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let dir = tempfile::tempdir().unwrap();
        let report = TickReport {
            occupancy: Some(2),
            ..TickReport::default()
        };
        let tick = |roots: &[PathBuf]| {
            capture::capture(|| super::record_tick(&report, roots))
                .1
                .metrics
        };

        assert!(tick(&[]).is_empty(), "no sink means no emission");
        assert!(super::REPORTING_PROCESS_ID.get().is_none());
        let queue = Arc::new(DurableQueue::open(dir.path().join("ops.jsonl"), 16));
        register_global_ops_sink(OpsSink::new(queue, "same-host"));

        let sweep = MetricPoint::int(MetricName::DispatchBusySlots, 2).label("role", SWEEP_ROLE);
        assert_eq!(tick(&[]), vec![sweep.clone()], "unpublished registry");
        assert!(super::REPORTING_PROCESS_ID.get().is_none());

        let scope = dir.path().join("workspace");
        super::set_steps(&scope, super::LiveSteps::new());
        let empty = tick(std::slice::from_ref(&scope));
        let instance = *super::REPORTING_PROCESS_ID.get().unwrap();
        assert_eq!(instance.get_version_num(), 4);
        assert_eq!(instance.get_variant(), uuid::Variant::RFC4122);
        let identity = instance.to_string();
        let slots: Vec<_> = empty
            .iter()
            .filter(|p| p.labels.contains_key(STEP_OCCUPANCY_PROCESS_ID))
            .collect();
        let expected_roles: BTreeSet<_> = crate::step_role::StepRole::ALL
            .iter()
            .map(|role| role.role_name())
            .collect();
        let observed_roles: BTreeSet<_> = slots
            .iter()
            .map(|point| point.labels["role"].as_str())
            .collect();
        assert_eq!(observed_roles, expected_roles);
        assert_eq!(slots.len(), expected_roles.len());
        assert!(slots
            .iter()
            .all(|p| p.name == MetricName::DispatchBusySlots && value(p) == 0));
        assert!(slots
            .iter()
            .all(|p| p.labels[STEP_OCCUPANCY_PROCESS_ID] == identity));

        let other = dir.path().join("other-workspace");
        super::set_steps(
            &other,
            super::LiveSteps::from([("judge", vec![live("2026-11-18T00:00:00Z", Some(3))])]),
        );
        let occupied = tick(&[scope, other]);
        assert!(occupied.contains(&sweep), "aggregate alert input is unchanged");
        for point in occupied {
            if point.name == MetricName::DispatchBusySlots && point.labels.contains_key("pool") {
                assert_eq!(point.labels[STEP_OCCUPANCY_PROCESS_ID], identity);
                if point.labels["role"] == "judge" {
                    assert_eq!(value(&point), 1);
                }
            } else {
                assert!(!point.labels.contains_key(STEP_OCCUPANCY_PROCESS_ID));
            }
        }
        println!("capture-process={identity}");
        // Keep this process alive until the parent has spawned the other one.
        std::io::stdin().read_to_end(&mut Vec::new()).unwrap();
        return;
    }

    let module = module_path!().split_once("::").unwrap().1;
    let test_name = format!("{module}::record_tick_binds_slots_to_the_reporting_process");
    let start = || {
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &test_name, "--nocapture"])
            .env(CHILD, "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let finish = |mut child: Child| {
        drop(child.stdin.take());
        let output = child.wait_with_output().unwrap();
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(output.status.success(), "{stdout}{}", String::from_utf8_lossy(&output.stderr));
        stdout
            .split_once("capture-process=")
            .and_then(|(_, rest)| rest.lines().next())
            .expect("isolated child did not emit a process token")
            .to_owned()
    };

    let first = start();
    let overlapping = start();
    let first_id = finish(first);
    let overlapping_id = finish(overlapping);
    let restarted_id = finish(start());
    assert_ne!(first_id, overlapping_id);
    assert_ne!(first_id, restarted_id);
    assert_ne!(overlapping_id, restarted_id);
}

fn at(s: &str) -> chrono::DateTime<chrono::Utc> {
    s.parse().unwrap()
}

/// #12873: the step pool's running ages. Steps 1 m, 20 m, 2 h and 5 h old
/// land one per bucket; the oldest age is per role; a role with no live
/// step has no oldest point but reads `0` in every bucket.
#[test]
fn running_ages_are_bucketed_and_the_oldest_is_per_role() {
    let now = at("2026-10-20T12:00:00Z");
    let running = BTreeMap::from([
        (
            "judge",
            vec![
                at("2026-10-20T11:59:00Z"),
                at("2026-10-20T11:40:00Z"),
                at("2026-10-20T10:00:00Z"),
                at("2026-10-20T07:00:00Z"),
            ],
        ),
        ("builder", vec![at("2026-10-20T11:50:00Z")]),
    ]);
    let points = super::age_points(now, &running);
    for p in &points {
        assert_eq!(p.labels.get("pool").map(String::as_str), Some(super::STEP_POOL));
        assert_eq!(bounded_labels(&p.labels), p.labels, "labels survive the policy");
    }
    let oldest: BTreeMap<String, i64> = points
        .iter()
        .filter(|p| p.name == MetricName::DispatchRunningOldestAgeSec)
        .map(|p| (p.labels["role"].clone(), value(p)))
        .collect();
    assert_eq!(
        oldest,
        BTreeMap::from([
            ("judge".to_string(), 5 * 3600),
            ("builder".to_string(), 600)
        ]),
        "no oldest point for a role with no live step"
    );
    let bucket = |role: &str, age: &str| {
        points
            .iter()
            .filter(|p| p.name == MetricName::DispatchRunningByAge)
            .find(|p| p.labels["role"] == role && p.labels["age"] == age)
            .map(value)
    };
    for age in super::AGE_BUCKETS {
        assert_eq!(bucket("judge", age), Some(1), "judge {age}");
        assert_eq!(bucket("curator", age), Some(0), "curator {age} reads 0, not absent");
    }
    assert_eq!(bucket("builder", "lt_15m"), Some(1));
    assert_eq!(bucket("builder", "15m_1h"), Some(0));
    let by_age = points
        .iter()
        .filter(|p| p.name == MetricName::DispatchRunningByAge)
        .count();
    assert_eq!(by_age, 5 * super::AGE_BUCKETS.len(), "every step role x bucket");
}

/// The bucket edges are exclusive upper bounds, and a step whose start is
/// ahead of the sampling clock reads age 0.
#[test]
fn bucket_edges_and_a_future_start() {
    let now = at("2026-10-20T12:00:00Z");
    let running = BTreeMap::from([(
        "doctor",
        vec![
            at("2026-10-20T11:45:00Z"), // exactly 15 m: 15m_1h
            at("2026-10-20T08:00:00Z"), // exactly 4 h: ge_4h
            at("2026-10-20T12:00:30Z"), // the future: 0
        ],
    )]);
    let points = super::age_points(now, &running);
    let bucket = |age: &str| {
        points
            .iter()
            .find(|p| {
                p.name == MetricName::DispatchRunningByAge
                    && p.labels["role"] == "doctor"
                    && p.labels["age"] == age
            })
            .map(value)
    };
    assert_eq!(bucket("lt_15m"), Some(1));
    assert_eq!(bucket("15m_1h"), Some(1));
    assert_eq!(bucket("1h_4h"), Some(0));
    assert_eq!(bucket("ge_4h"), Some(1));
    let oldest = points
        .iter()
        .find(|p| p.name == MetricName::DispatchRunningOldestAgeSec)
        .map(value);
    assert_eq!(oldest, Some(4 * 3600));
    // The existing busy_slots step counts come from the same start times.
    let busy = super::step_points(&running, FIXTURE_PROCESS);
    let doctor = busy
        .iter()
        .find(|p| p.labels["role"] == "doctor")
        .map(value);
    assert_eq!(doctor, Some(3));
}

/// Nothing live: every bucket reads 0 and no role has an oldest point.
#[test]
fn no_live_step_has_zero_buckets_and_no_oldest_age() {
    let points = super::age_points(chrono::Utc::now(), &BTreeMap::new());
    assert!(points
        .iter()
        .all(|p| p.name == MetricName::DispatchRunningByAge && value(p) == 0));
    assert_eq!(points.len(), 5 * super::AGE_BUCKETS.len());
}

fn live(started: &str, points: Option<u32>) -> super::LiveStep {
    super::LiveStep {
        started_at: at(started),
        points,
    }
}

/// #12957: live steps are counted per points class, `unsized` and `0`s
/// included for every step role, and per role the classes add up to that
/// role's `busy_slots{pool="step"}`, both read from the same publish.
#[test]
fn running_steps_are_counted_by_points_class_and_add_up_to_busy_slots() {
    let running = BTreeMap::from([
        (
            "builder",
            vec![
                live("2026-10-20T11:00:00Z", Some(13)),
                live("2026-10-20T11:10:00Z", Some(13)),
                live("2026-10-20T11:20:00Z", Some(2)),
                live("2026-10-20T11:30:00Z", None),
            ],
        ),
        ("judge", vec![live("2026-10-20T11:40:00Z", Some(5))]),
    ]);
    let points = super::size_points(&running);
    let mut by: BTreeMap<(String, String), i64> = BTreeMap::new();
    for p in &points {
        assert_eq!(p.name, MetricName::DispatchRunningByPoints);
        assert_eq!(p.labels.get("pool").map(String::as_str), Some(super::STEP_POOL));
        assert_eq!(bounded_labels(&p.labels), p.labels, "labels survive the policy");
        by.insert((p.labels["role"].clone(), p.labels["points"].clone()), value(p));
    }
    // Every step role × class reports, 0 included.
    assert_eq!(points.len(), 5 * super::POINTS_CLASSES.len());
    let get = |role: &str, class: &str| by[&(role.to_string(), class.to_string())];
    assert_eq!(get("builder", "13"), 2, "a count by class, never a sum of points");
    assert_eq!(get("builder", "2"), 1);
    assert_eq!(get("builder", super::POINTS_UNSIZED), 1);
    assert_eq!(get("builder", "1"), 0);
    assert_eq!(get("judge", "5"), 1);
    assert_eq!(get("curator", super::POINTS_UNSIZED), 0);

    let busy = super::step_points(&super::step_starts(&running), FIXTURE_PROCESS);
    for p in &busy {
        let role = &p.labels["role"];
        let classes: i64 = super::POINTS_CLASSES.iter().map(|c| get(role, c)).sum();
        assert_eq!(classes, value(p), "{role}: Σ_points running_by_points = busy_slots");
    }
}

/// The classes are the story-point vocabulary, then `unsized`: a vocabulary
/// change cannot leave the gauge reading a legal size as unsized.
#[test]
fn points_classes_are_the_vocabulary_plus_unsized() {
    let (sized, rest) = super::POINTS_CLASSES.split_at(super::POINTS_CLASSES.len() - 1);
    assert_eq!(sized, crate::points_marker::POINTS_VALUES);
    assert_eq!(rest, [super::POINTS_UNSIZED]);
}
