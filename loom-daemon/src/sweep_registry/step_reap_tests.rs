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
/// identity. (The fixture's `gh` cannot read the timeline, so the #11969
/// label path fails open here.)
#[test]
fn a_judge_steps_claim_is_held_elsewhere_only_when_another_sweep_holds_the_lease() {
    let dir = tempdir().unwrap();
    let mine = "step-judge-21-1";
    let reg = crate::sweep_registry::test_support::fixture_registry_with_lease_gh(
        dir.path(),
        &role_lease_row(5, mine),
        0,
    );
    assert!(!reg.step_lease_held_elsewhere(21, StepRole::Judge, mine, Utc::now()));
    assert!(reg.step_lease_held_elsewhere(21, StepRole::Judge, "step-judge-21-0", Utc::now()));
    // No live lease of the role on the item (a Doctor asking): not held.
    assert!(!reg.step_lease_held_elsewhere(21, StepRole::Doctor, mine, Utc::now()));

    let dir = tempdir().unwrap();
    let peer_first = format!("{}\n{}", role_lease_row(4, "peer"), role_lease_row(5, mine));
    let reg = crate::sweep_registry::test_support::fixture_registry_with_lease_gh(
        dir.path(),
        &peer_first,
        0,
    );
    assert!(
        reg.step_lease_held_elsewhere(21, StepRole::Judge, mine, Utc::now()),
        "the earlier lease wins"
    );

    // Unreadable: fail-open, so the claim is restored.
    let dir = tempdir().unwrap();
    let reg =
        crate::sweep_registry::test_support::fixture_registry_with_lease_gh(dir.path(), "", 1);
    assert!(!reg.step_lease_held_elsewhere(21, StepRole::Judge, "anyone", Utc::now()));
}

/// Review item 5: a PR step's roll requeue gives back only its own claim. A
/// peer that took the role's lease during the roll window keeps the PR.
#[test]
fn a_pr_step_requeue_after_a_roll_checks_the_claim_is_still_its_own() {
    let mine = "step-doctor-22-1";
    let dir = tempdir().unwrap();
    let peer = format!(
        "{}\n{}",
        role_lease_row(3, "peer-doctor").replace("role=judge", "role=doctor"),
        role_lease_row(5, mine).replace("role=judge", "role=doctor"),
    );
    let reg =
        crate::sweep_registry::test_support::fixture_registry_with_lease_gh(dir.path(), &peer, 0);
    assert!(reg
        .roll_pr_step_claim_still_ours(22, StepRole::Doctor, mine)
        .is_err());

    let dir = tempdir().unwrap();
    let own = role_lease_row(5, mine).replace("role=judge", "role=doctor");
    let reg =
        crate::sweep_registry::test_support::fixture_registry_with_lease_gh(dir.path(), &own, 0);
    assert!(reg
        .roll_pr_step_claim_still_ours(22, StepRole::Doctor, mine)
        .is_ok());
}

/// A registry whose fake `gh` serves `lease_rows` for comment reads and
/// appends every other call's arguments to the returned write log.
fn registry_logging_writes(dir: &std::path::Path, lease_rows: &str) -> (SweepRegistry, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let log = dir.join("gh-calls.log");
    let fake_gh = dir.join("fake-gh-logging.sh");
    let script = format!(
        "#!/usr/bin/env bash\n\
         if [[ \"$1\" == \"repo\" && \"$2\" == \"view\" ]]; then printf 'rjwalters/loom\\n'; exit 0; fi\n\
         if [[ \"$1\" == \"api\" && \"$*\" == *\"/comments\"* ]]; then printf '%s' '{rows}'; exit 0; fi\n\
         printf '%s\\n' \"$*\" >> '{log}'\n\
         exit 1\n",
        rows = crate::comment_trust::records::with_fleet_author(lease_rows)
            .replace('\'', "'\\''"),
        log = log.display(),
    );
    std::fs::write(&fake_gh, script).unwrap();
    std::fs::set_permissions(&fake_gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut reg = crate::sweep_registry::test_support::fixture_registry_with_lease_gh(dir, "", 0);
    reg.config.gh_bin = Some(fake_gh);
    (reg, log)
}

/// Judge blocker 1: a dead step's claim restored at startup is subject to the
/// same held-elsewhere check as the reaper's. A live peer lease means no
/// forge write; without one the restore does write.
#[test]
fn a_startup_restore_leaves_a_peer_held_claim_alone() {
    let mine = "step-judge-31-1";
    let dir = tempdir().unwrap();
    let (mut reg, log) = registry_logging_writes(dir.path(), &role_lease_row(3, "peer-judge"));
    reg.restore_dead_step_at_startup(31, StepRole::Judge, mine, Utc::now());
    assert!(
        std::fs::read_to_string(&log).unwrap_or_default().is_empty(),
        "a live peer lease means nothing is written"
    );

    let dir = tempdir().unwrap();
    let (mut reg, log) = registry_logging_writes(dir.path(), &role_lease_row(5, mine));
    reg.restore_dead_step_at_startup(31, StepRole::Judge, mine, Utc::now());
    assert!(
        !std::fs::read_to_string(&log).unwrap_or_default().is_empty(),
        "its own lease (or none) lets the restore write"
    );
}

/// Should-fix: a PR step's own claim label lands after its start (the role
/// prompt applies it), so the roll requeue judges by the lease alone.
#[test]
fn a_pr_step_roll_requeue_ignores_label_time() {
    let mine = "step-doctor-23-1";
    let dir = tempdir().unwrap();
    let own = role_lease_row(5, mine).replace("role=judge", "role=doctor");
    let reg =
        crate::sweep_registry::test_support::fixture_registry_with_lease_gh(dir.path(), &own, 0);
    // No lease-independent label read exists any more: the lease alone decides.
    assert!(reg
        .roll_pr_step_claim_still_ours(23, StepRole::Doctor, mine)
        .is_ok());
}

/// #11969: a non-Builder claim label is the step's own while its lease was
/// live, whenever after the start the prompt applied it (B1); a label applied
/// after that lease ended, or with no lease of the step's at all, is another
/// claimant's. Within the start's clock-skew slack it is always the step's.
#[test]
fn a_non_builder_label_is_foreign_only_outside_the_steps_own_lease() {
    let start = Utc::now() - chrono::Duration::hours(1);
    let m = chrono::Duration::minutes;
    let released = start + m(20);
    // The prompt's own label, minutes after the start, inside the lease.
    assert!(!label_claimed_elsewhere(start + m(3), start, Some(released)));
    // Inside the skew slack at either end.
    assert!(!label_claimed_elsewhere(start + chrono::Duration::seconds(30), start, None));
    assert!(!label_claimed_elsewhere(
        released + chrono::Duration::seconds(30),
        start,
        Some(released)
    ));
    // After the lease was released or reaped: an attended Doctor or Curator.
    assert!(label_claimed_elsewhere(released + m(5), start, Some(released)));
    // No lease record of the step's at all.
    assert!(label_claimed_elsewhere(start + m(5), start, None));
}

/// A registry whose fake `gh` serves `lease_rows` for the comments read and
/// `labeled_at` for the claim label's timeline read, and logs every other
/// call (the writes).
fn registry_with_label_time(
    dir: &std::path::Path,
    lease_rows: &str,
    labeled_at: DateTime<Utc>,
) -> (SweepRegistry, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let (mut reg, log) = registry_logging_writes(dir, lease_rows);
    let fake_gh = dir.join("fake-gh-timeline.sh");
    let script = format!(
        "#!/usr/bin/env bash\n\
         if [[ \"$1\" == \"api\" && \"$*\" == *\"/timeline\"* ]]; then printf '%s\\n' '{at}'; exit 0; fi\n\
         exec '{inner}' \"$@\"\n",
        at = labeled_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        inner = dir.join("fake-gh-logging.sh").display(),
    );
    std::fs::write(&fake_gh, script).unwrap();
    std::fs::set_permissions(&fake_gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    reg.config.gh_bin = Some(fake_gh);
    (reg, log)
}

fn released_doctor_row(id: u64, sweep: &str, released_at: DateTime<Utc>) -> String {
    let at = released_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    format!(
        r#"{{"id":{id},"updated_at":"{at}","body":"<!-- loom:pr-lease-released role=doctor host=h sweep={sweep} -->"}}"#
    )
}

/// #11969: a dead daemon Doctor step whose PR an attended Doctor has since
/// claimed by label alone (no lease) leaves that label alone; its own label,
/// applied while its lease was live, is still released.
#[test]
fn a_dead_doctor_step_leaves_an_attended_label_only_claim_alone() {
    let mine = "step-doctor-41-1";
    let start = Utc::now() - chrono::Duration::minutes(40);
    let released = start + chrono::Duration::minutes(20);
    let rows = released_doctor_row(5, mine, released);

    let dir = tempdir().unwrap();
    let (mut reg, log) =
        registry_with_label_time(dir.path(), &rows, released + chrono::Duration::minutes(10));
    assert!(reg.step_claim_held_elsewhere(41, StepRole::Doctor, mine, start));
    assert!(reg.release_dead_step(mine, 41, StepRole::Doctor, start, StepEnd::Exited(None)));
    assert!(
        std::fs::read_to_string(&log).unwrap_or_default().is_empty(),
        "the attended Doctor's label is not stripped"
    );

    let dir = tempdir().unwrap();
    let (mut reg, log) =
        registry_with_label_time(dir.path(), &rows, start + chrono::Duration::minutes(2));
    assert!(!reg.step_claim_held_elsewhere(41, StepRole::Doctor, mine, start));
    assert!(!reg.release_dead_step(mine, 41, StepRole::Doctor, start, StepEnd::Exited(None)));
    assert!(
        !std::fs::read_to_string(&log).unwrap_or_default().is_empty(),
        "its own label, applied under its lease, is released"
    );
}

/// #11969: the same for a Curator step, whose lease is issue-scoped, and a
/// startup restore after downtime: the lease aged out, and the label came
/// after it.
#[test]
fn a_startup_curator_restore_leaves_a_later_label_only_claim_alone() {
    let mine = "step-curator-42-1";
    let start = Utc::now() - chrono::Duration::hours(3);
    let renewed =
        (start + chrono::Duration::minutes(5)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let rows = format!(
        r#"{{"id":5,"updated_at":"{renewed}","body":"<!-- loom:pr-lease role=curator host=h sweep={mine} -->"}}"#
    );
    let dir = tempdir().unwrap();
    let (mut reg, log) =
        registry_with_label_time(dir.path(), &rows, Utc::now() - chrono::Duration::minutes(30));
    reg.restore_dead_step_at_startup(42, StepRole::Curator, mine, start);
    assert!(std::fs::read_to_string(&log).unwrap_or_default().is_empty());
}
