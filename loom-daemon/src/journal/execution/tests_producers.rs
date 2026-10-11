//! Per-producer wiring (E1, E2, E8, E9, E10, E16): enabled shadow appends
//! verify against the legacy store, default-off writes no journal and leaves
//! the legacy store unchanged, concern verification is read-only and reports
//! unexplained divergence, and journal evidence never establishes liveness.

use std::path::Path;

use chrono::{Duration, Utc};
use serde_json::json;
use tempfile::tempdir;

use super::tests::{disable, enable, records};
use super::*;
use crate::sweep_registry::test_support::fixture_registry;

fn kinds(root: &Path) -> Vec<String> {
    records(root).into_iter().map(|e| e.kind).collect()
}

fn assert_ok(root: &Path) -> ConcernReport {
    let report = verify_concern(root);
    assert!(report.ok, "{report:#?}");
    assert_eq!(report.recorded_divergences, 0, "{report:#?}");
    report
}

fn store_summary(report: &ConcernReport, store: Store) -> &StoreSummary {
    report
        .stores
        .iter()
        .find(|s| s.store == store.as_str())
        .unwrap_or_else(|| panic!("no {store:?} in {report:#?}"))
}

// ---- E1 sweeps.json -----------------------------------------------------

#[test]
fn e1_upsert_and_remove_are_shadowed_and_verify() {
    let root = tempdir().unwrap();
    let ws = tempdir().unwrap();
    let path = ws.path().join("sweeps.json");
    let pid = std::process::id();
    enable(root.path());
    crate::sweep_journal::record_sweep_at(&path, "/repo/a", 5, pid, Utc::now()).unwrap();
    let report = assert_ok(root.path());
    assert_eq!(store_summary(&report, Store::Sweeps).matched, 1);
    let first = &records(root.path())[0];
    assert_eq!(first.kind, "claim.acquired");
    assert_eq!(first.data["owner_pid"], pid);
    assert_eq!(first.data["subject"]["issue"], 5);

    crate::sweep_journal::remove_sweep_at(&path, "/repo/a", 5).unwrap();
    assert_eq!(kinds(root.path()), ["claim.acquired", "claim.released"]);
    assert_ok(root.path());

    // A legacy write the journal never saw (shadowing off) for a live pid:
    // the journal says released, the legacy store says live.
    disable();
    crate::sweep_journal::record_sweep_at(&path, "/repo/a", 5, pid, Utc::now()).unwrap();
    let before = std::fs::read(&path).unwrap();
    let report = verify_concern(root.path());
    assert!(!report.ok);
    assert_eq!(report.unexplained[0].item, "/repo/a#5");
    // Verification is read-only.
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(kinds(root.path()).len(), 2);
}

#[test]
fn e1_default_off_writes_no_journal_and_the_same_legacy_file() {
    let root = tempdir().unwrap();
    let ws = tempdir().unwrap();
    let (off, on) = (ws.path().join("off.json"), ws.path().join("on.json"));
    let at = Utc::now();
    let pid = std::process::id();
    disable();
    crate::sweep_journal::record_sweep_at(&off, "/repo/a", 1, pid, at).unwrap();
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    enable(root.path());
    crate::sweep_journal::record_sweep_at(&on, "/repo/a", 1, pid, at).unwrap();
    assert_eq!(std::fs::read(&off).unwrap(), std::fs::read(&on).unwrap());
    // A downgraded binary reads the legacy file exactly as before.
    let journal = crate::sweep_journal::load(&on);
    assert_eq!(journal.entries.len(), 1);
    assert_eq!(journal.entries[0].pid, pid);
}

#[test]
fn e1_dead_pids_are_pruned_on_both_sides() {
    let root = tempdir().unwrap();
    let ws = tempdir().unwrap();
    let path = ws.path().join("sweeps.json");
    enable(root.path());
    // A pid far above any real one: dead. The legacy upsert prunes it on the
    // next write; verification applies the same prune and explains it.
    crate::sweep_journal::record_sweep_at(&path, "/repo/a", 9, 3_999_999, Utc::now()).unwrap();
    crate::sweep_journal::record_sweep_at(&path, "/repo/a", 10, std::process::id(), Utc::now())
        .unwrap();
    let report = assert_ok(root.path());
    let summary = store_summary(&report, Store::Sweeps);
    assert_eq!(summary.matched, 1);
    assert_eq!(summary.explained.get("dead_pid"), Some(&1));
}

// ---- E2 owner.json --------------------------------------------------------

#[test]
fn e2_owner_lifecycle_is_shadowed_as_events_and_verifies() {
    let root = tempdir().unwrap();
    let ws = tempdir().unwrap();
    let (registry, _) = fixture_registry(ws.path());
    let pid = std::process::id();
    enable(root.path());
    registry.acquire_lock(11, "sweep-issue-11-a").unwrap();
    registry
        .record_child_pid_in_lock(11, pid, Some(pid), Some("opus"), Some("high"))
        .unwrap();
    registry.stamp_overflow_in_lock(11).unwrap();
    assert_eq!(
        kinds(root.path()),
        [
            "claim.acquired",
            "claim.child_recorded",
            "claim.child_recorded"
        ]
    );
    let all = records(root.path());
    assert_eq!(all[0].data["provisional"], true);
    assert_eq!(all[1].data["op"], "patch");
    assert_eq!(all[1].data["child_pid"], pid);
    assert_eq!(all[1].data["model"], "opus");
    let report = assert_ok(root.path());
    assert_eq!(store_summary(&report, Store::Owner).matched, 1);

    // Tamper with the legacy owner: an unexplained divergence on `model`.
    let owner_path = registry
        .config()
        .locks_dir()
        .join("issue-11")
        .join("owner.json");
    let original = std::fs::read_to_string(&owner_path).unwrap();
    let mut owner: serde_json::Value = serde_json::from_str(&original).unwrap();
    owner["model"] = json!("haiku");
    std::fs::write(&owner_path, owner.to_string()).unwrap();
    let report = verify_concern(root.path());
    assert!(!report.ok);
    assert_eq!(report.unexplained[0].fields, ["model"]);
    std::fs::write(&owner_path, original).unwrap();

    registry.release_lock(11).unwrap();
    assert_eq!(kinds(root.path()).last().unwrap(), "claim.released");
    assert_ok(root.path());
}

#[test]
fn e2_pr_locks_and_owned_release_are_shadowed() {
    let root = tempdir().unwrap();
    let ws = tempdir().unwrap();
    let (registry, _) = fixture_registry(ws.path());
    enable(root.path());
    registry.acquire_pr_lock(3, "sweep-prs-3").unwrap();
    registry
        .record_child_pid_in_pr_lock(3, std::process::id(), None)
        .unwrap();
    assert_ok(root.path());
    assert!(!registry.release_pr_lock_owned(3, "sweep-prs-3").retained());
    let all = records(root.path());
    assert_eq!(all.last().unwrap().kind, "claim.released");
    assert_eq!(all.last().unwrap().data["lock"], "pr");
    assert_eq!(all.last().unwrap().data["subject"]["pr"], 3);
    assert_ok(root.path());
}

#[test]
fn e2_default_off_writes_no_journal() {
    let root = tempdir().unwrap();
    let ws = tempdir().unwrap();
    let (registry, _) = fixture_registry(ws.path());
    disable();
    registry.acquire_lock(12, "sweep-issue-12-a").unwrap();
    registry
        .record_child_pid_in_lock(12, std::process::id(), None, None, None)
        .unwrap();
    registry.release_lock(12).unwrap();
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[test]
fn e2_journal_evidence_never_establishes_liveness() {
    let root = tempdir().unwrap();
    let ws = tempdir().unwrap();
    let (registry, _) = fixture_registry(ws.path());
    let pid = std::process::id();
    enable(root.path());
    registry.acquire_lock(13, "sweep-issue-13-a").unwrap();
    registry
        .record_child_pid_in_lock(13, pid, None, None, None)
        .unwrap();
    // The legacy lock disappears behind the journal's back. The journal
    // still says "acquired by a live pid" — the probe must not care.
    std::fs::remove_dir_all(registry.config().locks_dir().join("issue-13")).unwrap();
    let empty_e1 = ws.path().join("no-sweeps.json");
    assert!(crate::live_claim::probe(ws.path(), Some(&empty_e1), 13).is_none());
    // ...and verification reports the gap instead of acting on it.
    let report = verify_concern(root.path());
    assert!(!report.ok);
    assert_eq!(report.unexplained[0].fields, ["(absent in legacy)"]);
}

// ---- E8 sweep-outcomes.jsonl ---------------------------------------------

fn outcome(issue: u32) -> crate::sweep_outcomes::OutcomeRecord {
    crate::sweep_outcomes::OutcomeRecord {
        timestamp: Utc::now(),
        repo: "/repo/a".to_string(),
        issue,
        sweep_id: format!("sweep-issue-{issue}-0"),
        outcome: "crashed".to_string(),
        exit_code: Some(1),
        death_class: None,
        crash_classification: Some("exit-1".to_string()),
        token_name: "agent-1".to_string(),
        credential: None,
        jev_tier: None,
        jev_confidence: None,
        tap_usage: None,
        tap_usage_all: Vec::new(),
        duration_sec: 12,
    }
}

#[test]
fn e8_terminal_outcomes_are_shadowed_and_verify() {
    let root = tempdir().unwrap();
    let ws = tempdir().unwrap();
    let path = ws.path().join("sweep-outcomes.jsonl");
    disable();
    crate::sweep_outcomes::append_outcome(&path, &outcome(1)).unwrap();
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    enable(root.path());
    crate::sweep_outcomes::append_outcome(&path, &outcome(2)).unwrap();
    let all = records(root.path());
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].kind, "sweep.terminal");
    assert_eq!(all[0].key.as_deref(), Some("terminal:sweep-issue-2-0"));
    assert_eq!(all[0].data["value"]["token_name"], "agent-1");
    let report = assert_ok(root.path());
    let summary = store_summary(&report, Store::Outcomes);
    assert_eq!(summary.matched, 1);
    // The pre-enablement line is reported as unjournaled, not a failure.
    assert_eq!(summary.explained.get("unjournaled"), Some(&1));
    assert_eq!(crate::sweep_outcomes::read_all(&path).len(), 2);
}

// ---- E9 PR-less sidecars --------------------------------------------------

#[test]
fn e9_clear_marks_are_shadowed_and_verify() {
    let root = tempdir().unwrap();
    let ws = tempdir().unwrap();
    let (mut registry, _) = fixture_registry(ws.path());
    enable(root.path());
    registry.note_prless_cleared(21, Utc::now());
    assert_eq!(kinds(root.path()), ["prless.cleared"]);
    let report = assert_ok(root.path());
    assert_eq!(store_summary(&report, Store::PrlessClears).matched, 1);
}

#[test]
fn e9_counted_marks_verify_and_expire_as_retention() {
    let root = tempdir().unwrap();
    let ws = tempdir().unwrap();
    let sidecar = ws.path().join("outcomes.jsonl.prless-counted.json");
    let at = Utc::now();
    std::fs::write(&sidecar, json!({"sweep-a": at}).to_string()).unwrap();
    enable(root.path());
    prless_counted(&sidecar, ws.path(), "sweep-a", at);
    assert_ok(root.path());
    // An old mark the legacy horizon has pruned is explained...
    prless_counted(&sidecar, ws.path(), "sweep-old", at - Duration::days(3));
    assert_ok(root.path());
    // ...a fresh one missing from the legacy sidecar is not.
    prless_counted(&sidecar, ws.path(), "sweep-new", at);
    let report = verify_concern(root.path());
    assert_eq!(report.unexplained.len(), 1);
    assert_eq!(report.unexplained[0].item, "sweep-new");
}

// ---- E10 recovery-events.json ---------------------------------------------

#[test]
fn e10_recovery_events_verify_against_the_legacy_array() {
    let root = tempdir().unwrap();
    let ws = tempdir().unwrap();
    let path = ws.path().join(".loom/metrics/recovery-events.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let event = json!({"timestamp": "2026-10-10T00:00:00Z", "issue": 4, "recovery_type": "commit_and_pr",
                       "reason": "r", "elapsed_seconds": null, "pr_number": 9});
    std::fs::write(&path, json!([event]).to_string()).unwrap();
    disable();
    assert_eq!(recovery_event(ws.path(), 4, &path, &event), ShadowOutcome::Disabled);
    enable(root.path());
    let outcome = recovery_event(ws.path(), 4, &path, &event);
    assert!(matches!(
        outcome,
        ShadowOutcome::Appended {
            verdict: Verdict::Match,
            ..
        }
    ));
    assert_ok(root.path());
    std::fs::write(&path, "[]").unwrap();
    assert!(!verify_concern(root.path()).ok, "a dropped event under the cap is unexplained");
}

// ---- E16 # LOOM_LAUNCH credential attribution -----------------------------

#[test]
fn e16_launch_attribution_is_names_only_and_verifies() {
    let root = tempdir().unwrap();
    let ws = tempdir().unwrap();
    let log = ws.path().join("sweep-issue-5.log");
    let launch = json!({"schema": 1, "runtime": "codex", "model": "m",
                        "credentialSource": "pool", "credentialProvider": "zai",
                        "credentialAccount": "acct-a"})
    .to_string();
    std::fs::write(&log, format!("prose\n# LOOM_LAUNCH {launch}\n")).unwrap();
    enable(root.path());
    let outcome = launch_recorded(ws.path(), &log, &launch);
    assert!(matches!(
        outcome,
        ShadowOutcome::Appended {
            verdict: Verdict::Match,
            ..
        }
    ));
    let rec = &records(root.path())[0];
    assert_eq!(rec.kind, "claim.child_recorded");
    assert_eq!(
        rec.data["credential"],
        json!({"source": "pool", "provider": "zai", "account": "acct-a"})
    );
    assert!(rec.data.get("model").is_none(), "only attribution is journaled");
    assert_ok(root.path());

    // A later launch the journal never saw.
    disable();
    let other = launch.replace("acct-a", "acct-b");
    std::fs::write(&log, format!("# LOOM_LAUNCH {launch}\n# LOOM_LAUNCH {other}\n")).unwrap();
    let report = verify_concern(root.path());
    assert_eq!(report.unexplained[0].fields, ["account"]);
}
