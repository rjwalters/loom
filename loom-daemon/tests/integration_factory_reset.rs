// Integration test covering the factory reset loop described in docs/testing/factory-reset-loop.md
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]

mod common;

use common::{
    cleanup_all_loom_sessions, get_loom_tmux_sessions, tmux_session_exists, TestClient, TestDaemon,
};
use serial_test::serial;
use tokio::time::{sleep, Duration};
use uuid::Uuid;

/// Ensure we always begin with a clean slate of tmux sessions
fn setup() {
    cleanup_all_loom_sessions();
}

/// Create the standard set of 7 workspace terminals and return their IDs
async fn create_workspace_terminals(client: &mut TestClient) -> Vec<String> {
    let mut ids = Vec::with_capacity(7);

    for i in 1..=7 {
        let config_id = format!("terminal-{i}");
        let name = format!("Terminal {i}");
        let id = client
            .create_terminal_with_config(config_id.clone(), name, None, None, None)
            .await
            .expect("Failed to create terminal");
        ids.push(id);
    }

    ids
}

/// Destroy all provided terminals and wait briefly for tmux to settle
async fn destroy_terminals(client: &mut TestClient, ids: &[String]) {
    for id in ids {
        client
            .destroy_terminal(id)
            .await
            .expect("Failed to destroy terminal");
    }

    // Give tmux a moment to tear down sessions and release output files
    sleep(Duration::from_millis(200)).await;
}

/// Verify a clean state after factory reset: no tmux sessions, no terminal records, no output files
async fn assert_clean_state(client: &mut TestClient, previously_created_ids: &[String]) {
    let terminals = client
        .list_terminals()
        .await
        .expect("Failed to list terminals after cleanup");
    assert!(
        terminals.is_empty(),
        "Expected no terminals after cleanup, found {len}",
        len = terminals.len()
    );

    let sessions = get_loom_tmux_sessions();
    assert!(
        sessions.is_empty(),
        "Expected no tmux sessions after cleanup, found: {sessions:?}"
    );

    for id in previously_created_ids {
        let output_path = std::path::PathBuf::from(format!("/tmp/loom-{id}.out"));
        assert!(
            !output_path.exists(),
            "Expected terminal output file {path} to be removed",
            path = output_path.display()
        );
    }
}

#[tokio::test]
#[serial]
async fn test_factory_reset_loop_creates_clean_slate() {
    setup();

    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let mut client = TestClient::connect(daemon.socket_path())
        .await
        .expect("Failed to connect client");

    // Phase 3 from the documentation: create the full workspace terminal set
    let first_cycle_ids = create_workspace_terminals(&mut client).await;

    // Ensure all terminals are registered and backed by tmux sessions
    let list = client
        .list_terminals()
        .await
        .expect("Failed to list terminals after creation");
    assert_eq!(list.len(), first_cycle_ids.len(), "Terminal list should match created count");

    for id in &first_cycle_ids {
        let session_name = format!("loom-{id}-default-0");
        assert!(
            tmux_session_exists(&session_name),
            "Expected tmux session {session_name} to exist"
        );
    }

    // Phase 4: simulate factory reset by destroying all terminals
    destroy_terminals(&mut client, &first_cycle_ids).await;
    assert_clean_state(&mut client, &first_cycle_ids).await;

    // Repeat startup to ensure no stale state carries over
    let second_cycle_ids = create_workspace_terminals(&mut client).await;

    let list = client
        .list_terminals()
        .await
        .expect("Failed to list terminals after second creation");
    assert_eq!(
        list.len(),
        second_cycle_ids.len(),
        "Terminal list should match second cycle count"
    );

    for id in &second_cycle_ids {
        let session_name = format!("loom-{id}-default-0");
        assert!(
            tmux_session_exists(&session_name),
            "Expected tmux session {session_name} to exist after second cycle"
        );
    }

    destroy_terminals(&mut client, &second_cycle_ids).await;
    assert_clean_state(&mut client, &second_cycle_ids).await;
}

/// Regression test for ISSUE-terminal-session-missing-false-positives:
/// the daemon must report existing tmux sessions even if the terminal
/// has not been registered locally yet.
#[tokio::test]
#[serial]
async fn test_session_health_for_unregistered_terminal() {
    setup();

    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let mut client = TestClient::connect(daemon.socket_path())
        .await
        .expect("Failed to connect client");

    // Manually create a tmux session that matches Loom's naming convention
    let terminal_id = format!("terminal-{}", Uuid::new_v4().simple());
    let session_name = format!("loom-{terminal_id}-default-0");

    let manual_session_status = std::process::Command::new("tmux")
        .args([
            "-L",
            "loom",
            "new-session",
            "-d",
            "-s",
            &session_name,
            "-x",
            "80",
            "-y",
            "24",
        ])
        .status()
        .expect("Failed to spawn manual tmux session");
    assert!(
        manual_session_status.success(),
        "Failed to create manual tmux session {session_name}"
    );

    // Verify tmux reports the session exists
    assert!(
        tmux_session_exists(&session_name),
        "Expected manual tmux session {session_name} to exist"
    );

    // Ask the daemon to check session health for the unregistered terminal ID.
    // This exercises the fallback code path that scans tmux sessions directly.
    let has_session = client
        .check_session_health(&terminal_id)
        .await
        .expect("Failed to check session health");
    assert!(
        has_session,
        "Daemon should report existing session for unregistered terminal {terminal_id}"
    );

    // Clean up the manual session to avoid leaking tmux state
    cleanup_all_loom_sessions();
}
