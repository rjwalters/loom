//! Live proof for the stage-attribution queries of `signoz/eta-queries.sql`
//! (QA and QB, Issue #10957), executed verbatim against the pinned ClickHouse
//! the trial's telemetry store runs. Requires Docker; never converts missing
//! Docker to a pass.
//!
//! - QA joins an `eta.estimate` body's `stage_predictions` to the matching
//!   `eta.outcome` body's `attribution.stages`, one row per stage, with
//!   `sum(contribution_sec) + unattributed_sec` equal to the outcome's error,
//!   a duplicated (at-least-once) delivery counted once, and an absent entry
//!   instant NULL rather than 0.
//! - QB reads the nightly `eta.stage_attribution` rollup: a re-offered row
//!   counts once, an `n = 0` row has NULL (not 0) statistics, and the newest
//!   day sorts first.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::io::Write;
use std::process::{Command, Stdio};

const CLICKHOUSE_IMAGE: &str = "clickhouse/clickhouse-server:25.12.5@sha256:cacf32d6884291dc2ff5e0156a97f46fc53ff7c929a7906d114e268a929dfd3a";
const QUERIES: &str = include_str!("../../defaults/observability/signoz/eta-queries.sql");
const SINCE: &str = "2026-09-01 00:00:00";

type Row = BTreeMap<String, serde_json::Value>;

const FIXTURE: &str = r#"
CREATE DATABASE signoz_logs;
CREATE TABLE signoz_logs.distributed_logs_v2
(
    timestamp          UInt64,
    body               String,
    attributes_string  Map(LowCardinality(String), String),
    attributes_number  Map(LowCardinality(String), Float64),
    attributes_bool    Map(LowCardinality(String), Bool),
    resources_string   Map(LowCardinality(String), String)
) ENGINE = Memory;

-- The estimate: forecast two stages.
INSERT INTO signoz_logs.distributed_logs_v2 VALUES (
    1789430400000000000,
    '{"estimate_id":"e-1","stage_predictions":{"sweep.builder":{"entry_p50":0,"entry_p90":0,"dwell_p50":500,"dwell_p90":900,"alloc":600,"reach_pct":100},"review_wait":{"entry_p50":600,"entry_p90":900,"dwell_p50":250,"dwell_p90":400,"alloc":300,"reach_pct":80}}}',
    map('loom.eta.estimate_id', 'e-1', 'loom.eta.trigger', 'created'), map(), map(), map());
-- The outcome, delivered twice. `doctor` was visited but not forecast.
INSERT INTO signoz_logs.distributed_logs_v2 VALUES (
    1789434000000000000,
    '{"attribution":{"stages":{"sweep.builder":{"predicted_entry_sec":0,"predicted_dwell_sec":600,"actual_entry_sec":0,"actual_dwell_sec":900,"contribution_sec":300},"review_wait":{"predicted_entry_sec":600,"predicted_dwell_sec":300,"actual_entry_sec":900,"actual_dwell_sec":100,"contribution_sec":-200},"doctor":{"predicted_entry_sec":null,"predicted_dwell_sec":0,"actual_entry_sec":1000,"actual_dwell_sec":40,"contribution_sec":40}},"unattributed_sec":50}}',
    map('loom.eta.estimate_id', 'e-1', 'loom.eta.outcome', 'landed'), map(), map(), map());
INSERT INTO signoz_logs.distributed_logs_v2
SELECT * FROM signoz_logs.distributed_logs_v2 WHERE mapContains(attributes_string, 'loom.eta.outcome');

-- The rollup: day 10-05 (one row twice), day 10-04, and an empty stage.
INSERT INTO signoz_logs.distributed_logs_v2 VALUES
 (1791158400000000000, '{}',
  map('loom.eta.stage_attribution.row_id', 'r1', 'loom.eta.stage_attribution.day', '2026-10-05',
      'loom.eta.stage_attribution.heuristic', 'land-v2', 'loom.eta.stage_attribution.stage', 'sweep.builder'),
  map('loom.eta.stage_attribution.n', 3, 'loom.eta.stage_attribution.bias_sec', 120.5,
      'loom.eta.stage_attribution.mean_abs_sec', 130.25, 'loom.eta.stage_attribution.dominant_share', 0.6667,
      'loom.eta.stage_attribution.window_days', 7),
  map('loom.eta.provenance_complete', true), map()),
 (1791158400000000000, '{}',
  map('loom.eta.stage_attribution.row_id', 'r2', 'loom.eta.stage_attribution.day', '2026-10-05',
      'loom.eta.stage_attribution.heuristic', 'land-v2', 'loom.eta.stage_attribution.stage', 'doctor'),
  map('loom.eta.stage_attribution.n', 0, 'loom.eta.stage_attribution.window_days', 7),
  map('loom.eta.provenance_complete', true), map()),
 (1791072000000000000, '{}',
  map('loom.eta.stage_attribution.row_id', 'r3', 'loom.eta.stage_attribution.day', '2026-10-04',
      'loom.eta.stage_attribution.heuristic', 'land-v2', 'loom.eta.stage_attribution.stage', 'sweep.builder'),
  map('loom.eta.stage_attribution.n', 2, 'loom.eta.stage_attribution.bias_sec', -10,
      'loom.eta.stage_attribution.mean_abs_sec', 10, 'loom.eta.stage_attribution.dominant_share', 1,
      'loom.eta.stage_attribution.window_days', 7),
  map('loom.eta.provenance_complete', true), map());
INSERT INTO signoz_logs.distributed_logs_v2
SELECT * FROM signoz_logs.distributed_logs_v2 WHERE attributes_string['loom.eta.stage_attribution.row_id'] = 'r1';
"#;

fn clickhouse(script: &str, estimate_id: &str) -> String {
    let mut child = Command::new("docker")
        .args([
            "run",
            "--rm",
            "-i",
            "--entrypoint",
            "clickhouse",
            CLICKHOUSE_IMAGE,
            "local",
            "--multiquery",
            &format!("--param_since={SINCE}"),
            "--param_repo=",
            &format!("--param_estimate_id={estimate_id}"),
            "--format=JSONEachRow",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("docker is required for this test");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "clickhouse rejected the script:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The committed statement `index` (comments stripped before the split on
/// `;`, as the sibling tests do).
fn statement(index: usize) -> String {
    let code = QUERIES
        .lines()
        .map(|line| line.find("--").map_or(line, |at| &line[..at]))
        .collect::<Vec<_>>()
        .join("\n");
    let all: Vec<String> = code
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .collect();
    assert_eq!(all.len(), 10, "eta-queries.sql is section 0, Q1-Q7, QA and QB");
    all[index].clone()
}

fn run(index: usize, estimate_id: &str) -> Vec<Row> {
    let script = format!("{FIXTURE}\n{};\n", statement(index));
    clickhouse(&script, estimate_id)
        .lines()
        .map(|l| serde_json::from_str(l).expect("JSONEachRow line"))
        .collect()
}

fn int(row: &Row, key: &str) -> Option<i64> {
    let v = row
        .get(key)
        .unwrap_or_else(|| panic!("{key} missing in {row:?}"));
    if v.is_null() {
        return None;
    }
    Some(
        v.as_i64()
            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            .unwrap(),
    )
}

fn real(row: &Row, key: &str) -> Option<f64> {
    let v = row
        .get(key)
        .unwrap_or_else(|| panic!("{key} missing in {row:?}"));
    if v.is_null() {
        return None;
    }
    Some(
        v.as_f64()
            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            .unwrap(),
    )
}

fn stage(rows: &[Row]) -> Vec<&str> {
    rows.iter().map(|r| r["stage"].as_str().unwrap()).collect()
}

#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn qa_lays_each_stage_forecast_beside_what_happened() {
    let rows = run(8, "e-1");
    assert_eq!(
        stage(&rows),
        ["sweep.builder", "review_wait", "doctor"],
        "path order, once each"
    );
    let builder = &rows[0];
    assert_eq!(int(builder, "predicted_dwell_sec"), Some(600));
    assert_eq!(int(builder, "actual_dwell_sec"), Some(900));
    assert_eq!(int(builder, "contribution_sec"), Some(300));
    assert_eq!(int(builder, "predicted_dwell_p90_sec"), Some(900));
    assert_eq!(int(builder, "reach_pct"), Some(100));
    assert_eq!(int(&rows[1], "reach_pct"), Some(80));
    // Visited but not forecast: NULL, never a zero forecast.
    let doctor = &rows[2];
    assert_eq!(int(doctor, "predicted_entry_sec"), None);
    assert_eq!(int(doctor, "predicted_dwell_p90_sec"), None);
    assert_eq!(int(doctor, "reach_pct"), None);
    // The identity: the stages plus the unexplained remainder are the error.
    let explained: i64 = rows
        .iter()
        .map(|r| int(r, "contribution_sec").unwrap())
        .sum();
    assert_eq!(explained + int(&rows[0], "unattributed_sec").unwrap(), 190);
}

#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn qa_answers_nothing_for_an_unknown_estimate() {
    assert!(run(8, "no-such").is_empty());
    assert!(run(8, "").is_empty());
}

#[test]
#[ignore = "requires Docker: CI explicitly invokes this test with --ignored"]
fn qb_reads_the_rollup_once_newest_day_first_with_null_for_no_data() {
    let rows = run(9, "");
    assert_eq!(rows.len(), 3, "the re-offered row r1 counts once: {rows:?}");
    assert_eq!(rows[0]["day"], "2026-10-05");
    assert_eq!(rows[0]["stage"], "sweep.builder", "stage in path order within a day");
    assert_eq!(int(&rows[0], "n"), Some(3));
    assert_eq!(real(&rows[0], "bias_sec"), Some(120.5));
    assert_eq!(real(&rows[0], "mean_abs_sec"), Some(130.25));
    assert_eq!(real(&rows[0], "dominant_share"), Some(0.6667));
    assert_eq!(int(&rows[0], "window_days"), Some(7));
    assert_eq!(rows[1]["stage"], "doctor");
    assert_eq!(int(&rows[1], "n"), Some(0));
    assert_eq!(real(&rows[1], "bias_sec"), None, "no data is NULL, not a zero bias");
    assert_eq!(rows[2]["day"], "2026-10-04");
    assert_eq!(real(&rows[2], "bias_sec"), Some(-10.0));
}
