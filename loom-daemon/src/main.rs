mod ipc;
mod terminal;
mod types;

use anyhow::{anyhow, Result};
use ipc::IpcServer;
use std::fs;
use std::process::Command;
use std::sync::{Arc, Mutex};
use terminal::TerminalManager;

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();

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
