mod activity;
mod health_monitor;
mod ipc;
mod logging;
mod terminal;
mod types;

use activity::ActivityDb;
use anyhow::{anyhow, Result};
use ipc::IpcServer;
use std::fs;
use std::io::Write;
use std::process::Command;
use std::sync::{Arc, Mutex};
use terminal::TerminalManager;

#[tokio::main]
async fn main() -> Result<()> {
    // Setup logging to ~/.loom/daemon.log
    setup_logging()?;

    // Check tmux
    check_tmux_installed()?;

    // Setup loom directory and socket path
    // For testing, allow override via LOOM_SOCKET_PATH env var
    let (loom_dir, socket_path) = if let Ok(path) = std::env::var("LOOM_SOCKET_PATH") {
        // For testing, use the parent directory of the provided socket path
        let socket_path = std::path::PathBuf::from(path);
        let loom_dir = socket_path
            .parent()
            .ok_or_else(|| anyhow!("Socket path has no parent directory"))?
            .to_path_buf();
        (loom_dir, socket_path)
    } else {
        let loom_dir = dirs::home_dir()
            .ok_or_else(|| anyhow!("No home directory"))?
            .join(".loom");
        fs::create_dir_all(&loom_dir)?;
        let socket_path = loom_dir.join("loom-daemon.sock");
        (loom_dir, socket_path)
    };

    // Initialize activity database
    let db_path = loom_dir.join("activity.db");
    let activity_db = ActivityDb::new(db_path)?;
    log::info!("Activity database initialized");

    let activity_db = Arc::new(Mutex::new(activity_db));

    // Initialize terminal manager
    let mut tm = TerminalManager::new();
    tm.restore_from_tmux()?;
    log::info!("Restored {} terminals", tm.list_terminals().len());

    let tm = Arc::new(Mutex::new(tm));

    // Start optional health monitoring if enabled via environment variable
    if let Some(interval) = health_monitor::check_env_enabled() {
        health_monitor::start_tmux_health_monitor(interval);
        log::info!("✅ tmux health monitoring enabled (interval: {}s)", interval);
    } else {
        log::debug!("tmux health monitoring disabled (set LOOM_TMUX_HEALTH_MONITOR to enable)");
    }

    // Start IPC server
    let server = IpcServer::new(socket_path.clone(), tm, activity_db);

    // Setup signal handler for graceful shutdown
    let socket_path_clone = socket_path.clone();
    tokio::spawn(async move {
        match tokio::signal::ctrl_c().await {
            Ok(()) => {
                log::info!("Received shutdown signal, cleaning up...");
                let _ = tokio::fs::remove_file(&socket_path_clone).await;
                log::info!("Socket cleaned up, exiting");
                std::process::exit(0);
            }
            Err(err) => {
                log::error!("Unable to listen for shutdown signal: {err}");
            }
        }
    });

    log::info!("Loom daemon starting...");
    server.run().await?;

    Ok(())
}

fn check_tmux_installed() -> Result<()> {
    Command::new("which")
        .arg("tmux")
        .output()?
        .status
        .success()
        .then_some(())
        .ok_or_else(|| anyhow!("tmux not installed. Install with: brew install tmux"))
}

fn setup_logging() -> Result<()> {
    // Get log file path: ~/.loom/daemon.log
    let log_path = dirs::home_dir()
        .ok_or_else(|| anyhow!("No home directory"))?
        .join(".loom/daemon.log");

    // Create .loom directory if it doesn't exist
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Open log file in append mode
    let log_file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;

    // Configure env_logger to write to file
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .target(env_logger::Target::Pipe(Box::new(log_file)))
        .format(|buf, record| {
            writeln!(
                buf,
                "[{}] [{}] {}",
                chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.3f"),
                record.level(),
                record.args()
            )
        })
        .init();

    log::info!("Daemon logging initialized to {}", log_path.display());

    Ok(())
}
