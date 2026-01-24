mod activity;
mod git_parser;
mod git_utils;
mod github_parser;
mod health_monitor;
mod init;
mod ipc;
mod metrics_collector;
mod terminal;
mod types;

use activity::ActivityDb;
use anyhow::{anyhow, Result};
use clap::{Parser, Subcommand};
use ipc::IpcServer;
use std::fs;
use std::io::Write;
use std::process::Command;
use std::sync::{Arc, Mutex};
use terminal::TerminalManager;

/// Loom daemon - terminal multiplexing and workspace orchestration
#[derive(Parser)]
#[command(name = "loom-daemon")]
#[command(about = "Loom daemon for AI-powered development orchestration", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize a Loom workspace in a target repository
    Init {
        /// Target workspace directory (must be a git repository)
        #[arg(value_name = "PATH", default_value = ".")]
        workspace: String,

        /// Path to defaults directory
        #[arg(long, default_value = "defaults")]
        defaults: String,

        /// Overwrite existing .loom directory if it exists
        #[arg(long)]
        force: bool,

        /// Print what would be done without making changes
        #[arg(long)]
        dry_run: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Handle CLI commands (init mode)
    if let Some(command) = cli.command {
        return handle_cli_command(command);
    }

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
    let activity_db = ActivityDb::new(db_path.clone())?;
    log::info!("Activity database initialized");

    let activity_db = Arc::new(Mutex::new(activity_db));

    // Initialize terminal manager
    let mut tm = TerminalManager::new();
    tm.restore_from_tmux()?;
    log::info!("Restored {} terminals", tm.list_terminals().len());

    let tm = Arc::new(Mutex::new(tm));

    // Start health monitoring (enabled by default)
    if let Some(interval) = health_monitor::check_env_enabled() {
        let (_health_handle, _health_state) = health_monitor::start_tmux_health_monitor(interval);
        log::info!("✅ tmux health monitoring enabled (interval: {interval}s)");
        // Note: health_handle is dropped here, but the thread keeps running
        // health_state could be stored for querying crash status if needed
    }

    // Start GitHub metrics collection (if workspace is set)
    // Workspace can be set via LOOM_WORKSPACE environment variable
    let workspace_from_env = std::env::var("LOOM_WORKSPACE").ok();
    let db_path_str = db_path.to_str().map(std::string::ToString::to_string);

    if let (Some(workspace), Some(db_path_string)) = (workspace_from_env.as_deref(), db_path_str) {
        let _metrics_handle =
            metrics_collector::try_init_metrics_collector(Some(workspace), &db_path_string);
        // Note: metrics_handle is dropped here, but the thread keeps running if enabled
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

/// Rotate log file if it exceeds max size
/// Keeps last 10 files (log.1, log.2, ..., log.10)
fn rotate_log_file(log_path: &std::path::Path, max_size: u64, max_files: usize) -> Result<()> {
    // Check if rotation is needed
    if !log_path.exists() {
        return Ok(());
    }

    let metadata = fs::metadata(log_path)?;
    if metadata.len() < max_size {
        return Ok(()); // No rotation needed
    }

    // Remove oldest rotated file if it exists (log.10)
    let oldest_file = format!("{}.{max_files}", log_path.display());
    let _ = fs::remove_file(&oldest_file); // Ignore error if file doesn't exist

    // Shift existing rotated files (log.9 -> log.10, log.8 -> log.9, etc.)
    for i in (1..max_files).rev() {
        let old_path = format!("{}.{i}", log_path.display());
        let new_path = format!("{}.{}", log_path.display(), i + 1);
        if std::path::Path::new(&old_path).exists() {
            let _ = fs::rename(&old_path, &new_path); // Ignore errors
        }
    }

    // Rotate current log file to log.1
    let rotated_path = format!("{}.1", log_path.display());
    fs::rename(log_path, rotated_path)?;

    Ok(())
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

    // Rotate log file if it exceeds 10MB (keeps last 10 files)
    rotate_log_file(&log_path, 10 * 1024 * 1024, 10)?;

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

/// Handle CLI commands (init mode)
fn handle_cli_command(command: Commands) -> Result<()> {
    match command {
        Commands::Init {
            workspace,
            defaults,
            force,
            dry_run,
        } => {
            // Convert workspace path to absolute path
            let workspace_path = std::path::Path::new(&workspace);
            let absolute_workspace = if workspace_path.is_absolute() {
                workspace_path.to_path_buf()
            } else {
                std::env::current_dir()?.join(workspace_path)
            };

            let workspace_str = absolute_workspace
                .to_str()
                .ok_or_else(|| anyhow!("Invalid workspace path"))?;

            if dry_run {
                println!("Dry run mode - no changes will be made\n");
                println!("Would initialize Loom workspace:");
                println!("  Workspace: {workspace_str}");
                println!("  Defaults:  {defaults}");
                println!("  Force:     {force}");
                println!("\nActions that would be performed:");
                println!("  1. Validate {workspace_str} is a git repository");
                println!("  2. Copy .loom/ configuration from {defaults}");
                println!("  3. Setup repository scaffolding (CLAUDE.md, AGENTS.md, .claude/, .codex/, .github/)");
                println!("  4. Update .gitignore with Loom ephemeral patterns");
                return Ok(());
            }

            println!("Initializing Loom workspace...");
            println!("  Workspace: {workspace_str}");
            println!("  Defaults:  {defaults}");

            match init::initialize_workspace(workspace_str, &defaults, force) {
                Ok(report) => {
                    println!("\n✅ Loom workspace initialized successfully!");
                    println!("\nFiles installed:");
                    println!("  📁 .loom/          - Configuration directory");
                    println!("  📄 .loom/config.json - Terminal configuration");
                    println!("  📁 .loom/roles/    - Agent role definitions");
                    println!("  📄 CLAUDE.md       - AI context documentation");
                    println!("  📄 AGENTS.md       - Agent workflow guide");
                    println!("  📁 .claude/        - Claude Code configuration");
                    println!("  📁 .codex/         - Codex configuration");
                    println!("  📁 .github/        - GitHub workflow templates");
                    println!("  📄 .gitignore      - Updated with Loom patterns");

                    // Print report of what was added vs preserved
                    if !report.added.is_empty() || !report.preserved.is_empty() {
                        println!();
                        if !report.added.is_empty() {
                            println!("Files added ({}):", report.added.len());
                            for file in &report.added {
                                println!("  + {file}");
                            }
                        }
                        if !report.preserved.is_empty() {
                            println!("\nFiles preserved ({}):", report.preserved.len());
                            for file in &report.preserved {
                                println!("  = {file}");
                            }
                            println!("\n  ℹ️  Preserved files were not overwritten. To update them,");
                            println!("     delete them and run install again, or use --force.");
                        }
                        if !report.updated.is_empty() && force {
                            println!("\nFiles updated ({}):", report.updated.len());
                            for file in &report.updated {
                                println!("  ~ {file}");
                            }
                        }
                    }

                    println!("\nNext steps:");
                    println!("  1. Commit the changes: git add -A && git commit -m 'Add Loom configuration'");
                    println!("  2. Choose your workflow:");
                    println!("     Manual Mode (recommended to start):");
                    println!("       cd {} && claude", workspace_str);
                    println!("       Then use /builder, /judge, or other role commands");
                    println!("     Tauri App Mode (requires Loom.app - see README):");
                    println!("       Download Loom.app from releases, open workspace");
                    Ok(())
                }
                Err(e) => {
                    eprintln!("\n❌ Failed to initialize workspace: {e}");
                    std::process::exit(1);
                }
            }
        }
    }
}
