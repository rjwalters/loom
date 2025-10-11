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
    /// Connect to daemon at given socket path
    pub async fn connect(socket_path: &Path) -> Result<Self> {
        let stream = timeout(Duration::from_secs(2), UnixStream::connect(socket_path))
            .await
            .context("Timeout connecting to daemon")?
            .context("Failed to connect to daemon")?;

        let (reader, writer) = tokio::io::split(stream);
        let reader = BufReader::new(reader);

        Ok(Self { reader, writer })
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
    pub async fn ping(&mut self) -> Result<()> {
        let request = serde_json::json!({"Ping": null});
        let response = self.send_request(request).await?;

        if response != serde_json::json!("Pong") {
            anyhow::bail!("Expected Pong, got: {response:?}");
        }

        Ok(())
    }

    /// Helper: Create terminal
    pub async fn create_terminal(
        &mut self,
        name: impl Into<String>,
        working_dir: Option<String>,
    ) -> Result<String> {
        let request = serde_json::json!({
            "CreateTerminal": {
                "name": name.into(),
                "working_dir": working_dir
            }
        });

        let response = self.send_request(request).await?;

        if let Some(id) = response.get("TerminalCreated").and_then(|v| v.get("id")) {
            Ok(id.as_str().unwrap().to_string())
        } else {
            anyhow::bail!("Unexpected response: {response:?}");
        }
    }

    /// Helper: List terminals
    pub async fn list_terminals(&mut self) -> Result<Vec<serde_json::Value>> {
        let request = serde_json::json!({"ListTerminals": null});
        let response = self.send_request(request).await?;

        if let Some(terminals) = response
            .get("TerminalList")
            .and_then(|v| v.get("terminals"))
        {
            Ok(terminals.as_array().unwrap().clone())
        } else {
            anyhow::bail!("Unexpected response: {response:?}");
        }
    }

    /// Helper: Destroy terminal
    pub async fn destroy_terminal(&mut self, id: &str) -> Result<()> {
        let request = serde_json::json!({
            "DestroyTerminal": { "id": id }
        });

        let response = self.send_request(request).await?;

        if response != serde_json::json!("Success") {
            anyhow::bail!("Expected Success, got: {response:?}");
        }

        Ok(())
    }

    /// Helper: Send input to terminal
    pub async fn send_input(&mut self, id: &str, data: &str) -> Result<()> {
        let request = serde_json::json!({
            "SendInput": { "id": id, "data": data }
        });

        let response = self.send_request(request).await?;

        if response != serde_json::json!("Success") {
            anyhow::bail!("Expected Success, got: {response:?}");
        }

        Ok(())
    }
}

/// Helper: Check if a tmux session exists
pub fn tmux_session_exists(session_name: &str) -> bool {
    Command::new("tmux")
        .args(["has-session", "-t", session_name])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Helper: Kill a tmux session (for cleanup)
pub fn kill_tmux_session(session_name: &str) {
    let _ = Command::new("tmux")
        .args(["kill-session", "-t", session_name])
        .output();
}

/// Helper: Get list of all loom-* tmux sessions
pub fn get_loom_tmux_sessions() -> Vec<String> {
    let output = Command::new("tmux")
        .args(["list-sessions", "-F", "#{session_name}"])
        .output();

    match output {
        Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| line.starts_with("loom-"))
            .map(|s| s.to_string())
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
