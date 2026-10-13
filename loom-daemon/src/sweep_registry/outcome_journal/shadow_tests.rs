//! The T1 (`sweep.outcome`) producer's J2 shadow, driven through the real
//! [`SweepRegistry::append_outcome_telemetry_journal`] call site (#11348).

use super::*;
use crate::sweep_registry::test_support::*;
use crate::telemetry::journal_shadow::{tests as shadow, ShadowConfig, TelemetryStream};
use serial_test::serial;
use tempfile::tempdir;

fn registry(ws: &Path, telemetry_path: PathBuf) -> SweepRegistry {
    let mut config = SweepRegistryConfig::new(ws.to_path_buf());
    config.journal_path = Some(ws.join("test-sweeps-journal.json"));
    config.outcomes_journal_path = Some(ws.join("test-sweep-outcomes.jsonl"));
    config.outcome_telemetry_path = Some(telemetry_path);
    SweepRegistry::new(config)
}

fn reap(registry: &mut SweepRegistry, issue: u32) {
    let sweep_id = insert_dead_running_with_log(registry, issue, 0, "agent-1", "log\n");
    registry.append_outcome_telemetry_journal(
        issue,
        &sweep_id,
        300,
        telemetry::SweepResult::Success,
        None,
        None,
        crate::tap_usage::RegionAccounting::default(),
    );
}

#[test]
#[serial]
fn sweep_outcome_is_shadowed_verbatim_after_the_legacy_append() {
    let dir = tempdir().unwrap();
    let legacy = dir.path().join("sweep-outcome-telemetry.jsonl");
    let mut registry = registry(dir.path(), legacy.clone());
    let root = ShadowConfig::default_root(dir.path());
    let _f = shadow::force(Some(&root));
    reap(&mut registry, 11348);
    let records = shadow::records(&root, TelemetryStream::Sweep);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].envelope.kind, "sweep.outcome");
    let line: serde_json::Value = serde_json::from_str(
        std::fs::read_to_string(&legacy)
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(records[0].envelope.data, line);
}

#[test]
#[serial]
fn sweep_outcome_shadow_is_default_off() {
    let dir = tempdir().unwrap();
    let mut registry = registry(dir.path(), dir.path().join("t.jsonl"));
    let _f = shadow::force(None);
    reap(&mut registry, 1);
    assert!(dir.path().join("t.jsonl").exists());
    assert!(!dir.path().join(".loom/state/journal").exists());
}

#[test]
#[serial]
fn a_failed_legacy_sweep_outcome_append_is_not_shadowed() {
    let dir = tempdir().unwrap();
    // The legacy path's parent is a regular file: the append fails.
    std::fs::write(dir.path().join("blocker"), b"").unwrap();
    let mut registry = registry(dir.path(), dir.path().join("blocker/t.jsonl"));
    let root = ShadowConfig::default_root(dir.path());
    let _f = shadow::force(Some(&root));
    reap(&mut registry, 2);
    assert!(!root.join("sweep").exists());
}
