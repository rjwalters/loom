//! End-to-end tests for the per-phase effort stamp on `phase_durations`
//! (Issue #14242): every phase entry carries the effort the phase's launch
//! ran at — the sweep session's resolved effort, which each in-sweep phase
//! inherits because the Task tool exposes no effort parameter — and the
//! entry omits the key entirely (never `default` or empty) when no effort
//! could be named. Covers both the sampled-window entries and the unsampled
//! fallback single entry.
//!
//! In its own sibling module for the same file-size reason `phase_usage_tests`
//! already is. No transcript is seeded: the stamp is independent of usage.

use super::*;
use crate::sweep_registry::test_support::*;
use serial_test::serial;
use std::time::Duration;
use tempfile::tempdir;

use super::phase_usage_tests::{outcome_for, raw_outcome};

fn reap_with_effort(registry: &mut SweepRegistry, sweep_id: &str, effort: Option<&str>) {
    let entry = registry.entries.get_mut(sweep_id).unwrap();
    entry.effort = effort.map(str::to_string);
    registry.reap_once();
}

/// AC #14242: a sweep dispatched with an explicit effort stamps that effort on
/// every sampled phase entry — the launch's own effort, so `--group-by mix`
/// can read per-phase values without re-deriving anything from config.
#[test]
#[serial]
fn every_sampled_phase_entry_carries_the_launchs_resolved_effort() {
    std::env::remove_var("LOOM_EFFORT");
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());
    let issue = 14242;

    let sweep_id = insert_dead_running_with_log(&mut registry, issue, 0, "agent-1", "log\n");
    let started_at = Utc::now() - Duration::from_secs(600);
    registry.entries.get_mut(&sweep_id).unwrap().started_at = started_at;
    let mark = |secs: i64| started_at + chrono::Duration::seconds(secs);
    registry.phase_history.insert(
        sweep_id.clone(),
        vec![
            PhaseObservation {
                phase: "curator-done".to_string(),
                at: mark(100),
                pr_number: None,
            },
            PhaseObservation {
                phase: "builder-done".to_string(),
                at: mark(200),
                pr_number: None,
            },
            PhaseObservation {
                phase: "judge-done".to_string(),
                at: mark(300),
                pr_number: None,
            },
        ],
    );
    reap_with_effort(&mut registry, &sweep_id, Some("high"));

    let record = outcome_for(&registry, issue);
    assert_eq!(record.phase_durations.len(), 3);
    for entry in &record.phase_durations {
        assert_eq!(entry.effort.as_deref(), Some("high"), "entry {:?}", entry.phase);
    }
    // On the wire: present per entry, beside the attempt index.
    let raw = raw_outcome(&registry, issue);
    for entry in raw["phase_durations"].as_array().unwrap() {
        assert_eq!(entry["effort"], "high", "{entry}");
    }
}

/// AC #14242: unknown effort omits the key per entry — absent, never `default`
/// or `null` — on both the sampled-window entries and the unsampled fallback.
#[test]
#[serial]
fn an_unnameable_effort_leaves_every_phase_entry_unstamped() {
    std::env::remove_var("LOOM_EFFORT");
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());

    // Sampled windows.
    let sampled = 14243;
    let sweep_id = insert_dead_running_with_log(&mut registry, sampled, 0, "agent-2", "log\n");
    let started_at = Utc::now() - Duration::from_secs(600);
    registry.entries.get_mut(&sweep_id).unwrap().started_at = started_at;
    registry.phase_history.insert(
        sweep_id.clone(),
        vec![PhaseObservation {
            phase: "builder-done".to_string(),
            at: started_at + chrono::Duration::seconds(200),
            pr_number: None,
        }],
    );
    // The fixture entry's runtime is `unknown` (not `claude`), no explicit
    // effort, no ambient LOOM_EFFORT: nothing can name an effort.
    reap_with_effort(&mut registry, &sweep_id, None);

    let record = outcome_for(&registry, sampled);
    assert_eq!(record.phase_durations.len(), 1);
    assert_eq!(record.phase_durations[0].effort, None);
    let raw = raw_outcome(&registry, sampled);
    let entry = &raw["phase_durations"][0];
    assert!(
        entry.get("effort").is_none(),
        "an unnameable effort must OMIT the key, never publish default/null: {entry}"
    );

    // Unsampled fallback single entry: same contract.
    let fallback = 14244;
    let sweep_id = insert_dead_running_with_log(&mut registry, fallback, 0, "agent-3", "log\n");
    registry.entries.get_mut(&sweep_id).unwrap().latest_phase = Some("builder-done".to_string());
    reap_with_effort(&mut registry, &sweep_id, None);
    let record = outcome_for(&registry, fallback);
    assert_eq!(record.phase_durations.len(), 1);
    assert_eq!(record.phase_durations[0].effort, None);
    assert!(raw_outcome(&registry, fallback)["phase_durations"][0]
        .get("effort")
        .is_none());
}

/// The unsampled fallback entry carries the launch's effort too — a step
/// sweep's single phase entry is the role's own resolved effort, stamped by
/// this same fallback path.
#[test]
#[serial]
fn the_unsampled_fallback_entry_carries_the_launchs_effort() {
    std::env::remove_var("LOOM_EFFORT");
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());
    let issue = 14245;

    let sweep_id = insert_dead_running_with_log(&mut registry, issue, 0, "agent-4", "log\n");
    registry.entries.get_mut(&sweep_id).unwrap().latest_phase = Some("builder-done".to_string());
    reap_with_effort(&mut registry, &sweep_id, Some("medium"));

    let record = outcome_for(&registry, issue);
    assert_eq!(record.phase_durations.len(), 1);
    assert_eq!(record.phase_durations[0].phase, "builder");
    assert_eq!(record.phase_durations[0].effort.as_deref(), Some("medium"));
}
