//! Issue #10364 Part B, round 3: a recorded fail-closed removal is
//! finished, never undone. While `.session-drift-removed.json` stands, the
//! pass never `docker start`s or recreates the account's container, and a
//! container that is still present (a `docker rm` that failed after the
//! `docker stop`, or one something else started) is removed when idle, on
//! the backoff schedule if that keeps failing.

use super::safety_tests::{removed_for_a_firewalled_repo, ws_with};
use super::*;
use crate::tokens_pool::session_drift_removal::{self, DriftRemoval};

fn interval(tick: u64) -> u64 {
    tick * DEFAULT_SESSION_RECONCILE_INTERVAL_SECS
}

#[test]
#[serial]
fn a_stopped_but_not_removed_container_is_never_resumed_and_the_rm_is_retried() {
    let (env, _ws_dir, _ws, mut lifecycle) = removed_for_a_firewalled_repo();
    let fake = lifecycle.runner().clone();
    *fake.rm_fails.lock().unwrap() = 1;
    let mut state = ReconcileState::default();
    let first = pass(&mut lifecycle, &env, &host, &mut state, 0);
    assert!(matches!(first[..], [Outcome::Failed { .. }]), "{first:?}");
    assert!(session_drift_removal::read(&[profile(&env, "alice")]).is_some());
    let container = container_name("alice");
    assert!(!fake.containers.lock().unwrap()[&container].running, "stopped, not removed");
    let mut outcomes = Vec::new();
    for tick in 1..6 {
        outcomes.extend(pass(&mut lifecycle, &env, &host, &mut state, interval(tick)));
    }
    assert_eq!(fake.count("start_existing"), 0, "never resumed: {outcomes:?}");
    assert_eq!(fake.count("create"), 0, "{outcomes:?}");
    assert!(
        outcomes
            .iter()
            .any(|o| matches!(o, Outcome::DriftRemoved { .. })),
        "the removal is finished: {outcomes:?}"
    );
    assert!(fake.containers.lock().unwrap().is_empty());
}

#[test]
#[serial]
fn an_rm_that_keeps_failing_backs_off_and_is_never_resumed() {
    let (env, _ws_dir, _ws, mut lifecycle) = removed_for_a_firewalled_repo();
    let fake = lifecycle.runner().clone();
    *fake.rm_fails.lock().unwrap() = u32::MAX;
    let mut state = ReconcileState::default();
    let mut outcomes = Vec::new();
    for tick in 0..20 {
        outcomes.extend(pass(&mut lifecycle, &env, &host, &mut state, interval(tick)));
    }
    assert_eq!(fake.count("start_existing"), 0, "{outcomes:?}");
    assert_eq!(fake.count("create"), 0, "{outcomes:?}");
    // Retried, but on the backoff schedule (t=0, 120, 360, 840), not every pass.
    assert_eq!(fake.count("stop_and_remove"), 4, "{outcomes:?}");
    assert!(outcomes
        .iter()
        .any(|o| matches!(o, Outcome::BackingOff { .. })));
}

/// The record stands (removed earlier) and the container is running again
/// with the denied mount: the containment gap this exists to close.
#[test]
#[serial]
fn a_running_container_under_a_standing_record_is_removed_when_idle_deferred_when_busy() {
    let (env, _ws_dir, ws, mut lifecycle) = removed_for_a_firewalled_repo();
    let fake = lifecycle.runner().clone();
    session_drift_removal::record(
        &profile(&env, "alice"),
        &DriftRemoval {
            schema_version: 1,
            workspace: ws.clone(),
            denied: vec![ws.join("walled")],
            reason: "firewall".into(),
            removed_at_unix_ms: 1,
        },
    )
    .unwrap();
    let container = container_name("alice");
    fake.busy.lock().unwrap().insert(container.clone(), true);
    let mut state = ReconcileState::default();
    assert_eq!(
        pass(&mut lifecycle, &env, &host, &mut state, 0),
        vec![Outcome::DriftDeferred {
            reason: DeferReason::Busy
        }]
    );
    assert_eq!(fake.mutations(), Vec::<String>::new(), "never killed");
    fake.busy.lock().unwrap().insert(container, false);
    let out = pass(&mut lifecycle, &env, &host, &mut state, 60);
    assert!(matches!(out[..], [Outcome::DriftRemoved { .. }]), "{out:?}");
    assert!(fake.containers.lock().unwrap().is_empty());
    assert_eq!(
        pass(&mut lifecycle, &env, &host, &mut state, 120),
        vec![Outcome::DriftRemovalStands]
    );
}

/// A stopped container whose mounts include a positively denied path is not
/// `docker start`ed even without a record: it is removed (and recorded).
#[test]
#[serial]
fn a_stopped_container_with_a_denied_mount_is_never_resumed() {
    let (env, _ws_dir, _ws, mut lifecycle) = removed_for_a_firewalled_repo();
    let fake = lifecycle.runner().clone();
    let container = container_name("alice");
    fake.containers
        .lock()
        .unwrap()
        .get_mut(&container)
        .unwrap()
        .running = false;
    let mut state = ReconcileState::default();
    let out = pass(&mut lifecycle, &env, &host, &mut state, 0);
    assert_eq!(fake.count("start_existing"), 0, "{out:?}");
    assert!(matches!(out[..], [Outcome::DriftRemoved { .. }]), "{out:?}");
    assert!(session_drift_removal::read(&[profile(&env, "alice")]).is_some());
}

#[test]
#[serial]
fn after_an_operator_start_clears_the_record_a_stopped_container_is_resumed_again() {
    let (env, _ws_dir, ws, mut lifecycle) = removed_for_a_firewalled_repo();
    let fake = lifecycle.runner().clone();
    let mut state = ReconcileState::default();
    let out = pass(&mut lifecycle, &env, &host, &mut state, 0);
    assert!(matches!(out[..], [Outcome::DriftRemoved { .. }]), "{out:?}");
    // The operator deregisters the firewalled repo and starts by hand.
    *fake.registered.lock().unwrap() = vec![ws.join("a")];
    lifecycle.start_with_workspace("alice", Some(&ws)).unwrap();
    assert!(session_drift_removal::read(&[profile(&env, "alice")]).is_none());
    assert_eq!(pass(&mut lifecycle, &env, &host, &mut state, 60), vec![Outcome::Running]);
    // An ordinary crash: resumed as in Phase 1.
    let container = container_name("alice");
    fake.containers
        .lock()
        .unwrap()
        .get_mut(&container)
        .unwrap()
        .running = false;
    assert_eq!(pass(&mut lifecycle, &env, &host, &mut state, 120), vec![Outcome::Resumed]);
    let _ = ws_with;
}

// ---- round 4: stopping a RUNNING container needs a positive, current finding

/// alice runs mounting only `a`; a record from an earlier removal (for the
/// then-firewalled `walled`) is still on disk.
fn running_beside_a_leftover_record() -> (Env, tempfile::TempDir, PathBuf, SessionLifecycle<Fake>) {
    let (env, ws_dir, ws, lifecycle) = removed_for_a_firewalled_repo();
    let fake = lifecycle.runner().clone();
    let container = container_name("alice");
    fake.mounts
        .lock()
        .unwrap()
        .insert(container, vec![ws.join("a")]);
    session_drift_removal::record(
        &profile(&env, "alice"),
        &DriftRemoval {
            schema_version: 1,
            workspace: ws.clone(),
            denied: vec![ws.join("walled")],
            reason: "firewall".into(),
            removed_at_unix_ms: 1,
        },
    )
    .unwrap();
    (env, ws_dir, ws, lifecycle)
}

fn left_running(fake: &Fake, out: &[Outcome]) {
    assert_eq!(fake.mutations(), Vec::<String>::new(), "{out:?}");
    assert!(fake.containers.lock().unwrap()[&container_name("alice")].running, "{out:?}");
    assert!(
        !out.iter()
            .any(|o| matches!(o, Outcome::DriftRemoved { .. })),
        "{out:?}"
    );
}

/// The Judge's case: the denial has since been lifted, but the roster cannot
/// be read, so nothing proves the record is over. Unknown is no reason to
/// stop a running container.
#[test]
#[serial]
fn a_leftover_record_never_stops_a_running_container_when_the_roster_is_unreadable() {
    let (env, _ws_dir, _ws, mut lifecycle) = running_beside_a_leftover_record();
    let fake = lifecycle.runner().clone();
    fake.firewalled.lock().unwrap().clear();
    *fake.roster_unreadable.lock().unwrap() = true;
    let mut state = ReconcileState::default();
    let mut out = Vec::new();
    for tick in 0..4 {
        out.extend(pass(&mut lifecycle, &env, &host, &mut state, interval(tick)));
    }
    left_running(&fake, &out);
    assert!(session_drift_removal::read(&[profile(&env, "alice")]).is_some(), "kept");
}

#[test]
#[serial]
fn a_leftover_record_never_stops_a_running_container_when_the_registry_is_unreadable() {
    let (env, _ws_dir, _ws, mut lifecycle) = running_beside_a_leftover_record();
    let fake = lifecycle.runner().clone();
    fake.firewalled.lock().unwrap().clear();
    *fake.registry_unreadable.lock().unwrap() = true;
    let mut state = ReconcileState::default();
    let mut out = Vec::new();
    for tick in 0..4 {
        out.extend(pass(&mut lifecycle, &env, &host, &mut state, interval(tick)));
    }
    left_running(&fake, &out);
}

/// Inputs readable, the recorded denial still stands, but this container
/// mounts nothing denied: it is left running (the record stays, so it still
/// blocks a recreate) and nothing recreates it either.
#[test]
#[serial]
fn a_running_container_with_no_denied_mount_is_left_running_under_a_standing_record() {
    let (env, _ws_dir, _ws, mut lifecycle) = running_beside_a_leftover_record();
    let fake = lifecycle.runner().clone();
    let mut state = ReconcileState::default();
    let mut out = Vec::new();
    for tick in 0..4 {
        out.extend(pass(&mut lifecycle, &env, &host, &mut state, interval(tick)));
    }
    left_running(&fake, &out);
    assert!(out.iter().all(|o| *o == Outcome::Running), "{out:?}");
    assert!(session_drift_removal::read(&[profile(&env, "alice")]).is_some());
}

/// The outcome names only mounts the container actually has.
#[test]
#[serial]
fn a_removed_running_container_reports_only_its_own_denied_mounts() {
    let (env, _ws_dir, ws, mut lifecycle) = removed_for_a_firewalled_repo();
    let fake = lifecycle.runner().clone();
    session_drift_removal::record(
        &profile(&env, "alice"),
        &DriftRemoval {
            schema_version: 1,
            workspace: ws.clone(),
            denied: vec![ws.join("walled"), ws.join("elsewhere")],
            reason: "firewall".into(),
            removed_at_unix_ms: 1,
        },
    )
    .unwrap();
    let mut state = ReconcileState::default();
    assert_eq!(
        pass(&mut lifecycle, &env, &host, &mut state, 0),
        vec![Outcome::DriftRemoved {
            drift: MountDrift {
                missing: Vec::new(),
                extra: vec![ws.join("walled")],
            },
        }]
    );
    let _ = fake;
}
