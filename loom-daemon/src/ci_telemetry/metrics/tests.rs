//! Tests for the CI telemetry cycle metrics and liveness (Issue #12793).

use super::*;
use crate::observability::ops::capture::capture;
use crate::telemetry::ops::{MetricKind, MetricValue};
use crate::telemetry::{TelemetryEnvelope, TelemetryRecord};
use tempfile::TempDir;

fn of(
    captured: &crate::observability::ops::capture::Captured,
    name: MetricName,
) -> Vec<MetricPoint> {
    captured
        .metrics
        .iter()
        .filter(|p| p.name == name)
        .cloned()
        .collect()
}

fn report() -> CycleReport {
    CycleReport {
        summary: CycleSummary {
            repos_polled: 4,
            runs_emitted: 7,
            jobs_emitted: 11,
            recovered_units: 2,
            ..CycleSummary::default()
        },
        ..CycleReport::default()
    }
}

fn io_error() -> CycleError {
    CycleError::Io(io::Error::other("disk"))
}

#[test]
fn every_cycle_error_variant_has_a_distinct_outcome() {
    let until = Utc::now();
    let results: Vec<Result<CycleReport, CycleError>> = vec![
        Ok(report()),
        Err(CycleError::Busy),
        Err(CycleError::BackingOff { until: None }),
        Err(CycleError::RateLimited {
            until,
            detail: String::new(),
        }),
        Err(CycleError::CredentialRejected {
            detail: String::new(),
            progress: Box::default(),
        }),
        Err(CycleError::Discovery(super::super::api::ApiError::Transport(String::new()))),
        Err(io_error()),
    ];
    let mut labels: Vec<&str> = results.iter().map(outcome_label).collect();
    assert_eq!(
        labels,
        [
            "ok",
            "busy",
            "backing_off",
            "rate_limited",
            "credential_rejected",
            "discovery_failed",
            "io_failed"
        ]
    );
    labels.push(OUTCOME_PANICKED);
    labels.sort_unstable();
    labels.dedup();
    assert_eq!(labels.len(), 8);
}

#[test]
fn an_ok_cycle_emits_one_cycle_point_with_its_items_and_duration() {
    let dir = TempDir::new().unwrap();
    let result = Ok(report());
    let ((), captured) =
        capture(|| record_cycle(dir.path(), Some(&result), Duration::from_secs(3)));

    let cycles = of(&captured, MetricName::CiTelemetryCycles);
    assert_eq!(cycles.len(), 1);
    assert_eq!(cycles[0].labels["outcome"], "ok");
    assert_eq!(cycles[0].value, MetricValue::Int(1));
    assert_eq!(MetricName::CiTelemetryCycles.kind(), MetricKind::DeltaCounter);

    let items: Vec<(String, MetricValue)> = of(&captured, MetricName::CiTelemetryCycleItems)
        .into_iter()
        .map(|p| (p.labels["kind"].clone(), p.value))
        .collect();
    assert_eq!(
        items,
        [
            ("repos_polled".to_string(), MetricValue::Int(4)),
            ("runs_emitted".to_string(), MetricValue::Int(7)),
            ("jobs_emitted".to_string(), MetricValue::Int(11)),
            ("recovered_units".to_string(), MetricValue::Int(2)),
        ]
    );
    let duration = of(&captured, MetricName::CiTelemetryCycleDuration);
    assert_eq!(duration[0].value, MetricValue::Double(3.0));
}

#[test]
fn a_failed_cycle_emits_its_outcome_and_no_items() {
    let dir = TempDir::new().unwrap();
    let result = Err(CycleError::Busy);
    let ((), captured) = capture(|| record_cycle(dir.path(), Some(&result), Duration::ZERO));
    let cycles = of(&captured, MetricName::CiTelemetryCycles);
    assert_eq!(cycles.len(), 1);
    assert_eq!(cycles[0].labels["outcome"], "busy");
    assert!(of(&captured, MetricName::CiTelemetryCycleItems).is_empty());
}

#[test]
fn a_rejected_credential_still_reports_the_progress_it_made() {
    let dir = TempDir::new().unwrap();
    let progress = CycleSummary {
        repos_polled: 3,
        runs_emitted: 5,
        ..CycleSummary::default()
    };
    let result = Err(CycleError::CredentialRejected {
        detail: String::new(),
        progress: Box::new(progress),
    });
    let ((), captured) = capture(|| record_cycle(dir.path(), Some(&result), Duration::ZERO));
    let runs = of(&captured, MetricName::CiTelemetryCycleItems)
        .into_iter()
        .find(|p| p.labels["kind"] == "runs_emitted")
        .unwrap();
    assert_eq!(runs.value, MetricValue::Int(5));
}

#[test]
fn a_panicked_task_emits_a_panicked_cycle() {
    let dir = TempDir::new().unwrap();
    let ((), captured) = capture(|| record_cycle(dir.path(), None, Duration::ZERO));
    let cycles = of(&captured, MetricName::CiTelemetryCycles);
    assert_eq!(cycles.len(), 1);
    assert_eq!(cycles[0].labels["outcome"], "panicked");
}

fn write_journal(root: &Path, emitted: &[DateTime<Utc>]) -> u64 {
    let path = journal_path(root);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut text = String::new();
    for at in emitted {
        let mut env = TelemetryEnvelope::new(
            "host",
            TelemetryRecord::MetricPoints(crate::telemetry::ops::MetricPointsRecord {
                captured_at: *at,
                interval_start: None,
                points: Vec::new(),
            }),
        );
        env.emitted_at = *at;
        text.push_str(&serde_json::to_string(&env).unwrap());
        text.push('\n');
    }
    std::fs::write(&path, &text).unwrap();
    text.len() as u64
}

fn gauge(points: &[MetricPoint], name: MetricName) -> i64 {
    let MetricValue::Int(v) = points.iter().find(|p| p.name == name).unwrap().value else {
        panic!("gauge is an int")
    };
    v
}

#[test]
fn lag_is_the_age_of_the_oldest_unexported_line_and_zero_when_caught_up() {
    let dir = TempDir::new().unwrap();
    let now = Utc::now();
    let old = now - chrono::Duration::seconds(300);
    let new = now - chrono::Duration::seconds(10);
    let len = write_journal(dir.path(), &[old, new]);

    // Cursor at 0: the oldest line is 300 s old.
    let points = commit_points(dir.path(), now);
    assert_eq!(gauge(&points, MetricName::CiTelemetryJournalBytes), len as i64);
    assert_eq!(gauge(&points, MetricName::CiTelemetryExportCursorOffset), 0);
    assert_eq!(gauge(&points, MetricName::CiTelemetryExportLag), 300);

    // Cursor past the first line: the pending line is 10 s old.
    let first = serde_json::to_string(&{
        let lines = std::fs::read_to_string(journal_path(dir.path())).unwrap();
        lines.lines().next().unwrap().len() as u64 + 1
    })
    .unwrap();
    let offset: u64 = first.parse().unwrap();
    let cursor_dir = super::super::state_dir(dir.path());
    std::fs::create_dir_all(&cursor_dir).unwrap();
    std::fs::write(
        cursor_dir.join("export-cursor.json"),
        format!(r#"{{"byte_offset":{offset},"exported":1}}"#),
    )
    .unwrap();
    let points = commit_points(dir.path(), now);
    assert_eq!(gauge(&points, MetricName::CiTelemetryExportCursorOffset), offset as i64);
    assert_eq!(gauge(&points, MetricName::CiTelemetryExportLag), 10);

    // Cursor at the end: caught up.
    std::fs::write(
        cursor_dir.join("export-cursor.json"),
        format!(r#"{{"byte_offset":{len},"exported":2}}"#),
    )
    .unwrap();
    assert_eq!(gauge(&commit_points(dir.path(), now), MetricName::CiTelemetryExportLag), 0);
}

#[test]
fn a_missing_journal_reads_as_zeros() {
    let dir = TempDir::new().unwrap();
    let points = commit_points(dir.path(), Utc::now());
    assert_eq!(gauge(&points, MetricName::CiTelemetryJournalBytes), 0);
    assert_eq!(gauge(&points, MetricName::CiTelemetryExportLag), 0);
}

#[test]
fn the_poller_registers_and_beats_in_the_liveness_registry() {
    let interval = Duration::from_secs(300);
    register_liveness(interval);
    task_liveness::beat(CI_TELEMETRY, interval);
    let entry = task_liveness::snapshot()
        .into_iter()
        .find(|e| e.task == CI_TELEMETRY)
        .unwrap();
    assert!(entry.alive);
    assert!(entry.last_beat.is_some());
    assert_eq!(entry.stale_after_secs, liveness_window(interval).as_secs());
    assert!(liveness_window(interval) > task_liveness::default_stale_after(interval));
}
