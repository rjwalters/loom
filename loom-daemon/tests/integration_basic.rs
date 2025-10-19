// Integration tests - expect/unwrap are acceptable here since tests should panic on failure
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]

mod common;

use common::{
    capture_terminal_output, cleanup_all_loom_sessions, tmux_session_exists, TestClient, TestDaemon,
};
use serial_test::serial;

/// Cleanup helper to run before/after tests
fn setup() {
    cleanup_all_loom_sessions();
}

/// Test 1.1: Basic Ping/Pong communication
#[tokio::test]
#[serial]
async fn test_ping_pong() {
    setup();

    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let mut client = TestClient::connect(daemon.socket_path())
        .await
        .expect("Failed to connect");

    // Send ping and verify pong
    client.ping().await.expect("Ping failed");

    // Verify connection still works after ping
    client.ping().await.expect("Second ping failed");
}

/// Test 1.2: Error handling - malformed JSON
#[tokio::test]
#[serial]
async fn test_malformed_json() {
    setup();

    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let mut client = TestClient::connect(daemon.socket_path())
        .await
        .expect("Failed to connect");

    // Send malformed JSON (missing required "type" field)
    let malformed = serde_json::json!({"InvalidRequest": "test"});
    let result = client.send_request(malformed).await;

    // Daemon currently closes connection on malformed requests
    // This is reasonable behavior - malformed requests indicate a protocol error
    assert!(result.is_err(), "Malformed request should result in error");

    // Since connection is closed, we can't send more requests
    // This documents current behavior: daemon closes connection on protocol errors
}

/// Test 2.1: Create terminal
#[tokio::test]
#[serial]
async fn test_create_terminal() {
    setup();

    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let mut client = TestClient::connect(daemon.socket_path())
        .await
        .expect("Failed to connect");

    // Create terminal
    let terminal_id = client
        .create_terminal("test-terminal", None)
        .await
        .expect("Failed to create terminal");

    assert!(!terminal_id.is_empty(), "Terminal ID should not be empty");

    // Verify tmux session exists
    // Terminal ID format: config_id, tmux session: loom-{config_id}-{role}-{instance}
    // For default role and instance 0: loom-{config_id}-default-0
    let session_name = format!("loom-{terminal_id}-default-0");
    assert!(tmux_session_exists(&session_name), "tmux session {session_name} should exist");

    // Cleanup
    cleanup_all_loom_sessions();
}

/// Test 2.2: Create terminal with working directory
#[tokio::test]
#[serial]
async fn test_create_terminal_with_working_dir() {
    setup();

    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let mut client = TestClient::connect(daemon.socket_path())
        .await
        .expect("Failed to connect");

    // Create temp directory
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let working_dir = temp_dir.path().to_str().unwrap().to_string();

    // Create terminal with working directory
    let terminal_id = client
        .create_terminal("test-terminal-wd", Some(working_dir.clone()))
        .await
        .expect("Failed to create terminal");

    assert!(!terminal_id.is_empty());

    // Verify tmux session exists
    let session_name = format!("loom-{terminal_id}-default-0");
    assert!(tmux_session_exists(&session_name));

    // Wait for terminal shell to initialize
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    // Send pwd command to verify working directory
    client
        .send_input(&terminal_id, "pwd\r")
        .await
        .expect("Failed to send pwd command");

    // Wait for command to execute and output to appear
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    // Capture terminal output
    let output = capture_terminal_output(&session_name).expect("Failed to capture terminal output");

    // Verify working directory appears in output
    // The output should contain the actual directory path from pwd command
    assert!(
        output.contains(&working_dir),
        "Expected working directory '{}' in output, got: '{}'",
        working_dir,
        output
    );

    // Cleanup
    cleanup_all_loom_sessions();
}

/// Test 2.3: List terminals
#[tokio::test]
#[serial]
async fn test_list_terminals() {
    setup();

    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let mut client = TestClient::connect(daemon.socket_path())
        .await
        .expect("Failed to connect");

    // Initially should be empty
    let terminals = client
        .list_terminals()
        .await
        .expect("Failed to list terminals");
    assert_eq!(terminals.len(), 0, "Should start with no terminals");

    // Create 3 terminals
    let id1 = client
        .create_terminal("terminal-1", None)
        .await
        .expect("Failed to create terminal 1");
    let id2 = client
        .create_terminal("terminal-2", None)
        .await
        .expect("Failed to create terminal 2");
    let id3 = client
        .create_terminal("terminal-3", None)
        .await
        .expect("Failed to create terminal 3");

    // List should now show 3 terminals
    let terminals = client
        .list_terminals()
        .await
        .expect("Failed to list terminals");
    assert_eq!(terminals.len(), 3, "Should have 3 terminals");

    // Verify all IDs are present
    let ids: Vec<String> = terminals
        .iter()
        .filter_map(|t| t.get("id")?.as_str())
        .map(String::from)
        .collect();

    assert!(ids.contains(&id1), "Should contain terminal 1");
    assert!(ids.contains(&id2), "Should contain terminal 2");
    assert!(ids.contains(&id3), "Should contain terminal 3");

    // Verify metadata fields exist
    for terminal in &terminals {
        assert!(terminal.get("id").is_some(), "Should have id");
        assert!(terminal.get("name").is_some(), "Should have name");
        assert!(terminal.get("tmux_session").is_some(), "Should have tmux_session");
        assert!(terminal.get("created_at").is_some(), "Should have created_at");
    }

    // Cleanup
    cleanup_all_loom_sessions();
}

/// Test 2.4: Destroy terminal
#[tokio::test]
#[serial]
async fn test_destroy_terminal() {
    setup();

    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let mut client = TestClient::connect(daemon.socket_path())
        .await
        .expect("Failed to connect");

    // Create terminal
    let terminal_id = client
        .create_terminal("test-terminal", None)
        .await
        .expect("Failed to create terminal");

    let session_name = format!("loom-{terminal_id}-default-0");
    assert!(tmux_session_exists(&session_name));

    // Destroy terminal
    client
        .destroy_terminal(&terminal_id)
        .await
        .expect("Failed to destroy terminal");

    // Verify tmux session is gone
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    assert!(!tmux_session_exists(&session_name), "tmux session should be killed");

    // Verify terminal no longer in list
    let terminals = client
        .list_terminals()
        .await
        .expect("Failed to list terminals");
    assert_eq!(terminals.len(), 0, "Should have no terminals after destroy");

    // Cleanup
    cleanup_all_loom_sessions();
}

/// Test 2.5: Destroy non-existent terminal
#[tokio::test]
#[serial]
async fn test_destroy_nonexistent_terminal() {
    setup();

    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let mut client = TestClient::connect(daemon.socket_path())
        .await
        .expect("Failed to connect");

    // Try to destroy non-existent terminal
    let fake_id = "00000000-0000-0000-0000-000000000000";
    let result = client.destroy_terminal(fake_id).await;

    // Should get error response
    assert!(result.is_err(), "Destroying non-existent terminal should fail");

    // Cleanup
    cleanup_all_loom_sessions();
}

/// Test 2.6: Send input to terminal
#[tokio::test]
#[serial]
async fn test_send_input() {
    setup();

    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let mut client = TestClient::connect(daemon.socket_path())
        .await
        .expect("Failed to connect");

    // Create terminal
    let terminal_id = client
        .create_terminal("test-terminal", None)
        .await
        .expect("Failed to create terminal");

    // Send some input (echo command)
    client
        .send_input(&terminal_id, "echo hello\r")
        .await
        .expect("Failed to send input");

    // Wait for command to execute
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    // Capture terminal output
    let session_name = format!("loom-{terminal_id}-default-0");
    let output = capture_terminal_output(&session_name).expect("Failed to capture terminal output");

    // Verify echoed output appears in terminal
    assert!(output.contains("hello"), "Expected 'hello' in output, got: {}", output);

    // Cleanup
    cleanup_all_loom_sessions();
}

/// Test 2.7: Multiple clients can connect
#[tokio::test]
#[serial]
async fn test_multiple_clients() {
    setup();

    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    // Connect 3 clients
    let mut client1 = TestClient::connect(daemon.socket_path())
        .await
        .expect("Failed to connect client 1");
    let mut client2 = TestClient::connect(daemon.socket_path())
        .await
        .expect("Failed to connect client 2");
    let mut client3 = TestClient::connect(daemon.socket_path())
        .await
        .expect("Failed to connect client 3");

    // Each client pings
    client1.ping().await.expect("Client 1 ping failed");
    client2.ping().await.expect("Client 2 ping failed");
    client3.ping().await.expect("Client 3 ping failed");

    // Each client creates a terminal
    let id1 = client1
        .create_terminal("terminal-1", None)
        .await
        .expect("Client 1 create failed");
    let id2 = client2
        .create_terminal("terminal-2", None)
        .await
        .expect("Client 2 create failed");
    let id3 = client3
        .create_terminal("terminal-3", None)
        .await
        .expect("Client 3 create failed");

    // All clients should see all 3 terminals
    let terminals = client1
        .list_terminals()
        .await
        .expect("Failed to list terminals");
    assert_eq!(terminals.len(), 3);

    // Verify IDs are unique
    assert_ne!(id1, id2);
    assert_ne!(id2, id3);
    assert_ne!(id1, id3);

    // Cleanup
    cleanup_all_loom_sessions();
}
