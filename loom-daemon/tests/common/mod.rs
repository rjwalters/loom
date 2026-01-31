// Test infrastructure - expect/unwrap are acceptable here since tests should panic on failure
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::time::timeout;

/// Test daemon instance that cleans up on drop
pub struct TestDaemon {
    _temp_dir: TempDir,
    socket_path: PathBuf,
    process: Option<Child>,
}

impl TestDaemon {
    /// Start a new daemon instance with a unique socket path
    pub async fn start() -> Result<Self> {
        let temp_dir = TempDir::new().context("Failed to create temp directory")?;
        let socket_path = temp_dir.path().join("daemon.sock");

        // Build the daemon binary (in case it's not already built)
        let build_output = Command::new("cargo")
            .args(["build", "--bin", "loom-daemon"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .context("Failed to build daemon")?;

        if !build_output.status.success() {
            anyhow::bail!(
                "Failed to build daemon: {}",
                String::from_utf8_lossy(&build_output.stderr)
            );
        }

        // Start the daemon process
        let daemon_bin =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/debug/loom-daemon");

        let mut process = Command::new(&daemon_bin)
            .env("LOOM_SOCKET_PATH", &socket_path)
            .env("RUST_LOG", "debug")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("Failed to spawn daemon")?;

        // Wait for socket to be created (with timeout)
        let start = std::time::Instant::now();
        while !socket_path.exists() {
            if start.elapsed() > Duration::from_secs(5) {
                // Kill the process and get logs
                let _ = process.kill();
                let output = process.wait_with_output()?;
                anyhow::bail!(
                    "Daemon failed to create socket within 5s.\nStderr: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        Ok(Self {
            _temp_dir: temp_dir,
            socket_path,
            process: Some(process),
        })
    }

    /// Get the socket path for connecting clients
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        if let Some(mut process) = self.process.take() {
            // Try graceful shutdown first
            let _ = process.kill();
            let _ = process.wait();
        }
        // temp_dir cleanup handled by TempDir's Drop
    }
}

/// Test client for communicating with daemon
pub struct TestClient {
    reader: BufReader<tokio::io::ReadHalf<UnixStream>>,
    writer: tokio::io::WriteHalf<UnixStream>,
}

impl TestClient {
    /// Connect to daemon at given socket path with retry logic
    ///
    /// The daemon creates the socket file before it starts listening, creating a race condition.
    /// This method retries with exponential backoff to handle this race.
    pub async fn connect(socket_path: &Path) -> Result<Self> {
        let max_retries = 5;
        let mut retry_delay = Duration::from_millis(50);

        for attempt in 0..max_retries {
            match timeout(Duration::from_secs(2), UnixStream::connect(socket_path)).await {
                Ok(Ok(stream)) => {
                    let (reader, writer) = tokio::io::split(stream);
                    let reader = BufReader::new(reader);
                    return Ok(Self { reader, writer });
                }
                Ok(Err(_e)) if attempt < max_retries - 1 => {
                    // Connection failed, retry with backoff
                    tokio::time::sleep(retry_delay).await;
                    retry_delay *= 2; // Exponential backoff
                }
                Ok(Err(e)) => {
                    // Final attempt failed
                    return Err(e).context("Failed to connect to daemon");
                }
                Err(_) => {
                    return Err(anyhow::anyhow!("Timeout connecting to daemon"));
                }
            }
        }

        unreachable!("Loop should always return before reaching here")
    }

    /// Send a request and receive a response
    pub async fn send_request(&mut self, request: serde_json::Value) -> Result<serde_json::Value> {
        // Serialize and send request
        let request_json = serde_json::to_string(&request)?;
        self.writer
            .write_all(request_json.as_bytes())
            .await
            .context("Failed to write request")?;
        self.writer
            .write_all(b"\n")
            .await
            .context("Failed to write newline")?;
        self.writer.flush().await.context("Failed to flush")?;

        // Read response
        let mut response_line = String::new();
        timeout(Duration::from_secs(2), self.reader.read_line(&mut response_line))
            .await
            .context("Timeout reading response")?
            .context("Failed to read response")?;

        // Parse response
        serde_json::from_str(&response_line).context("Failed to parse response JSON")
    }

    /// Helper: Send Ping request
    #[allow(dead_code)]
    pub async fn ping(&mut self) -> Result<()> {
        let request = serde_json::json!({"type": "Ping"});
        let response = self.send_request(request).await?;

        if response != serde_json::json!({"type": "Pong"}) {
            anyhow::bail!("Expected Pong, got: {response:?}");
        }

        Ok(())
    }

    /// Helper: Create terminal
    ///
    /// For security tests, the first parameter (id) is used as the `config_id`.
    /// For non-security tests that need unique IDs, use `create_terminal_with_unique_id` instead.
    #[allow(dead_code)]
    pub async fn create_terminal(
        &mut self,
        id: impl Into<String>,
        working_dir: Option<String>,
    ) -> Result<String> {
        let id_str: String = id.into();
        // Use the provided ID as both config_id and name for security testing
        self.create_terminal_with_config(&id_str, &id_str, working_dir, None, None)
            .await
    }

    /// Helper: Create terminal with auto-generated unique ID
    ///
    /// Use this for non-security tests that need valid, unique terminal IDs.
    #[allow(dead_code)]
    pub async fn create_terminal_with_unique_id(
        &mut self,
        name: impl Into<String>,
        working_dir: Option<String>,
    ) -> Result<String> {
        // Generate a unique config_id for this test terminal
        let config_id = format!("test-{}", uuid::Uuid::new_v4());
        self.create_terminal_with_config(config_id, name, working_dir, None, None)
            .await
    }

    /// Helper: Create terminal with explicit configuration parameters
    #[allow(dead_code)]
    pub async fn create_terminal_with_config(
        &mut self,
        config_id: impl Into<String>,
        name: impl Into<String>,
        working_dir: Option<String>,
        role: Option<String>,
        instance_number: Option<u32>,
    ) -> Result<String> {
        let config_id: String = config_id.into();
        let name: String = name.into();

        let request = serde_json::json!({
            "type": "CreateTerminal",
            "payload": {
                "config_id": config_id,
                "name": name,
                "working_dir": working_dir,
                "role": role,
                "instance_number": instance_number
            }
        });

        let response = self.send_request(request).await?;

        if let Some(payload) = response.get("payload") {
            if let Some(id) = payload.get("id") {
                return Ok(id.as_str().unwrap().to_string());
            }
        }
        anyhow::bail!("Unexpected response: {response:?}");
    }

    /// Helper: List terminals
    #[allow(dead_code)]
    pub async fn list_terminals(&mut self) -> Result<Vec<serde_json::Value>> {
        let request = serde_json::json!({"type": "ListTerminals"});
        let response = self.send_request(request).await?;

        if let Some(payload) = response.get("payload") {
            if let Some(terminals) = payload.get("terminals") {
                return Ok(terminals.as_array().unwrap().clone());
            }
        }
        anyhow::bail!("Unexpected response: {response:?}");
    }

    /// Helper: Destroy terminal
    #[allow(dead_code)]
    pub async fn destroy_terminal(&mut self, id: &str) -> Result<()> {
        let request = serde_json::json!({
            "type": "DestroyTerminal",
            "payload": { "id": id }
        });

        let response = self.send_request(request).await?;

        if response != serde_json::json!({"type": "Success"}) {
            anyhow::bail!("Expected Success, got: {response:?}");
        }

        Ok(())
    }

    /// Helper: Send input to terminal
    /// Returns the `input_id` for tracking git changes
    #[allow(dead_code)]
    pub async fn send_input(&mut self, id: &str, data: &str) -> Result<i64> {
        let request = serde_json::json!({
            "type": "SendInput",
            "payload": { "id": id, "data": data }
        });

        let response = self.send_request(request).await?;

        // Accept both Success (legacy) and InputSent (new) responses
        if response.get("type") == Some(&serde_json::json!("Success")) {
            return Ok(0);
        }

        if response.get("type") == Some(&serde_json::json!("InputSent")) {
            if let Some(payload) = response.get("payload") {
                if let Some(input_id) = payload.get("input_id") {
                    return Ok(input_id.as_i64().unwrap_or(0));
                }
            }
        }

        anyhow::bail!("Expected Success or InputSent, got: {response:?}");
    }

    /// Helper: Check session health for a terminal ID
    #[allow(dead_code)]
    pub async fn check_session_health(&mut self, id: &str) -> Result<bool> {
        let request = serde_json::json!({
            "type": "CheckSessionHealth",
            "payload": { "id": id }
        });

        let response = self.send_request(request).await?;

        if let Some(payload) = response.get("payload") {
            if let Some(has_session) = payload.get("has_session") {
                return Ok(has_session.as_bool().unwrap_or(false));
            }
        }

        anyhow::bail!("Unexpected response: {response:?}");
    }
}

/// Helper: Check if a tmux session exists
#[allow(dead_code)]
pub fn tmux_session_exists(session_name: &str) -> bool {
    Command::new("tmux")
        .args(["-L", "loom", "has-session", "-t", session_name])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Helper: Kill a tmux session (for cleanup)
pub fn kill_tmux_session(session_name: &str) {
    let _ = Command::new("tmux")
        .args(["-L", "loom", "kill-session", "-t", session_name])
        .output();
}

/// Helper: Get list of all loom-* tmux sessions
pub fn get_loom_tmux_sessions() -> Vec<String> {
    let output = Command::new("tmux")
        .args(["-L", "loom", "list-sessions", "-F", "#{session_name}"])
        .output();

    match output {
        Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| line.starts_with("loom-"))
            .map(std::string::ToString::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

/// Helper: Clean up all loom-* tmux sessions (for test teardown)
pub fn cleanup_all_loom_sessions() {
    for session in get_loom_tmux_sessions() {
        kill_tmux_session(&session);
    }
}

/// Helper: Check if the tmux server is running
#[allow(dead_code)]
pub fn tmux_server_running() -> bool {
    Command::new("tmux")
        .args(["-L", "loom", "list-sessions"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Helper: Capture terminal output using tmux capture-pane
///
/// Uses tmux's built-in capture mechanism to read the terminal's pane content.
/// Returns the captured output as a String.
///
/// This function includes retry logic to handle transient tmux server state
/// issues that can occur during test setup/teardown.
///
/// # Arguments
/// * `session_name` - The tmux session name to capture from
///
/// # Returns
/// * `Result<String>` - The captured output or an error message
#[allow(dead_code)]
pub fn capture_terminal_output(session_name: &str) -> Result<String> {
    const MAX_RETRIES: u32 = 3;
    const RETRY_DELAY_MS: u64 = 100;

    let mut last_error = String::new();

    for attempt in 0..MAX_RETRIES {
        // First verify the session exists
        if !tmux_session_exists(session_name) {
            last_error = format!("tmux session '{session_name}' does not exist");
            if attempt < MAX_RETRIES - 1 {
                std::thread::sleep(std::time::Duration::from_millis(RETRY_DELAY_MS));
                continue;
            }
            anyhow::bail!("{last_error}");
        }

        let output = Command::new("tmux")
            .args(["-L", "loom", "capture-pane", "-t", session_name, "-p"])
            .output()
            .context("Failed to execute tmux capture-pane")?;

        if output.status.success() {
            return String::from_utf8(output.stdout).context("Invalid UTF-8 in captured output");
        }

        last_error = String::from_utf8_lossy(&output.stderr).to_string();

        // If this is not the last attempt, wait and retry
        if attempt < MAX_RETRIES - 1 {
            std::thread::sleep(std::time::Duration::from_millis(RETRY_DELAY_MS));
        }
    }

    anyhow::bail!("tmux capture-pane failed after {MAX_RETRIES} attempts: {last_error}")
}
