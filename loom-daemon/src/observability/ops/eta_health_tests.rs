//! ETA health gauge tests (Issue #10391, slice 2).

use std::collections::BTreeMap;
use std::sync::Mutex as StdMutex;

use chrono::{Duration, TimeZone, Utc};

use super::*;
use crate::eta::health::RefreshCycleState;
use crate::eta::{Kind, NoEstimateReason, Provenance, Stage};
use crate::observability::ops::capture::capture;
use crate::telemetry::ops::{MetricValue, OPS_METRIC_LABEL_KEYS};

/// The health state is process-global: tests that touch it run one at a time.
static SERIAL: StdMutex<()> = StdMutex::new(());

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 5, 12, 0, 0).unwrap()
}

fn est(issue: u32, kind: Kind, heuristic: &str, mins: i64, refused: bool) -> EstimateSummary {
    EstimateSummary {
        estimate_id: format!("{issue}-{heuristic}-{mins}"),
        kind,
        heuristic: heuristic.to_string(),
        loom: Provenance {
            version: "0.19.700".into(),
            revision: "9d8e226ce0123456789abcdef0123456789abcde".into(),
            tree_state: "clean".into(),
            complete: true,
        },
        repo: "rjwalters/loom".into(),
        repo_id: Some(1),
        issue,
        pr_number: None,
        as_of: now() + Duration::minutes(mins),
        stage: Some(Stage::ReviewWait),
        age_sec: None,
        p25_sec: None,
        p50_sec: (!refused).then_some(100),
        p75_sec: None,
        p90_sec: None,
        samples_min: None,
        no_estimate_reason: refused.then_some(NoEstimateReason::NoModel),
        stage_quartiles: Vec::new(),
        tail_extrapolated: false,
        stall_cause: None,
    }
}

fn find<'a>(
    points: &'a [MetricPoint],
    name: MetricName,
    label: Option<(&str, &str)>,
) -> Vec<&'a MetricPoint> {
    points
        .iter()
        .filter(|p| p.name == name)
        .filter(|p| label.is_none_or(|(k, v)| p.labels.get(k).is_some_and(|x| x == v)))
        .collect()
}

fn value(p: &MetricPoint) -> i64 {
    match p.value {
        MetricValue::Int(v) => v,
        MetricValue::Double(_) => panic!("expected int"),
    }
}

fn tick(gate: &str, at: DateTime<Utc>, reasons: &[(&str, u64)]) -> RefreshCycleState {
    RefreshCycleState {
        started_at: at,
        gate: gate.into(),
        captain: None,
        interval_secs: 3600,
        stop_reasons: reasons
            .iter()
            .map(|(k, v)| ((*k).to_string(), *v))
            .collect(),
        repos: Vec::new(),
    }
}

#[test]
fn buckets_count_the_newest_estimate_per_item_and_heuristic() {
    let pending = vec![
        est(1, Kind::Land, "land-v1", 0, false),
        est(1, Kind::Land, "land-v1", 5, true), // newer: replaces the answer
        est(2, Kind::Land, "land-v1", 0, false),
        est(2, Kind::Land, "land-fit", 0, true),
    ];
    let b = buckets(&pending);
    let key = |h: &str, r: &str| ("land".to_string(), h.to_string(), r.to_string());
    assert_eq!(b.get(&key("land-v1", "answered")), Some(&1));
    assert_eq!(b.get(&key("land-v1", "no_model")), Some(&1));
    assert_eq!(b.get(&key("land-fit", "no_model")), Some(&1));
    assert_eq!(b.values().sum::<u64>(), 3);
}

#[test]
fn full_fixture_emits_every_gauge_with_its_labels_and_values() {
    let facts = Facts {
        now: now(),
        items: Some(buckets(&[
            est(1, Kind::Land, "land-v1", 0, false),
            est(2, Kind::Land, "land-v1", 0, true),
        ])),
        fit_cutoff: Some(now() - Duration::hours(2)),
        fit_check: Some((now() - Duration::minutes(10), "today_exists".into())),
        snapshots: vec![("rjwalters/loom".into(), now() - Duration::minutes(30))],
        gate: Some("captain".into()),
        last_tick: Some(now() - Duration::minutes(5)),
        refresh_repos: BTreeMap::from([("no_reader".to_string(), 2)]),
        snapshot_rows: Some((7, 3)),
    };
    let p = points(&facts);
    let items = find(&p, MetricName::EtaHealthItems, Some(("reason", "answered")));
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].labels.get("kind").map(String::as_str), Some("land"));
    assert_eq!(items[0].labels.get("heuristic").map(String::as_str), Some("land-v1"));
    assert_eq!(value(items[0]), 1);
    assert_eq!(value(find(&p, MetricName::EtaHealthItems, Some(("reason", "no_model")))[0]), 1);
    assert_eq!(value(find(&p, MetricName::EtaHealthFitLoaded, None)[0]), 1);
    assert_eq!(value(find(&p, MetricName::EtaHealthFitAgeSeconds, None)[0]), 7200);
    let check = find(&p, MetricName::EtaHealthFitCheckAgeSeconds, Some(("reason", "today_exists")));
    assert_eq!(value(check[0]), 600);
    let snap = find(&p, MetricName::EtaHealthSnapshotAgeSeconds, Some(("repo", "rjwalters/loom")));
    assert_eq!(value(snap[0]), 1800);
    assert_eq!(
        value(find(&p, MetricName::EtaHealthRefreshGate, Some(("state", "captain")))[0]),
        1
    );
    assert_eq!(value(find(&p, MetricName::EtaHealthRefreshLastCycleAgeSeconds, None)[0]), 300);
    let repos = find(&p, MetricName::EtaHealthRefreshRepos, Some(("reason", "no_reader")));
    assert_eq!(value(repos[0]), 2);
    assert_eq!(value(find(&p, MetricName::EtaHealthSnapshotRows, None)[0]), 7);
    assert_eq!(value(find(&p, MetricName::EtaHealthSnapshotAlternatesRows, None)[0]), 3);
    for point in &p {
        assert!(point
            .labels
            .keys()
            .all(|k| OPS_METRIC_LABEL_KEYS.contains(&k.as_str())));
    }
}

#[test]
fn unknown_is_not_zero() {
    let p = points(&Facts {
        now: now(),
        ..Facts::default()
    });
    // Only fit_loaded = 0 is a measured fact here.
    assert_eq!(p.len(), 1);
    assert_eq!(p[0].name, MetricName::EtaHealthFitLoaded);
    assert_eq!(value(&p[0]), 0);
}

#[test]
fn stand_down_host_keeps_its_gate_and_a_growing_age() {
    let _guard = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    reset();
    let dir = tempfile::tempdir().unwrap();
    // A refreshing tick first, then stand-down ticks: the repo counts of the
    // last refreshing tick survive; the gate and the age follow the stand-down.
    note_tick(&tick("captain", now() - Duration::hours(3), &[("complete", 1)]));
    let started = now() - Duration::hours(1);
    note_tick(&tick("stand_down", started, &[]));
    let (_, early) = capture(|| export(dir.path(), "test-host", now()));
    let (_, late) = capture(|| export(dir.path(), "test-host", now() + Duration::hours(2)));
    let at =
        |c: &crate::observability::ops::capture::Captured, n| value(find(&c.metrics, n, None)[0]);
    let gate =
        find(&early.metrics, MetricName::EtaHealthRefreshGate, Some(("state", "stand_down")));
    assert_eq!(value(gate[0]), 1);
    assert_eq!(at(&early, MetricName::EtaHealthRefreshLastCycleAgeSeconds), 3600);
    assert_eq!(at(&late, MetricName::EtaHealthRefreshLastCycleAgeSeconds), 3 * 3600);
    let repos =
        find(&late.metrics, MetricName::EtaHealthRefreshRepos, Some(("reason", "complete")));
    assert_eq!(value(repos[0]), 1);
    reset();
}

#[test]
fn a_host_whose_refresh_never_ticked_emits_the_gate_but_no_age() {
    let _guard = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    reset();
    // Fleet refresh is enabled by default and no captain is declared in an
    // empty workspace: the gate is `no_captain`, and there is no tick to age.
    let dir = tempfile::tempdir().unwrap();
    let (_, c) = capture(|| export(dir.path(), "test-host", now()));
    let gate = find(&c.metrics, MetricName::EtaHealthRefreshGate, None);
    assert_eq!(gate.len(), 1);
    assert_eq!(gate[0].labels.get("state").map(String::as_str), Some("no_captain"));
    assert!(find(&c.metrics, MetricName::EtaHealthRefreshLastCycleAgeSeconds, None).is_empty());
    assert!(find(&c.metrics, MetricName::EtaHealthRefreshRepos, None).is_empty());
}

#[test]
fn no_coefficient_file_means_fit_not_loaded_and_no_fit_age() {
    let _guard = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    reset();
    let dir = tempfile::tempdir().unwrap();
    let (_, c) = capture(|| export(dir.path(), "test-host", now()));
    assert_eq!(value(find(&c.metrics, MetricName::EtaHealthFitLoaded, None)[0]), 0);
    assert!(find(&c.metrics, MetricName::EtaHealthFitAgeSeconds, None).is_empty());
    assert!(find(&c.metrics, MetricName::EtaHealthFitCheckAgeSeconds, None).is_empty());
    assert!(find(&c.metrics, MetricName::EtaHealthSnapshotRows, None).is_empty());
}

#[test]
fn fit_check_and_snapshot_notes_surface_and_cached_snapshots_age() {
    let _guard = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    reset();
    let dir = tempfile::tempdir().unwrap();
    let mut snap = crate::eta::fleet::FleetSnapshot::empty("rjwalters/loom");
    snap.as_of = now() - Duration::minutes(90);
    crate::eta::fleet::write(
        &crate::eta::fleet::snapshot_path(dir.path(), "rjwalters/loom"),
        &snap,
    )
    .unwrap();
    note_fit_check(now() - Duration::minutes(20), "no_snapshots");
    note_snapshot(4, 1);
    let (_, c) = capture(|| {
        export(dir.path(), "test-host", now());
        export(dir.path(), "test-host", now()); // second pass reuses the mtime cache
    });
    let ages = find(
        &c.metrics,
        MetricName::EtaHealthSnapshotAgeSeconds,
        Some(("repo", "rjwalters/loom")),
    );
    assert_eq!(ages.len(), 2);
    assert!(ages.iter().all(|p| value(p) == 5400));
    let check = find(
        &c.metrics,
        MetricName::EtaHealthFitCheckAgeSeconds,
        Some(("reason", "no_snapshots")),
    );
    assert_eq!(value(check[0]), 1200);
    assert_eq!(value(find(&c.metrics, MetricName::EtaHealthSnapshotAlternatesRows, None)[0]), 1);
    reset();
}
