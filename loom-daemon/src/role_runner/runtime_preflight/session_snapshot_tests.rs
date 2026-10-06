//! #10660: the pre-spawn gate reads session-container liveness from the
//! shared snapshot (`tokens_pool::session_state`) — split from `tests.rs`
//! (file-size ratchet).

use super::*;
use crate::tokens_pool::session_hold;
use crate::tokens_pool::session_lifecycle::liveness::test_support::{
    running, set, set_selector_docker, states, unavailable,
};
use crate::tokens_pool::session_state::SessionState::{Restarting, StaleMounts, Stopped};

/// A pool of four enabled accounts: three session-managed, one bare-metal.
fn pool(profiles: &Path, workspace: &Path) {
    for name in ["agent-1", "agent-2", "agent-3"] {
        mark_session_managed(profiles, name);
    }
    fs::create_dir(profiles.join("bare-metal")).unwrap();
    fs::create_dir_all(workspace.join(".loom")).unwrap();
}

/// Stopped, restarting and missing containers are not spawnable; a running
/// one with stale mounts is; the bare-metal account never depends on any of
/// it.
#[test]
#[serial]
fn the_count_follows_the_snapshot_state() {
    let workspace = tempfile::tempdir().unwrap();
    let profiles = tempfile::tempdir().unwrap();
    let _env = EnvGuard::new(profiles.path());
    pool(profiles.path(), workspace.path());
    let now = epoch_now();
    let count = || {
        let state = codex_pool_state(workspace.path(), now);
        (state.enabled, state.spawnable, state.session_down, state.session_held)
    };
    {
        // agent-3 has no container at all: missing.
        let _live = set(states(&[("agent-1", Stopped), ("agent-2", Restarting)]));
        assert_eq!(count(), (4, 1, 3, 0), "only the bare-metal account is spawnable");
    }
    {
        let _live = set(states(&[
            ("agent-1", StaleMounts),
            ("agent-2", StaleMounts),
            ("agent-3", StaleMounts),
        ]));
        assert_eq!(count(), (4, 4, 0, 0), "stale mounts is not down");
    }
    {
        let _live = set(running(&["agent-1", "agent-2", "agent-3"]));
        assert_eq!(count(), (4, 4, 0, 0));
    }
}

/// Cannot observe is never down: Docker unqueryable in a fresh snapshot, or
/// no fresh snapshot at all (the watch has not run yet, or its last pass is
/// too old).
#[test]
#[serial]
fn an_unavailable_or_absent_snapshot_leaves_every_account_spawnable() {
    let workspace = tempfile::tempdir().unwrap();
    let profiles = tempfile::tempdir().unwrap();
    let _env = EnvGuard::new(profiles.path());
    pool(profiles.path(), workspace.path());
    // Even a held account: the hold alone marks nothing down.
    session_hold::write_hold(&profiles.path().join("agent-1"), session_hold::now_unix_ms())
        .unwrap();
    for (what, liveness) in [("unavailable", unavailable()), ("no fresh snapshot", None)] {
        let _live = set(liveness);
        let state = codex_pool_state(workspace.path(), epoch_now());
        assert_eq!(
            (state.spawnable, state.session_down, state.session_held),
            (4, 0, 0),
            "{what}: {state:?}"
        );
    }
}

/// With Docker wedged the gate neither blocks nor starts a `docker` process:
/// it has no Docker read of its own. The fake would be reached by the
/// out-of-process selector's snapshot, and is not touched here.
#[cfg(unix)]
#[test]
#[serial]
fn the_gate_starts_no_docker_process_even_with_docker_wedged() {
    let workspace = tempfile::tempdir().unwrap();
    let profiles = tempfile::tempdir().unwrap();
    let _env = EnvGuard::new(profiles.path());
    pool(profiles.path(), workspace.path());
    let bin = tempfile::tempdir().unwrap();
    let calls = bin.path().join("calls");
    let docker = bin.path().join("docker");
    fs::write(
        &docker,
        format!("#!/bin/sh\necho \"$1\" >> '{}'\nexec sleep 30\n", calls.display()),
    )
    .unwrap();
    fs::set_permissions(&docker, fs::Permissions::from_mode(0o755)).unwrap();
    let _seam = set_selector_docker(&docker.to_string_lossy(), false);

    let started = std::time::Instant::now();
    let state = codex_pool_state(workspace.path(), epoch_now());
    assert!(started.elapsed() < std::time::Duration::from_secs(5), "{:?}", started.elapsed());
    assert_eq!((state.spawnable, state.session_down), (4, 0), "fail open: {state:?}");
    assert!(!calls.exists(), "the gate ran docker: {:?}", fs::read_to_string(&calls));
}

/// A held account is down like any other stopped one, but the reason says it
/// is held and offers no `session start` hint; a mix names the hint only for
/// the accounts that are not held.
#[test]
#[serial]
fn a_held_account_is_reported_as_held_with_no_start_hint() {
    let workspace = tempfile::tempdir().unwrap();
    let profiles = tempfile::tempdir().unwrap();
    let _env = EnvGuard::new(profiles.path());
    mark_session_managed(profiles.path(), "agent-1");
    mark_session_managed(profiles.path(), "agent-2");
    fs::create_dir_all(workspace.path().join(".loom")).unwrap();
    let hold = |name: &str| {
        session_hold::write_hold(&profiles.path().join(name), session_hold::now_unix_ms()).unwrap();
    };
    let _down = set(running(&[]));
    let read = || codex_pool_state(workspace.path(), epoch_now());

    let reason = codex_exhausted_reason(&read());
    assert!(reason.contains("2/2 session") && reason.contains("session start"), "{reason}");
    assert!(!reason.contains("held"), "{reason}");

    hold("agent-1");
    let state = read();
    assert_eq!(
        (state.spawnable, state.session_down, state.session_held),
        (0, 2, 1),
        "{state:?}"
    );
    let reason = codex_exhausted_reason(&state);
    assert!(reason.contains("SessionDown") && reason.contains("2/2 session"), "{reason}");
    assert!(reason.contains("1 held (operator stop)"), "{reason}");
    assert!(reason.contains("session start <name>` for the 1 not held"), "{reason}");

    hold("agent-2");
    let state = read();
    assert_eq!(
        (state.spawnable, state.session_down, state.session_held),
        (0, 2, 2),
        "{state:?}"
    );
    let reason = codex_exhausted_reason(&state);
    assert!(reason.contains("SessionDown") && reason.contains("2/2 session"), "{reason}");
    assert!(reason.contains("held (operator stop)"), "{reason}");
    assert!(!reason.contains("session start"), "no hint to undo the stop: {reason}");
}

/// The pinned-runtime skip, end to end, for a held account: the tick is
/// passed over and the role log says held, not "start it".
#[test]
#[serial(loom_shared_tokens_dir_env)]
fn codex_pinned_role_skip_says_held_for_an_operator_stopped_account() {
    codex_pinned_role_skip_says_held_for_an_operator_stopped_account_body();
}

#[serial]
fn codex_pinned_role_skip_says_held_for_an_operator_stopped_account_body() {
    let workspace = tempfile::tempdir().unwrap();
    let profiles = tempfile::tempdir().unwrap();
    let _env = EnvGuard::new(profiles.path());
    mark_session_managed(profiles.path(), "agent-4");
    session_hold::write_hold(&profiles.path().join("agent-4"), session_hold::now_unix_ms())
        .unwrap();
    let (marker, _pool) = codex_judge_workspace(workspace.path(), CODEX_MANIFEST, false);
    let _down = set(states(&[("agent-4", Stopped)]));

    let outcome = judge_runner(workspace.path()).invoke("judge", "/loom:judge");

    assert!(matches!(outcome, RoleTickOutcome::PoolExhausted { .. }), "{outcome:?}");
    assert!(!marker.exists(), "the doomed spawn must never run");
    let log = judge_log(workspace.path());
    assert!(log.contains("SessionDown") && log.contains("held (operator stop)"), "{log}");
    assert!(!log.contains("session start"), "{log}");
}
