//! A step through the registry's consumers (#11898): the reaper's terminal
//! path, occupancy and the watchdogs' exclusions.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::sweep_registry::test_support::{fixture_registry, write_lock_owner};
use std::process::Command;
use tempfile::tempdir;

/// Insert a `Running` step entry with `pid`.
fn insert_step(reg: &mut SweepRegistry, item: u32, role: StepRole, pid: u32) -> String {
    let sweep_id = format!("step-{role}-{item}-1");
    reg.entries.insert(
        sweep_id.clone(),
        SweepInfo {
            pgid: None,
            sweep_id: sweep_id.clone(),
            kind: SweepKind::Step { item, role },
            pid,
            token_name: "unknown".into(),
            runtime: "unknown".into(),
            runtime_source: None,
            log_path: reg.compute_step_log_path(item, role),
            idempotency_key: None,
            started_at: Utc::now(),
            state: SweepState::Running,
            latest_phase: None,
            pr_number: None,
            model: None,
            effort: None,
            depends_on: None,
            repo: None,
            overflow: false,
        },
    );
    sweep_id
}

fn dead_pid() -> u32 {
    let mut child = Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

fn write_pr_lock(reg: &SweepRegistry, pr: u32, sweep_id: &str) -> PathBuf {
    let lock = reg.config.locks_dir().join(format!("pr-{pr}"));
    std::fs::create_dir_all(&lock).unwrap();
    let owner = LockOwner::new(pr, std::process::id(), sweep_id);
    std::fs::write(lock.join("owner.json"), serde_json::to_string(&owner).unwrap()).unwrap();
    lock
}

#[test]
fn a_dead_issue_step_is_reaped_and_its_issue_lock_released() {
    let dir = tempdir().unwrap();
    let (mut reg, _) = fixture_registry(dir.path());
    let id = insert_step(&mut reg, 11_898, StepRole::Curator, dead_pid());
    let lock = write_lock_owner(&reg, 11_898, &id, std::process::id());

    assert_eq!(reg.reap_once(), 1);
    assert!(reg.entries[&id].state.is_terminal(), "{:?}", reg.entries[&id].state);
    assert!(!lock.exists(), "the step's issue lock is released");
}

#[test]
fn a_dead_pr_step_releases_its_pr_lock() {
    let dir = tempdir().unwrap();
    let (mut reg, _) = fixture_registry(dir.path());
    let id = insert_step(&mut reg, 9_001, StepRole::Judge, dead_pid());
    let lock = write_pr_lock(&reg, 9_001, &id);

    assert_eq!(reg.reap_once(), 1);
    assert!(reg.entries[&id].state.is_terminal());
    assert!(!lock.exists(), "the step's PR lock is released");
}

#[test]
fn a_dead_step_leaves_a_newer_owners_lock_alone() {
    let dir = tempdir().unwrap();
    let (mut reg, _) = fixture_registry(dir.path());
    let id = insert_step(&mut reg, 9_002, StepRole::Builder, dead_pid());
    let lock = write_lock_owner(&reg, 9_002, "sweep-issue-9002-newer", std::process::id());

    assert_eq!(reg.reap_once(), 1);
    assert!(reg.entries[&id].state.is_terminal());
    assert!(lock.exists(), "a newer claimant's lock survives");
}

/// A live step holds its item's slot in the occupancy reading the work
/// finder (and `busy_slots{role="sweep"}`) reads, and is visible to status.
#[test]
fn a_live_step_occupies_its_item_and_is_excluded_from_sweep_restarts() {
    let dir = tempdir().unwrap();
    let (mut reg, _) = fixture_registry(dir.path());
    insert_step(&mut reg, 9_003, StepRole::Doctor, std::process::id());
    reg.startup_proof_grace = Duration::from_secs(3600);

    assert!(reg.occupied_issues().contains(&9_003));
    assert_eq!(reg.unproven_startups().len(), 1);
    // The sweep watchdogs re-dispatch `/loom:sweep N`; a step is never theirs.
    assert_eq!(reg.watchdog_once(Duration::ZERO), 0);
    assert_eq!(reg.review_stall_watchdog_once(Duration::ZERO), 0);
}
