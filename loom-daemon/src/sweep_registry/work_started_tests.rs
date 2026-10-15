//! Issue #11641: a sweep emits one `stage.work_started` per role it starts,
//! joinable to the stage stay `fleet.state` / `eta.stage_outcome` report.

use super::*;
use crate::observability::ops::capture::capture;
use crate::sweep_registry::test_support::*;
use crate::telemetry::kinds::fleet_state::FleetStage;
use crate::telemetry::kinds::work_started::WorkStartedRecord;
use crate::telemetry::{DispatchOrigin, TelemetryRecord};
use serial_test::serial;
use tempfile::tempdir;

const REPO: &str = "RJWalters/Loom";

fn starts(records: Vec<TelemetryRecord>) -> Vec<WorkStartedRecord> {
    records
        .into_iter()
        .filter_map(|r| match r {
            TelemetryRecord::WorkStarted(w) => Some(w),
            _ => None,
        })
        .collect()
}

fn write_marker(registry: &SweepRegistry, issue: u32, phase: &str, at: &str, pr: u32) {
    let dir = registry.config().checkpoint_dir();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!("issue-{issue}.json")),
        format!(r#"{{"phase":"{phase}","timestamp":"{at}","pr_number":{pr}}}"#),
    )
    .unwrap();
}

#[test]
fn stage_attempt_counts_reentries_of_the_same_stage() {
    assert_eq!(work_started::stage_attempt("curator-done", ["curator-done"]), 1);
    let loop_ = [
        "curator-done",
        "builder-done",
        "judge-rejected",
        "doctor-done",
    ];
    assert_eq!(work_started::stage_attempt("doctor-done", loop_), 2);
    assert_eq!(work_started::stage_attempt("judge-rejected", loop_), 1);
}

#[test]
#[serial]
fn dispatch_starts_the_curator_with_the_sweeps_start_facts() {
    std::env::set_var("LOOM_REPO", REPO);
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());
    let issue = 1_164_101;
    let (sweep_id, captured) = capture(|| {
        registry
            .dispatch_from(
                &SweepKind::Issue(issue),
                None,
                None,
                None,
                None,
                DispatchOrigin::Watchdog,
            )
            .unwrap()
            .sweep_id
    });
    std::env::remove_var("LOOM_REPO");
    let started = starts(captured.records);
    assert_eq!(started.len(), 1, "{started:?}");
    let r = &started[0];
    assert_eq!(r.repo, "rjwalters/loom", "lowercased like fleet.state");
    assert_eq!(r.issue, Some(issue));
    assert_eq!(r.stage, FleetStage::SweepCurator);
    assert_eq!(r.role, "curator");
    assert_eq!(r.sweep_id, sweep_id);
    assert_eq!(r.attempt_index, Some(1));
    assert_eq!(r.stage_attempt, 1);
    assert_eq!(r.dispatch_origin.as_deref(), Some("watchdog"));
    assert_eq!(r.source, "sweep_dispatch");
    assert_eq!(
        r.started_at, registry.entries[&sweep_id].started_at,
        "the instant fleet.state dates sweep.curator with"
    );
    assert!(r.loom.is_valid());
}

#[test]
#[serial]
fn each_marker_starts_the_next_role_at_its_own_timestamp() {
    std::env::set_var("LOOM_REPO", REPO);
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());
    let issue = 1_164_102;
    let sweep_id = insert_dead_running_with_log(&mut registry, issue, 0, "agent-1", "log\n");
    let started_at = Utc::now() - chrono::Duration::seconds(600);
    registry.entries.get_mut(&sweep_id).unwrap().started_at = started_at;

    let steps = [
        ("curator-done", "2030-01-01T00:01:00Z", FleetStage::SweepBuilder, "builder", 1),
        ("builder-done", "2030-01-01T00:02:00Z", FleetStage::ReviewWait, "judge", 1),
        ("judge-rejected", "2030-01-01T00:03:00Z", FleetStage::Doctor, "doctor", 1),
        ("doctor-done", "2030-01-01T00:04:00Z", FleetStage::ReviewWait, "judge", 2),
        ("judge-done", "2030-01-01T00:05:00Z", FleetStage::MergeWait, "merge", 1),
    ];
    let mut seen = Vec::new();
    for (phase, at, ..) in steps {
        write_marker(&registry, issue, phase, at, 77);
        let ((), captured) = capture(|| {
            registry.sample_phase_transition(&sweep_id, &SweepKind::Issue(issue), started_at);
            // An unchanged marker on the next tick starts nothing.
            registry.sample_phase_transition(&sweep_id, &SweepKind::Issue(issue), started_at);
        });
        seen.extend(starts(captured.records));
    }
    // `merge-done` starts no work.
    write_marker(&registry, issue, "merge-done", "2030-01-01T00:06:00Z", 77);
    let ((), captured) = capture(|| {
        registry.sample_phase_transition(&sweep_id, &SweepKind::Issue(issue), started_at);
    });
    std::env::remove_var("LOOM_REPO");
    assert!(starts(captured.records).is_empty());

    assert_eq!(seen.len(), steps.len(), "{seen:?}");
    for (r, (phase, at, stage, role, attempt)) in seen.iter().zip(steps) {
        assert_eq!(r.stage, stage, "{phase}");
        assert_eq!(r.role, role, "{phase}");
        assert_eq!(r.stage_attempt, attempt, "{phase}");
        assert_eq!(
            r.started_at.to_rfc3339(),
            chrono::DateTime::parse_from_rfc3339(at)
                .unwrap()
                .to_rfc3339()
        );
        assert_eq!(r.source, "sweep_checkpoint");
        assert_eq!(r.pr_number, Some(77));
        assert_eq!(r.sweep_id, sweep_id);
        assert_eq!(r.issue, Some(issue));
        // Not dispatched by this daemon: no start facts, so no origin/attempt.
        assert_eq!(r.dispatch_origin, None);
        assert_eq!(r.attempt_index, None);
    }
}

#[test]
#[serial]
fn a_marker_without_a_timestamp_is_dated_when_observed() {
    std::env::set_var("LOOM_REPO", REPO);
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());
    let issue = 1_164_103;
    let sweep_id = insert_dead_running_with_log(&mut registry, issue, 0, "agent-1", "log\n");
    let started_at = Utc::now() - chrono::Duration::seconds(600);
    registry.entries.get_mut(&sweep_id).unwrap().started_at = started_at;
    let checkpoints = registry.config().checkpoint_dir();
    std::fs::create_dir_all(&checkpoints).unwrap();
    std::fs::write(
        checkpoints.join(format!("issue-{issue}.json")),
        r#"{"phase":"curator-done","pr_number":null}"#,
    )
    .unwrap();
    let before = Utc::now();
    let ((), captured) = capture(|| {
        registry.sample_phase_transition(&sweep_id, &SweepKind::Issue(issue), started_at);
    });
    std::env::remove_var("LOOM_REPO");
    let started = starts(captured.records);
    assert_eq!(started.len(), 1);
    assert_eq!(started[0].source, "sweep_observed");
    assert!(started[0].started_at >= before);
    assert_eq!(started[0].pr_number, None);
}

#[test]
#[serial]
fn nothing_is_built_without_an_exporter() {
    std::env::set_var("LOOM_REPO", REPO);
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());
    // No capture and no global sink: the dispatch must still succeed.
    registry
        .dispatch_from(
            &SweepKind::Issue(1_164_104),
            None,
            None,
            None,
            None,
            DispatchOrigin::Operator,
        )
        .unwrap();
    std::env::remove_var("LOOM_REPO");
}
