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

/// M7: the step's own dispatch label, stamped up to a clock skew after the
/// step's local start, is not another claimant's; a label well after it is.
#[test]
fn the_builder_label_check_tolerates_clock_skew() {
    let start = Utc::now();
    let own = start + chrono::Duration::seconds(45);
    assert!(!label_reapplied_after(own, start));
    assert!(!label_reapplied_after(start - chrono::Duration::seconds(3), start));
    assert!(label_reapplied_after(start + chrono::Duration::minutes(5), start));
}

fn role_lease_row(id: u64, sweep: &str) -> String {
    let at = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    format!(
        r#"{{"id":{id},"updated_at":"{at}","body":"<!-- loom:pr-lease role=judge host=h sweep={sweep} -->"}}"#
    )
}

/// B1: a non-Builder step's claim is judged by the role-keyed lease's sweep
/// identity, never by label time.
#[test]
fn a_judge_steps_claim_is_held_elsewhere_only_when_another_sweep_holds_the_lease() {
    let dir = tempdir().unwrap();
    let mine = "step-judge-21-1";
    let reg = crate::sweep_registry::test_support::fixture_registry_with_lease_gh(
        dir.path(),
        &role_lease_row(5, mine),
        0,
    );
    assert!(!reg.step_lease_held_elsewhere(21, StepRole::Judge, mine));
    assert!(reg.step_lease_held_elsewhere(21, StepRole::Judge, "step-judge-21-0"));
    // No live lease of the role on the item (a Doctor asking): not held.
    assert!(!reg.step_lease_held_elsewhere(21, StepRole::Doctor, mine));

    let dir = tempdir().unwrap();
    let peer_first = format!("{}\n{}", role_lease_row(4, "peer"), role_lease_row(5, mine));
    let reg = crate::sweep_registry::test_support::fixture_registry_with_lease_gh(
        dir.path(),
        &peer_first,
        0,
    );
    assert!(
        reg.step_lease_held_elsewhere(21, StepRole::Judge, mine),
        "the earlier lease wins"
    );

    // Unreadable: fail-open, so the claim is restored.
    let dir = tempdir().unwrap();
    let reg =
        crate::sweep_registry::test_support::fixture_registry_with_lease_gh(dir.path(), "", 1);
    assert!(!reg.step_lease_held_elsewhere(21, StepRole::Judge, "anyone"));
}
