mod ipc;
mod terminal;
mod types;

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
    let socket_path = if let Ok(path) = std::env::var("LOOM_SOCKET_PATH") {
        std::path::PathBuf::from(path)
    } else {
        let loom_dir = dirs::home_dir()
            .ok_or_else(|| anyhow!("No home directory"))?
            .join(".loom");
        fs::create_dir_all(&loom_dir)?;
        loom_dir.join("daemon.sock")
    };

    // Initialize terminal manager
    let mut tm = TerminalManager::new();
    tm.restore_from_tmux()?;
    log::info!("Restored {} terminals", tm.list_terminals().len());

    let tm = Arc::new(Mutex::new(tm));

    // Start IPC server
    let server = IpcServer::new(socket_path, tm);

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
