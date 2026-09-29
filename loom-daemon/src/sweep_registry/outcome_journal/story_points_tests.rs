//! End-to-end tests for the `sweep.outcome` record's `story_points` field
//! (Issue #9432, epic #9429).
//!
//! Mirrors [`super::complexity_tests`]'s shape: what is tested HERE is the
//! record-construction path in
//! [`SweepRegistry::append_outcome_telemetry_journal`], reading the estimate
//! the registry retained at dispatch time — not the pure label guard (already
//! unit-tested by [`crate::story_points`]'s own module tests) and not the
//! dispatch-time resolution (covered by
//! `sweep_registry::dispatch::story_points_dispatch_tests`).

use super::*;
use crate::sweep_registry::test_support::*;
use serial_test::serial;
use tempfile::tempdir;

/// Read the raw JSON of the `sweep.outcome` telemetry line for `issue` — the
/// only way to tell an omitted optional field from a `null`/`0` one.
fn raw_record(registry: &SweepRegistry, issue: u32) -> serde_json::Value {
    let path = registry.config().resolve_outcome_telemetry_path();
    let contents = std::fs::read_to_string(&path).expect("telemetry journal must exist");
    contents
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|v| v["record"]["kind"] == "sweep.outcome" && v["record"]["issue"] == issue)
        .expect("a sweep.outcome line for this issue")["record"]
        .clone()
}

/// Emit one durable `sweep.outcome` record for a dead sweep whose dispatch
/// resolved `retained_points` (the shape `finish_issue_dispatch` leaves in
/// `registry.story_points`), in a registry rooted at `ws`. The caller owns
/// the `TempDir` so the journal stays readable for the raw-JSON assertion.
fn emit_with_points(
    ws: &std::path::Path,
    retained_points: Option<u8>,
) -> (SweepRegistry, telemetry::SweepOutcomeRecord) {
    let mut registry = fixture_registry(ws).0;
    let issue = 9432;
    let sweep_id = insert_dead_running_with_log(&mut registry, issue, 0, "agent-1", "log\n");
    if let Some(points) = retained_points {
        registry.story_points.insert(sweep_id.clone(), points);
    }
    registry.append_outcome_telemetry_journal(
        issue,
        &sweep_id,
        300,
        telemetry::SweepResult::Success,
        None,
        None,
        crate::tap_usage::RegionAccounting::default(),
    );
    let path = registry.config().resolve_outcome_telemetry_path();
    let record = sweep_outcomes::read_all_sweep_outcomes(&path)
        .into_iter()
        .find(|r| r.issue == issue)
        .expect("a sweep.outcome record for this issue");
    (registry, record)
}

/// AC: a sweep of an issue carrying one `points:*` label produces a
/// `sweep.outcome` record with `story_points` — as a NUMBER, so backend
/// cost-per-point joins need no cast.
#[test]
#[serial]
fn a_resolved_estimate_carries_on_the_outcome_record_as_a_number() {
    let dir = tempdir().unwrap();
    let (registry, record) = emit_with_points(dir.path(), Some(8));
    assert_eq!(record.story_points, Some(8));
    let raw = raw_record(&registry, 9432);
    assert_eq!(
        raw.get("story_points").and_then(serde_json::Value::as_u64),
        Some(8),
        "story_points must reach the journal as a JSON number: {raw}"
    );
}

/// AC: a sweep of an issue without a points label (or whose guard verdict was
/// multiple/invalid, which retains nothing) produces a record with NO such
/// attribute — absent, not `0`.
#[test]
#[serial]
fn an_unretained_estimate_omits_the_key_entirely() {
    let dir = tempdir().unwrap();
    let (registry, record) = emit_with_points(dir.path(), None);
    assert_eq!(record.story_points, None);
    let raw = raw_record(&registry, 9432);
    assert!(
        raw.get("story_points").is_none(),
        "an unresolved estimate must omit the key entirely, never emit 0: {raw}"
    );
}
