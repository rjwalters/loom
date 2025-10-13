use anyhow::{anyhow, Result};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::Duration;
use tokio::time::sleep;

use crate::daemon_client::{DaemonClient, Request, Response};

pub struct DaemonManager {
    daemon_process: Option<Child>,
    socket_path: PathBuf,
}

impl DaemonManager {
    pub fn new() -> Result<Self> {
        let socket_path = dirs::home_dir()
            .ok_or_else(|| anyhow!("No home directory"))?
            .join(".loom/daemon.sock");

        Ok(Self {
            daemon_process: None,
            socket_path,
        })
    }

    /// Check if daemon is already running by attempting to ping it
    pub async fn is_daemon_running(&self) -> bool {
        match DaemonClient::new() {
            Ok(client) => matches!(client.send_request(Request::Ping).await, Ok(Response::Pong)),
            Err(_) => false,
        }
    }

    /// Start daemon in production mode (as child process)
    pub fn start_daemon_production(&mut self, daemon_path: PathBuf) -> Result<()> {
        eprintln!("[DaemonManager] Starting daemon in production mode...");

        let child = Command::new(daemon_path)
            .spawn()
            .map_err(|e| anyhow!("Failed to spawn daemon: {e}"))?;

        self.daemon_process = Some(child);
        eprintln!(
            "[DaemonManager] Daemon process spawned with PID: {:?}",
            self.daemon_process.as_ref().map(std::process::Child::id)
        );

        Ok(())
    }

    /// Wait for daemon to be ready (socket exists and responds to ping)
    pub async fn wait_for_ready(&self, timeout_secs: u64) -> Result<()> {
        eprintln!("[DaemonManager] Waiting for daemon to be ready...");

        let start = std::time::Instant::now();
        let timeout = Duration::from_secs(timeout_secs);

        loop {
            if start.elapsed() > timeout {
                return Err(anyhow!("Daemon failed to start within {timeout_secs} seconds"));
            }

            // Check if socket exists
            if !self.socket_path.exists() {
                sleep(Duration::from_millis(100)).await;
                continue;
            }

            // Try to ping daemon
            if self.is_daemon_running().await {
                eprintln!("[DaemonManager] Daemon is ready!");
                return Ok(());
            }

            sleep(Duration::from_millis(100)).await;
        }
    }

    /// Connect to existing daemon (development mode)
    pub async fn connect_to_existing(&self) -> Result<()> {
        eprintln!("[DaemonManager] Connecting to existing daemon...");

        if !self.socket_path.exists() {
            return Err(anyhow!(
                "Daemon socket not found at {}. \
                 In development mode, please start the daemon manually:\n  \
                 RUST_LOG=info cargo run --manifest-path=loom-daemon/Cargo.toml",
                self.socket_path.display()
            ));
        }

        if !self.is_daemon_running().await {
            return Err(anyhow!(
                "Daemon socket exists but daemon is not responding. \
                 Please restart the daemon:\n  \
                 RUST_LOG=info cargo run --manifest-path=loom-daemon/Cargo.toml"
            ));
        }

        eprintln!("[DaemonManager] Successfully connected to existing daemon");
        Ok(())
    }

    /// Ensure daemon is running (start if needed in prod, connect in dev)
    pub async fn ensure_daemon_running(&mut self, is_production: bool) -> Result<()> {
        // If daemon is already running, we're good
        if self.is_daemon_running().await {
            eprintln!("[DaemonManager] Daemon already running");
            return Ok(());
        }

        if is_production {
            // Production: spawn daemon as child process
            let daemon_path = std::env::current_exe()?
                .parent()
                .ok_or_else(|| anyhow!("Failed to get parent directory"))?
                .join("loom-daemon");

            self.start_daemon_production(daemon_path)?;
            self.wait_for_ready(10).await?;
        } else {
            // Development: connect to existing daemon
            self.connect_to_existing().await?;
        }

        Ok(())
    }

    /// Kill daemon (only if we spawned it)
    pub fn kill_daemon(&mut self) {
        if let Some(mut child) = self.daemon_process.take() {
            eprintln!("[DaemonManager] Killing daemon process...");
            let _ = child.kill();
            let _ = child.wait();
            eprintln!("[DaemonManager] Daemon killed");
        }
    }
}

impl Drop for DaemonManager {
    fn drop(&mut self) {
        self.kill_daemon();
    }
}
