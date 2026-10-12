//! Issue #11280: a dispatched sweep's `sweep.started` carries the facts its
//! later `sweep.outcome` reports, under the same keys.

use super::*;
use crate::sweep_registry::test_support::*;
use crate::telemetry::{TelemetryEnvelope, TelemetryRecord};
use serial_test::serial;
use std::time::Duration;
use tempfile::tempdir;

const REPO: &str = "rjwalters/loom";

/// One prior terminal attempt for `issue` on this host's outcome journal.
fn seed_prior_attempt(registry: &SweepRegistry, issue: u32) {
    let prior: TelemetryRecord = serde_json::from_value(serde_json::json!({
        "kind": "sweep.outcome",
        "repo": REPO,
        "issue": issue,
        "sweep_id": "sweep-prior",
        "total_duration_sec": 60,
        "result": "failure",
    }))
    .unwrap();
    sweep_outcomes::append_outcome_telemetry(
        &registry.config().resolve_outcome_telemetry_path(),
        &TelemetryEnvelope::new("host-test", prior),
    )
    .unwrap();
}

/// The `record` object of `record`'s native envelope.
fn native(record: TelemetryRecord) -> serde_json::Value {
    serde_json::to_value(TelemetryEnvelope::new("host-test", record)).unwrap()["record"].clone()
}

#[tokio::test]
#[serial]
async fn sweep_started_carries_the_facts_its_outcome_reports() {
    std::env::set_var("LOOM_REPO", REPO);
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());
    let bus = Arc::new(EventBus::new());
    registry.set_event_bus(bus.clone());
    let mut sub = bus.subscribe::<[&str; 0], &str>([]);
    let issue = 11280;
    seed_prior_attempt(&registry, issue);

    let sweep_id = registry
        .dispatch(&SweepKind::Issue(issue), None, Some("opus"), Some("high"), None)
        .unwrap()
        .sweep_id;
    let event = loop {
        let recv = tokio::time::timeout(Duration::from_secs(5), sub.recv())
            .await
            .expect("a dispatch must publish sweep.global.dispatch");
        if let ev @ Event::SweepGlobalDispatch { .. } = recv.unwrap() {
            break ev;
        }
    };
    let records = crate::observability::collector::map_event_to_records(
        &event,
        issue,
        REPO,
        crate::telemetry::RepoVisibility::Public,
        &mut HashMap::new(),
    );
    let started = native(records.into_iter().next().expect("one sweep.started"));
    assert_eq!(started["kind"], "sweep.started");
    assert_eq!(started["model_source"], "explicit");
    assert_eq!(started["effort_source"], "explicit");
    assert_eq!(
        registry
            .start_facts_snapshot()
            .get(&sweep_id)
            .map(|f| f.attempt_index),
        Some(Some(2)),
        "the running sweep's facts are kept for fleet.state"
    );

    assert!(wait_for_condition(5_000, || {
        registry.reap_once();
        registry
            .entries
            .get(&sweep_id)
            .is_some_and(|i| i.state.is_terminal())
    }));
    let outcome = sweep_outcomes::read_all_sweep_outcomes(
        &registry.config().resolve_outcome_telemetry_path(),
    )
    .into_iter()
    .find(|r| r.sweep_id == sweep_id)
    .expect("the dispatched sweep's outcome");
    let outcome = native(TelemetryRecord::SweepOutcome(outcome));
    std::env::remove_var("LOOM_REPO");

    for (key, want) in [
        ("model", serde_json::json!("claude-opus-5-5")),
        ("effort", serde_json::json!("high")),
        ("attempt_index", serde_json::json!(2)),
        ("previous_sweep_id", serde_json::json!("sweep-prior")),
        ("trigger", serde_json::json!("retry_after_substantive_failure")),
    ] {
        assert_eq!(started[key], want, "sweep.started {key}");
        assert_eq!(outcome[key], want, "sweep.outcome {key}");
    }
    assert_eq!(outcome["config"]["effort_source"], "explicit");
}

/// Issue #11370: no explicit effort reports the effort that applies, as a
/// `default`; an alias reports its full id; an unnamed model is absent with
/// `model_source=unknown`. The child's own arguments are unchanged.
#[tokio::test]
#[serial]
async fn dispatch_reports_resolved_effort_and_model() {
    std::env::set_var("LOOM_REPO", REPO);
    std::env::remove_var("LOOM_EFFORT");
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());
    let alias = registry
        .dispatch(&SweepKind::Issue(11371), None, Some("sonnet"), None, None)
        .unwrap()
        .sweep_id;
    let unnamed = registry
        .dispatch(&SweepKind::Issue(11372), None, None, None, None)
        .unwrap()
        .sweep_id;
    std::env::remove_var("LOOM_REPO");

    let facts = registry.start_facts_snapshot();
    let a = &facts[&alias];
    assert_eq!(a.model.as_deref(), Some("claude-sonnet-5-5"));
    assert_eq!(a.model_source.as_deref(), Some("explicit"));
    let u = &facts[&unnamed];
    assert_eq!(u.model, None);
    assert_eq!(u.model_source.as_deref(), Some("unknown"));
    for f in [a, u] {
        // Present exactly when the admitted runtime has an effort setting.
        assert_eq!(f.effort.is_some(), f.effort_source.is_some());
        if let Some(source) = f.effort_source.as_deref() {
            assert_eq!(source, "default");
        }
    }
    // The launch arguments stay as dispatched.
    assert_eq!(registry.entries[&alias].model.as_deref(), Some("sonnet"));
    assert_eq!(registry.entries[&alias].effort, None);
}

#[tokio::test]
#[serial]
async fn outcome_reuses_dispatch_lineage_after_journal_rotation() {
    std::env::set_var("LOOM_REPO", REPO);
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());
    let issue = 11280;
    seed_prior_attempt(&registry, issue);
    let journal = registry.config().resolve_outcome_telemetry_path();

    let sweep_id = registry
        .dispatch(&SweepKind::Issue(issue), None, Some("opus"), Some("high"), None)
        .unwrap()
        .sweep_id;
    // Another issue's outcome rotates the journal before this sweep reaps.
    let backup = journal
        .with_file_name(format!("{}.1", journal.file_name().and_then(|n| n.to_str()).unwrap()));
    std::fs::rename(&journal, &backup).unwrap();

    assert!(wait_for_condition(5_000, || {
        registry.reap_once();
        registry
            .entries
            .get(&sweep_id)
            .is_some_and(|i| i.state.is_terminal())
    }));
    let outcome = sweep_outcomes::read_all_sweep_outcomes(&journal)
        .into_iter()
        .find(|r| r.sweep_id == sweep_id)
        .expect("the dispatched sweep's outcome");
    std::env::remove_var("LOOM_REPO");

    assert_eq!(outcome.attempt_index, Some(2));
    assert_eq!(outcome.previous_sweep_id.as_deref(), Some("sweep-prior"));
}

#[tokio::test]
#[serial]
async fn unreadable_journal_leaves_start_lineage_absent() {
    std::env::set_var("LOOM_REPO", REPO);
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());
    // A directory where the journal file belongs: `read_to_string` fails.
    let journal = registry.config().resolve_outcome_telemetry_path();
    std::fs::create_dir_all(&journal).unwrap();

    let sweep_id = registry
        .dispatch(&SweepKind::Issue(11280), None, Some("opus"), Some("high"), None)
        .unwrap()
        .sweep_id;
    std::env::remove_var("LOOM_REPO");

    let facts = registry.start_facts_snapshot();
    let facts = facts.get(&sweep_id).expect("start facts recorded");
    assert_eq!(facts.attempt_index, None);
    assert_eq!(facts.previous_sweep_id, None);
    assert_eq!(facts.trigger, None);
}
