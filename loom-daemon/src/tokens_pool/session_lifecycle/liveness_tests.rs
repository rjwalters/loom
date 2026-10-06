//! Tests for session-container liveness in selection (Issues #10454, #10660).
//!
//! The selection-side tests live here rather than in `select_tests.rs`, which
//! is at the file-size threshold — and Codex selection is
//! `account_registry::select_codex_account_where`, not `select.rs` (the Claude
//! token selector).

use super::test_support::{running, set, set_selector_docker, states, unavailable};
use super::*;
use crate::tokens_pool::account_registry::{select_account, AccountId, InventoryProvenance};
use crate::tokens_pool::session_lifecycle::SESSION_MARKER_FILE;
use crate::tokens_pool::CredentialKind;
use serial_test::serial;
use std::fs;
use std::path::Path;

fn account(profiles: &Path, name: &str, session_managed: bool) -> AccountDescriptor {
    let dir = profiles.join(name);
    fs::create_dir_all(&dir).unwrap();
    if session_managed {
        fs::write(dir.join(SESSION_MARKER_FILE), "{}").unwrap();
    }
    AccountDescriptor {
        id: AccountId {
            provider: AccountProvider::Codex,
            name: name.to_string(),
        },
        credential_kind: CredentialKind::CodexHome,
        credential_reference: dir,
        enabled: true,
        provenance: InventoryProvenance::Shared,
        email: None,
    }
}

/// The mapping from the shared snapshot's states: stopped, restarting and
/// missing are down; running is not, and neither is a running container with
/// stale mounts (it still serves the roots it does mount). An unavailable
/// snapshot says nothing about any container.
#[test]
fn down_is_the_shared_is_down_rule_and_stale_mounts_is_not_down() {
    use crate::tokens_pool::session_state::SessionState::{
        Missing, Restarting, Running, StaleMounts, Stopped,
    };
    let live = states(&[
        ("up", Running),
        ("stale", StaleMounts),
        ("stopped", Stopped),
        ("looping", Restarting),
    ])
    .unwrap();
    assert_eq!(live.state_of("absent"), Some(Missing));
    for (name, down) in [
        ("up", false),
        ("stale", false),
        ("stopped", true),
        ("looping", true),
        ("absent", true),
    ] {
        assert_eq!(live.is_down(name), down, "{name}");
    }
    let blind = unavailable().unwrap();
    assert_eq!(blind.state_of("stopped"), None);
    assert!(!blind.is_down("stopped") && !blind.is_down("absent"));
}

#[test]
fn session_down_needs_both_a_session_marker_and_a_successful_listing() {
    let profiles = tempfile::tempdir().unwrap();
    let session = account(profiles.path(), "agent-1", true);
    let bare_metal = account(profiles.path(), "agent-2", false);
    let nothing_running = Some(SessionLiveness::default());
    assert!(is_session_down(&session, nothing_running.as_ref()));
    assert!(!is_session_down(&session, running(&["agent-1"]).as_ref()));
    // Fail open: no fresh snapshot, or Docker could not be queried, so
    // nothing is "down".
    assert!(!is_session_down(&session, None));
    assert!(!is_session_down(&session, unavailable().as_ref()));
    // A bare-metal account has no container to be down.
    assert!(!is_session_down(&bare_metal, nothing_running.as_ref()));
}

#[test]
fn live_preferred_narrows_only_when_something_is_down_and_something_is_live() {
    let profiles = tempfile::tempdir().unwrap();
    let inventory = vec![
        account(profiles.path(), "agent-1", true),
        account(profiles.path(), "agent-2", true),
    ];
    let live = set(running(&["agent-2"]));
    let names: Vec<_> = live_preferred(&inventory)
        .unwrap()
        .into_iter()
        .map(|a| a.id.name)
        .collect();
    assert_eq!(names, ["agent-2"]);
    drop(live);
    // All up, all down, or Docker unobservable: nothing to prefer.
    for liveness in [running(&["agent-1", "agent-2"]), running(&[]), None] {
        let _guard = set(liveness.clone());
        assert_eq!(live_preferred(&inventory), None, "{liveness:?}");
    }
}

/// AC2: with one session down and one up, Codex selection hands out the live
/// account every time instead of round-robining onto the down one (which
/// spawn-codex.sh would refuse with exit 78). With Docker unobservable the
/// round-robin is untouched (fail open).
#[test]
#[serial]
fn selection_prefers_a_live_session_account() {
    let workspace = tempfile::tempdir().unwrap();
    let profiles = tempfile::tempdir().unwrap();
    let _root = crate::tokens_pool::profile_root_env::ProfileRootEnv::set(profiles.path());
    let prior_probe = std::env::var_os("LOOM_CODEX_SESSION_PROBE");
    // The in-container auth probe is a separate concern and would shell out.
    std::env::set_var("LOOM_CODEX_SESSION_PROBE", "0");
    account(profiles.path(), "agent-1", true);
    account(profiles.path(), "agent-2", true);

    let pick = || {
        select_account(workspace.path(), AccountProvider::Codex, None)
            .unwrap()
            .id
            .name
    };
    {
        let _live = set(running(&["agent-2"]));
        for _ in 0..4 {
            assert_eq!(pick(), "agent-2");
        }
    }
    let unobservable: std::collections::HashSet<_> = (0..4).map(|_| pick()).collect();
    assert_eq!(unobservable.len(), 2, "fail open keeps both accounts in rotation");

    match prior_probe {
        Some(value) => std::env::set_var("LOOM_CODEX_SESSION_PROBE", value),
        None => std::env::remove_var("LOOM_CODEX_SESSION_PROBE"),
    }
}

/// A restarting container is passed over, a stale-mounts one is not, and a
/// held account stays out of the preferred set like any other down one.
#[test]
fn live_preferred_follows_the_snapshot_state_and_ignores_the_hold() {
    use crate::tokens_pool::session_state::SessionState::{Restarting, StaleMounts};
    let profiles = tempfile::tempdir().unwrap();
    let inventory = vec![
        account(profiles.path(), "looping", true),
        account(profiles.path(), "stale", true),
        account(profiles.path(), "held", true),
        account(profiles.path(), "bare-metal", false),
    ];
    session_hold::write_hold(&inventory[2].credential_reference, session_hold::now_unix_ms())
        .unwrap();
    assert!(is_held(&inventory[2]) && !is_held(&inventory[0]));
    let live = set(states(&[("looping", Restarting), ("stale", StaleMounts)]));
    let names: Vec<_> = live_preferred(&inventory)
        .unwrap()
        .into_iter()
        .map(|a| a.id.name)
        .collect();
    assert_eq!(names, ["stale", "bare-metal"]);
    drop(live);
    // A hold alone marks nothing down: with no snapshot there is nothing to
    // prefer, exactly as for any other unobservable container.
    let _blind = set(unavailable());
    assert_eq!(live_preferred(&inventory), None);
}

/// The out-of-process selector (`tokens select`): one bounded snapshot from
/// the fake `docker` `session_state`'s own tests use, classified by the shared
/// code. Also: the selector's no-process cases.
#[cfg(unix)]
mod selector_snapshot {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};

    /// A fake `docker` that logs each subcommand to `calls`, then runs `body`.
    fn fake_docker(dir: &Path, body: &str) -> (String, std::path::PathBuf) {
        let calls = dir.join("calls");
        let path = dir.join("docker");
        let script = format!("#!/bin/sh\necho \"$1\" >> '{}'\n{body}\n", calls.display());
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        (path.to_string_lossy().into_owned(), calls)
    }

    fn calls(log: &Path) -> String {
        fs::read_to_string(log).unwrap_or_default()
    }

    /// `ps` lists agent-1 (restarting) and agent-2 (running); agent-3 has no
    /// container.
    const TWO_CONTAINERS: &str = r#"case "$1" in
  ps) printf 'loom-codex-session-agent-1\nloom-codex-session-agent-2\n' ;;
  inspect) printf '[{"Name":"/loom-codex-session-agent-1","State":{"Running":true,"Restarting":true}},{"Name":"/loom-codex-session-agent-2","State":{"Running":true,"Restarting":false}}]' ;;
esac"#;

    #[test]
    fn one_snapshot_marks_restarting_and_missing_down() {
        let profiles = tempfile::tempdir().unwrap();
        let inventory = vec![
            account(profiles.path(), "agent-1", true),
            account(profiles.path(), "agent-2", true),
            account(profiles.path(), "agent-3", true),
        ];
        let (docker, log) = fake_docker(profiles.path(), TWO_CONTAINERS);
        let _seam = set_selector_docker(&docker, false);
        let names: Vec<_> = live_preferred(&inventory)
            .unwrap()
            .into_iter()
            .map(|a| a.id.name)
            .collect();
        assert_eq!(names, ["agent-2"]);
        assert_eq!(calls(&log), "ps\ninspect\n", "one snapshot: one ps, one inspect");
    }

    /// End to end through `select_account`, as `tokens select` runs it.
    #[test]
    #[serial]
    fn tokens_select_picks_the_live_account_from_its_own_snapshot() {
        let workspace = tempfile::tempdir().unwrap();
        let profiles = tempfile::tempdir().unwrap();
        let _root = crate::tokens_pool::profile_root_env::ProfileRootEnv::set(profiles.path());
        let prior_probe = std::env::var_os("LOOM_CODEX_SESSION_PROBE");
        std::env::set_var("LOOM_CODEX_SESSION_PROBE", "0");
        account(profiles.path(), "agent-1", true);
        account(profiles.path(), "agent-2", true);
        let bin = tempfile::tempdir().unwrap();
        let (docker, log) = fake_docker(bin.path(), TWO_CONTAINERS);
        {
            let _seam = set_selector_docker(&docker, false);
            for _ in 0..3 {
                let picked = select_account(workspace.path(), AccountProvider::Codex, None);
                assert_eq!(picked.unwrap().id.name, "agent-2");
            }
        }
        assert_eq!(calls(&log), "ps\ninspect\n".repeat(3), "one snapshot per selection");
        match prior_probe {
            Some(value) => std::env::set_var("LOOM_CODEX_SESSION_PROBE", value),
            None => std::env::remove_var("LOOM_CODEX_SESSION_PROBE"),
        }
    }

    #[test]
    fn an_unqueryable_or_wedged_docker_fails_open() {
        let profiles = tempfile::tempdir().unwrap();
        let inventory = vec![account(profiles.path(), "agent-1", true)];
        let (docker, _) =
            fake_docker(profiles.path(), "echo 'Cannot connect to the Docker daemon' >&2; exit 1");
        let seam = set_selector_docker(&docker, false);
        let live = for_selector(&inventory).expect("a snapshot was taken");
        assert_eq!(live.state_of("agent-1"), None, "unavailable, not missing");
        assert!(!is_session_down(&inventory[0], Some(&live)));
        assert_eq!(live_preferred(&inventory), None);
        drop(seam);
        let _absent = set_selector_docker("/nonexistent/docker", false);
        assert_eq!(live_preferred(&inventory), None);
    }

    #[test]
    fn no_docker_call_without_a_session_managed_account_or_inside_the_daemon() {
        let profiles = tempfile::tempdir().unwrap();
        let (docker, log) = fake_docker(profiles.path(), "exec sleep 30");
        // Bare-metal and disabled-session pools: nothing to ask Docker about.
        let mut disabled = account(profiles.path(), "off", true);
        disabled.enabled = false;
        let pool = vec![account(profiles.path(), "bare-metal", false), disabled];
        {
            let _seam = set_selector_docker(&docker, false);
            assert_eq!(for_selector(&pool), None);
        }
        // A process that runs the watch reads what it published and nothing
        // else, even with no fresh snapshot and a wedged Docker.
        let session = vec![account(profiles.path(), "agent-1", true)];
        let _seam = set_selector_docker(&docker, true);
        let started = Instant::now();
        assert_eq!(for_selector(&session), None);
        assert_eq!(live_preferred(&session), None);
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(calls(&log), "", "no docker process was started");
    }

    #[test]
    fn a_published_snapshot_wins_over_asking_docker() {
        let profiles = tempfile::tempdir().unwrap();
        let (docker, log) = fake_docker(profiles.path(), "exit 9");
        let published = running(&["agent-1"]);
        let read = selector_read(published.clone(), false, Some(&docker));
        assert_eq!(read, published);
        assert_eq!(calls(&log), "");
    }
}
