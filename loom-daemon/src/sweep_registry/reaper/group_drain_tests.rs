//! Issue #11076: the daemon never releases a claim while a member of the dead
//! leader's process group — e.g. the resilient wrapper's own retry of the
//! session — is still alive.
use super::*;
use crate::sweep_registry::test_support::*;
use serial_test::serial;
use std::process::Command;
use tempfile::tempdir;

/// Spawn a group leader that forks `background` into its own group and then
/// exits with `exit_code`; returns the reaped leader's pid and the survivor's
/// pid. The leader is reaped here, so it is genuinely dead.
fn dead_leader_with_survivor(dir: &Path, background: &str) -> (u32, u32, std::process::Child) {
    let pidfile = dir.join("survivor.pid");
    let mut cmd = Command::new("bash");
    cmd.arg("-c").arg(format!(
        "{background} &\necho \"$!\" > \"{pf}\"\nsleep 0.3\nkill -TERM $$\n",
        pf = pidfile.display()
    ));
    cmd.process_group(0);
    let leader = cmd.spawn().expect("spawn fixture leader");
    let leader_pid = leader.id();
    let survivor =
        read_pid_file(&pidfile, FIXTURE_CHILD_WAIT_MS).expect("survivor pid should be recorded");
    (leader_pid, survivor, leader)
}

fn insert_running(registry: &mut SweepRegistry, sweep_id: &str, issue: u32, pid: u32) {
    registry.entries.insert(
        sweep_id.to_string(),
        SweepInfo {
            sweep_id: sweep_id.to_string(),
            kind: SweepKind::Issue(issue),
            pid,
            pgid: Some(pid),
            token_name: "unknown".into(),
            runtime: "unknown".into(),
            runtime_source: None,
            log_path: registry.compute_log_path(issue),
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
    registry
        .acquire_lock(issue, sweep_id)
        .expect("acquire fixture lock");
}

fn is_running(registry: &SweepRegistry, sweep_id: &str) -> bool {
    matches!(registry.entries[sweep_id].state, SweepState::Running)
}

/// Acceptance criterion 2: a dead leader whose group still holds a live
/// member (the wrapper's `Attempt 2/5`, which traps SIGTERM like the real
/// wrapper does) keeps its claim — lock held, entry `Running` — across the
/// SIGTERM tick and the SIGKILL tick, and is released only on the tick that
/// finds the group empty.
#[test]
#[serial]
fn a_live_wrapper_retry_keeps_the_claim_until_its_group_drains() {
    let dir = tempdir().unwrap();
    let (mut registry, _log) = fixture_registry(dir.path());
    let (leader_pid, retry_pid, mut leader) =
        dead_leader_with_survivor(dir.path(), "bash -c 'trap \"\" TERM; sleep 300'");
    // `Child` caches the status once observed, so the reaper's own `try_wait`
    // still yields it (#10726) — the 143 the gate must remember.
    let _ = leader.wait();
    let sweep_id = "sweep-issue-11076-retry";
    insert_running(&mut registry, sweep_id, 11076, leader_pid);
    registry.children.insert(sweep_id.to_string(), leader);
    let lock = registry.config.locks_dir().join("issue-11076");

    // Tick 1: leader dead, retry alive -> SIGTERM (ignored), nothing released.
    registry.reap_once();
    assert!(is_running(&registry, sweep_id), "sweep must stay live while its retry runs");
    assert!(lock.exists(), "the claim must NOT be released while the retry is alive");
    let pending = registry.pending_group_reaps[sweep_id];
    assert_eq!(pending.exit_code, Some(143), "the leader's SIGTERM exit is remembered");
    assert!(is_pid_alive(retry_pid), "the TERM-trapping retry survives the SIGTERM");

    // Tick 2: escalation due -> SIGKILL. Still nothing released on this tick
    // unless the group has already drained by the time the gate looks.
    registry
        .pending_group_reaps
        .get_mut(sweep_id)
        .unwrap()
        .escalate_at = Instant::now();
    registry.reap_once();
    assert!(wait_until_dead(retry_pid, FIXTURE_CHILD_WAIT_MS), "SIGKILL must end the retry");

    // Group empty -> the ordinary terminal transition runs and releases.
    registry.reap_once();
    assert!(!is_running(&registry, sweep_id), "an empty group ends the sweep");
    assert!(!lock.exists(), "the claim is released once the group is empty");
    assert!(registry.pending_group_reaps.is_empty(), "the gate drops its pending reap");
}

/// The gate hands back the exit code observed on the first tick (when
/// `poll_liveness` consumed the `Child` handle), and its release cap bounds the
/// wait for a group that will not drain, so the entry can never wedge.
#[test]
#[serial]
fn the_release_cap_bounds_the_wait_and_keeps_the_first_exit_code() {
    let dir = tempdir().unwrap();
    let (mut registry, _log) = fixture_registry(dir.path());
    let (leader_pid, retry_pid, mut leader) =
        dead_leader_with_survivor(dir.path(), "bash -c 'trap \"\" TERM; sleep 300'");
    let _ = leader.wait();
    let sweep_id = "sweep-issue-11077-stubborn";
    insert_running(&mut registry, sweep_id, 11077, leader_pid);

    let first = registry.await_group_drain(sweep_id, Some(11077), Some(leader_pid), Some(143));
    assert_eq!(first, GroupDrain::Draining);
    assert_eq!(
        registry.await_group_drain(sweep_id, Some(11077), Some(leader_pid), None),
        GroupDrain::Draining,
        "inside the cap a live group keeps draining"
    );
    registry
        .pending_group_reaps
        .get_mut(sweep_id)
        .unwrap()
        .release_deadline = Instant::now();
    assert_eq!(
        registry.await_group_drain(sweep_id, Some(11077), Some(leader_pid), None),
        GroupDrain::Drained(Some(143)),
        "past the cap the gate releases, with the first tick's exit code"
    );
    assert!(is_pid_alive(retry_pid), "precondition: the cap path, not a drain, released");
    assert!(registry.pending_group_reaps.is_empty());
    send_group_signal(leader_pid, 9);
    assert!(wait_until_dead(retry_pid, FIXTURE_CHILD_WAIT_MS));
}

/// Control: the ordinary death — the leader takes its whole group with it —
/// releases on the first tick, exactly as before #11076.
#[test]
#[serial]
fn an_empty_group_releases_on_the_first_tick() {
    let dir = tempdir().unwrap();
    let (mut registry, _log) = fixture_registry(dir.path());
    let mut cmd = Command::new("bash");
    cmd.arg("-c").arg("exit 0");
    cmd.process_group(0);
    let mut leader = cmd.spawn().expect("spawn fixture leader");
    let leader_pid = leader.id();
    let _ = leader.wait();
    let sweep_id = "sweep-issue-11078-clean";
    insert_running(&mut registry, sweep_id, 11078, leader_pid);
    let lock = registry.config.locks_dir().join("issue-11078");

    registry.reap_once();
    assert!(!is_running(&registry, sweep_id), "a drained group ends the sweep at once");
    assert!(!lock.exists(), "and releases its claim on the same tick");
    assert!(registry.pending_group_reaps.is_empty());
}

/// Control: no recorded pgid (a pre-#4980 entry) cannot be waited on, so the
/// gate is a pass-through carrying this tick's exit code.
#[test]
fn no_recorded_pgid_is_an_immediate_pass_through() {
    let dir = tempdir().unwrap();
    let (mut registry, _log) = fixture_registry(dir.path());
    assert_eq!(
        registry.await_group_drain("sweep-no-pgid", Some(1), None, Some(3)),
        GroupDrain::Drained(Some(3))
    );
    assert!(registry.pending_group_reaps.is_empty());
}
