//! The capacity series (#10959): the per-host queue capacity the timeline
//! keeps, the one feature builder, and the persisted log's point-in-time
//! rules (leak and parity).

use crate::eta::capacity_features::{
    build, merge_stall, with_stall, CapacityFeatures, QUEUE_TICK_SEC, SINCE_MAIN_GREEN_CAP_SEC,
};
use crate::eta::capacity_log::{
    append, backfill, backfilled_instants, latest_before, load, missing_instants, uncovered,
    CapacityRow, Source as LogSource,
};
use crate::eta::fleet_signoz_refresh::{FileRows, Limits, SignozRead};
use crate::eta::fleet_signoz_timeline::{Family, Timeline};
use crate::eta::fleet_signoz_timeline_rows::{walk, CiRunRow, QueueCapacity, Row, RowBody, Source};
use crate::eta::stall_features::{PoolReading, StallSnapshot};
use chrono::{DateTime, Duration, TimeZone, Utc};
use std::collections::{BTreeMap, BTreeSet};

const REPO: &str = "rjwalters/loom";
const FIXTURE: &str = include_str!("../fixtures/signoz-timeline.jsonl");

fn t(sec: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 1, 10, 0, 0).unwrap() + Duration::seconds(sec)
}

fn snapshot(host: &str, tick: i64, observed: i64, max: Option<u32>, running: u32) -> Row {
    Row {
        record_id: format!("q:{host}:{tick}:{observed}"),
        repo: REPO.to_string(),
        source: Source::Webhook,
        observed_at: Some(t(observed)),
        noop: false,
        body: RowBody::Queue {
            tick_at: t(tick),
            entries: Vec::new(),
            capacity: QueueCapacity {
                host_id: Some(host.to_string()),
                max_concurrent: max,
                running: Some(running),
                ready: Some(0),
                blocked: Some(0),
            },
        },
    }
}

fn run(id: u64, conclusion: &str, completed: i64, observed: i64, duration_ms: i64) -> Row {
    Row {
        record_id: format!("ci:{id}"),
        repo: REPO.to_string(),
        source: Source::Daemon,
        observed_at: Some(t(observed)),
        noop: false,
        body: RowBody::CiRun(CiRunRow {
            run_id: id,
            run_attempt: 1,
            workflow: "CI".to_string(),
            git_ref: Some("main".to_string()),
            head_sha: None,
            status: Some("completed".to_string()),
            conclusion: Some(conclusion.to_string()),
            completed_at: t(completed),
            duration_ms: Some(duration_ms),
        }),
    }
}

fn fixture_rows() -> Vec<Row> {
    let mut reader = FileRows::parse(FIXTURE).unwrap();
    let limits = Limits {
        page_size: 100,
        max_pages: 100,
    };
    let reader: &mut dyn SignozRead = &mut reader;
    walk(REPO, reader, t(-86_400), t(30 * 86_400), limits)
        .unwrap_or_else(|r| panic!("walk stopped: {r:?}"))
        .0
}

#[test]
fn the_timeline_keeps_each_hosts_capacity_from_the_fixture() {
    let timeline = Timeline::build(&fixture_rows(), t(86_400));
    let host = &timeline.hosts["loom-worker-2"];
    assert_eq!(host.capacity.max_concurrent, Some(12));
    assert_eq!(host.capacity.running, Some(0));
    assert_eq!(host.capacity.ready, Some(1), "the later snapshot wins");
    assert_eq!(host.tick_at, Utc.with_ymd_and_hms(2026, 10, 1, 10, 20, 0).unwrap());
}

#[test]
fn slots_sum_live_hosts_and_a_stale_host_is_not_live() {
    let rows = vec![
        snapshot("a", 0, 5, Some(4), 1),
        snapshot("b", 0, 5, Some(8), 3),
        // b ticks again; a goes quiet.
        snapshot("b", 1_100, 1_105, Some(8), 5),
    ];
    let f = build(&Timeline::build(&rows, t(1_110)), REPO);
    assert_eq!((f.hosts_live, f.slots_fleet, f.running_fleet), (2, Some(12), Some(6)));
    // a's tick is more than two intervals old at 1 201 s: no longer live.
    let f = build(&Timeline::build(&rows, t(QUEUE_TICK_SEC * 2 + 1)), REPO);
    assert_eq!(f.hosts_live, 1);
    let f = build(&Timeline::build(&rows, t(1_110 + 600)), REPO);
    assert_eq!((f.hosts_live, f.slots_fleet), (1, Some(8)));
    assert_eq!(f.slot_util_fleet, Some(5.0 / 8.0));
}

#[test]
fn zero_slots_are_zero_and_no_hosts_are_unknown() {
    let f = build(&Timeline::build(&[snapshot("a", 0, 5, Some(0), 0)], t(60)), REPO);
    assert_eq!((f.hosts_live, f.slots_fleet, f.slot_util_fleet), (1, Some(0), None));
    // A host that stated nothing counts as live but adds no slots.
    let f = build(&Timeline::build(&[snapshot("a", 0, 5, None, 0)], t(60)), REPO);
    assert_eq!((f.hosts_live, f.slots_fleet), (1, Some(0)));
    // Hosts seen, none live: zero capacity. Never seen: unknown.
    let rows = [snapshot("a", 0, 5, Some(4), 0)];
    assert_eq!(build(&Timeline::build(&rows, t(10_000)), REPO).slots_fleet, Some(0));
    assert_eq!(build(&Timeline::build(&[], t(10_000)), REPO).slots_fleet, None);
}

#[test]
fn a_delayed_snapshot_changes_nothing_before_it_was_observed() {
    let live = snapshot("a", 0, 5, Some(4), 1);
    // Ticked at 100 but observed at 900: invisible at 600, visible at 900.
    let late = snapshot("b", 100, 900, Some(8), 1);
    let rows = vec![live, late];
    assert_eq!(build(&Timeline::build(&rows, t(600)), REPO).slots_fleet, Some(4));
    assert_eq!(build(&Timeline::build(&rows, t(900)), REPO).slots_fleet, Some(12));
}

#[test]
fn main_goes_red_then_green() {
    let rows = vec![
        run(1, "success", 100, 110, 60_000),
        run(2, "failure", 500, 510, 90_000),
        run(3, "success", 900, 910, 30_000),
    ];
    let at = |s| build(&Timeline::build(&rows, t(s)), REPO);
    let green = at(200);
    assert_eq!((green.main_red, green.since_main_green_sec), (Some(false), Some(0)));
    let red = at(600);
    assert_eq!((red.main_red, red.since_main_green_sec), (Some(true), Some(500)));
    assert_eq!(red.ci_dur_p50_24h_ms, Some(60_000));
    let back = at(1_000);
    assert_eq!((back.main_red, back.since_main_green_sec), (Some(false), Some(0)));
    assert_eq!(back.ci_dur_p50_24h_ms, Some(60_000));
    assert_eq!(at(50).main_red, None, "no run, no verdict");
    // A failure with no success ever seen is capped.
    let only = [run(9, "failure", 10, 20, 1)];
    let f = build(&Timeline::build(&only, t(30)), REPO);
    assert_eq!(f.since_main_green_sec, Some(SINCE_MAIN_GREEN_CAP_SEC));
}

#[test]
fn a_run_observed_later_than_it_ran_waits_for_its_observation() {
    // Completed (failed) at 500 but only ingested at 2 000.
    let rows = vec![
        run(1, "success", 100, 110, 1),
        run(2, "failure", 500, 2_000, 1),
    ];
    assert_eq!(build(&Timeline::build(&rows, t(1_000)), REPO).main_red, Some(false));
    assert_eq!(build(&Timeline::build(&rows, t(2_000)), REPO).main_red, Some(true));
}

#[test]
fn a_backfilled_row_equals_the_builder_at_that_instant() {
    let rows = vec![
        snapshot("a", 3_000, 3_005, Some(4), 2),
        run(1, "failure", 3_100, 3_110, 5),
    ];
    let at = t(3_600);
    let got = backfill(&rows, "RJWalters/Loom", &[at]);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].source, LogSource::Backfill);
    assert_eq!(got[0].repo, REPO);
    assert_eq!(got[0].features, build(&Timeline::build(&rows, at), REPO));
    assert_eq!(got[0].features.main_red, Some(true));
    // Before anything was knowable there is no row, not an all-unknown one.
    assert!(backfill(&rows, REPO, &[t(100)]).is_empty());
}

#[test]
fn the_log_round_trips_and_serves_only_strictly_earlier_rows() {
    let dir = tempfile::tempdir().unwrap();
    let mk = |sec, source, slots| CapacityRow {
        known_at: t(sec),
        repo: REPO.to_string(),
        source,
        features: CapacityFeatures {
            slots_fleet: Some(slots),
            ..CapacityFeatures::default()
        },
    };
    append(
        dir.path(),
        &[
            mk(0, LogSource::Backfill, 1),
            mk(3_600, LogSource::Backfill, 2),
        ],
    )
    .unwrap();
    append(dir.path(), &[mk(3_600, LogSource::Live, 3)]).unwrap();
    let rows = load(dir.path());
    assert_eq!(rows.len(), 3);
    assert!(latest_before(&rows, REPO, t(0)).is_none());
    assert_eq!(
        latest_before(&rows, REPO, t(1))
            .unwrap()
            .features
            .slots_fleet,
        Some(1)
    );
    // A row at as_of itself is not served; a later one never is.
    assert_eq!(
        latest_before(&rows, REPO, t(3_600))
            .unwrap()
            .features
            .slots_fleet,
        Some(1)
    );
    // At one instant the live row beats the backfill row.
    assert_eq!(
        latest_before(&rows, REPO, t(3_601))
            .unwrap()
            .features
            .slots_fleet,
        Some(3)
    );
    assert!(latest_before(&rows, "other/repo", t(9_999)).is_none());
}

#[test]
fn missing_instants_are_the_hourly_grid_minus_what_is_logged() {
    let from = Utc.with_ymd_and_hms(2026, 10, 1, 9, 30, 0).unwrap();
    let to = Utc.with_ymd_and_hms(2026, 10, 1, 12, 15, 0).unwrap();
    let have: BTreeSet<_> = [Utc.with_ymd_and_hms(2026, 10, 1, 11, 0, 0).unwrap()].into();
    let got = missing_instants(from, to, &have);
    let hours: Vec<_> = got.iter().map(|d| d.format("%H").to_string()).collect();
    assert_eq!(hours, ["10", "12"]);
    let rows = [CapacityRow {
        known_at: *have.iter().next().unwrap(),
        repo: REPO.to_string(),
        source: LogSource::Backfill,
        features: CapacityFeatures::default(),
    }];
    assert_eq!(backfilled_instants(&rows, "RJWalters/loom"), have);
}

#[test]
fn coverage_reports_the_families_short_of_the_window() {
    let rows = vec![snapshot("a", 0, 5, Some(4), 0)];
    let timeline = Timeline::build(&rows, t(15 * 86_400));
    assert_eq!(uncovered(&timeline, t(15 * 86_400), 14), vec![Family::Ci]);
}

#[test]
fn the_stall_fields_are_the_authoritys_and_merge_by_field() {
    let stall = StallSnapshot {
        observed_at: t(0),
        budgets: BTreeMap::new(),
        writers: BTreeMap::new(),
        breaker: None,
        pool: PoolReading {
            usable: 0,
            total: 3,
        },
    };
    let live = with_stall(CapacityFeatures::default(), &stall, REPO, t(60));
    assert_eq!((live.pool_usable, live.pool_exhausted), (Some(0), Some(true)));
    assert_eq!(live.breaker_open, None, "no breaker registered is unknown");
    // A stale snapshot is unknown, never zero.
    let stale = with_stall(CapacityFeatures::default(), &stall, REPO, t(3_600));
    assert_eq!(stale.pool_exhausted, None);
    let base = CapacityFeatures {
        slots_fleet: Some(4),
        ..CapacityFeatures::default()
    };
    let merged = merge_stall(base, &live);
    assert_eq!((merged.slots_fleet, merged.pool_exhausted), (Some(4), Some(true)));
}
