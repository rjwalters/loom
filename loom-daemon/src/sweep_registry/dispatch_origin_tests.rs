//! Issue #11444: every dispatch path records who asked for the sweep
//! (`dispatch_origin`), and only an operator re-dispatch reports
//! `trigger=operator_redispatch`.
//!
//! One test per in-process dispatch path (work finder, epic supervisor,
//! watchdog, reaper) drives that path's real caller and reads the origin off
//! the sweep's start facts. The IPC paths (operator CLI, MCP, ChatOps, legacy
//! clients) are covered in `ipc/tests/dispatch_origin.rs`,
//! `cli/dispatch.rs` and `safehouse_chatops/tests.rs`.

use super::*;
use crate::sweep_registry::test_support::*;
use crate::telemetry::{trigger, DispatchOrigin, TelemetryEnvelope, TelemetryRecord};
use serial_test::serial;
use tempfile::tempdir;

const REPO: &str = "rjwalters/loom";

/// One prior terminal attempt for `issue` on this host's outcome journal.
fn seed_prior_attempt(registry: &SweepRegistry, issue: u32, result: &str) {
    let prior: TelemetryRecord = serde_json::from_value(serde_json::json!({
        "kind": "sweep.outcome",
        "repo": REPO,
        "issue": issue,
        "sweep_id": format!("sweep-prior-{issue}"),
        "total_duration_sec": 60,
        "result": result,
    }))
    .unwrap();
    sweep_outcomes::append_outcome_telemetry(
        &registry.config().resolve_outcome_telemetry_path(),
        &TelemetryEnvelope::new("host-test", prior),
    )
    .unwrap();
}

/// `(dispatch_origin, trigger)` recorded on `sweep_id`'s start facts.
fn recorded(registry: &SweepRegistry, sweep_id: &str) -> (Option<String>, Option<String>) {
    let facts = registry.start_facts_snapshot();
    let facts = facts.get(sweep_id).expect("start facts recorded");
    (facts.dispatch_origin.clone(), facts.trigger.clone())
}

/// The newest start facts recorded for `issue`.
fn recorded_for_issue(registry: &SweepRegistry, issue: u32) -> (Option<String>, Option<String>) {
    let sweep_id = registry
        .entries
        .values()
        .filter(|info| info.kind == SweepKind::Issue(issue))
        .max_by_key(|info| info.started_at)
        .map(|info| info.sweep_id.clone())
        .expect("a sweep was dispatched for the issue");
    recorded(registry, &sweep_id)
}

#[test]
#[serial]
fn every_origin_is_recorded_and_only_an_operator_redispatch_says_so() {
    std::env::set_var("LOOM_REPO", REPO);
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());
    let origins = [
        DispatchOrigin::Operator,
        DispatchOrigin::Watchdog,
        DispatchOrigin::ReaperRetry,
        DispatchOrigin::Unknown,
    ];
    let mut results = Vec::new();
    for (issue, origin) in (1_144_401..).zip(origins) {
        // The previous attempt landed: the journal alone says
        // `redispatch_after_success`, whoever asked.
        seed_prior_attempt(&registry, issue, "success");
        let sweep_id = registry
            .dispatch_from(&SweepKind::Issue(issue), None, None, None, None, origin)
            .unwrap()
            .sweep_id;
        results.push((origin, recorded(&registry, &sweep_id)));
    }
    std::env::remove_var("LOOM_REPO");

    for (origin, (recorded_origin, recorded_trigger)) in results {
        assert_eq!(recorded_origin.as_deref(), Some(origin.as_str()), "{origin:?}");
        let want = if origin == DispatchOrigin::Operator {
            trigger::OPERATOR_REDISPATCH
        } else {
            trigger::REDISPATCH_AFTER_SUCCESS
        };
        assert_eq!(recorded_trigger.as_deref(), Some(want), "{origin:?}");
    }
}

/// An operator's FIRST dispatch of an issue is still `first`: there is
/// nothing to re-dispatch.
#[test]
#[serial]
fn an_operator_first_attempt_stays_first() {
    std::env::set_var("LOOM_REPO", REPO);
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());
    let sweep_id = registry
        .dispatch_from(
            &SweepKind::Issue(1_144_410),
            None,
            None,
            None,
            None,
            DispatchOrigin::Operator,
        )
        .unwrap()
        .sweep_id;
    std::env::remove_var("LOOM_REPO");

    assert_eq!(
        recorded(&registry, &sweep_id),
        (Some("operator".to_string()), Some(trigger::FIRST.to_string()))
    );
}

/// `sweep.outcome` reports the origin and trigger `sweep.started` did.
#[test]
#[serial]
fn the_outcome_reports_the_start_origin_and_trigger() {
    std::env::set_var("LOOM_REPO", REPO);
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());
    let issue = 1_144_420;
    seed_prior_attempt(&registry, issue, "failure");
    let sweep_id = registry
        .dispatch_from(&SweepKind::Issue(issue), None, None, None, None, DispatchOrigin::Operator)
        .unwrap()
        .sweep_id;
    let started = recorded(&registry, &sweep_id);

    assert!(wait_for_condition(FIXTURE_CHILD_WAIT_MS, || {
        registry.reap_once();
        registry
            .entries
            .get(&sweep_id)
            .is_some_and(|info| info.state.is_terminal())
    }));
    let outcome = sweep_outcomes::read_all_sweep_outcomes(
        &registry.config().resolve_outcome_telemetry_path(),
    )
    .into_iter()
    .find(|record| record.sweep_id == sweep_id)
    .expect("the dispatched sweep's outcome");
    std::env::remove_var("LOOM_REPO");

    assert_eq!(
        started,
        (Some("operator".to_string()), Some(trigger::OPERATOR_REDISPATCH.to_string()))
    );
    assert_eq!((outcome.dispatch_origin, outcome.trigger), started);
}

/// A watchdog recovery (here the mid-build-death watchdog) records
/// `watchdog`.
#[test]
#[serial]
fn a_watchdog_recovery_records_its_origin() {
    let dir = tempdir().unwrap();
    let ws = dir.path();
    let (mut registry, _rec) = fixture_registry(ws);
    make_quiet_dirty_git_worktree(&mut registry, ws, 1_144_450);
    insert_terminal_issue(&mut registry, "sweep-issue-1144450-dead", 1_144_450, None);

    assert_eq!(registry.midbuild_watchdog_once(), 1, "the dead sweep is re-dispatched");
    assert_eq!(recorded_for_issue(&registry, 1_144_450).0.as_deref(), Some("watchdog"));
}

/// The reaper's resume-after-crash records `reaper_retry`.
#[test]
#[serial]
fn a_reaper_resume_records_its_origin() {
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());
    let sweep_id = registry
        .dispatch_resume_after_crash(1_144_460, 777)
        .unwrap()
        .sweep_id;
    assert_eq!(recorded(&registry, &sweep_id).0.as_deref(), Some("reaper_retry"));
}
