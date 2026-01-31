mod activity;
mod errors;
mod git_parser;
mod git_utils;
mod github_parser;
mod health_monitor;
mod init;
mod ipc;
mod metrics_collector;
mod role_validation;
mod terminal;
mod types;

use activity::{ActivityDb, StatsQueries};
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

    /// Display agent effectiveness and activity metrics
    Stats {
        /// Filter by agent role (builder, judge, curator, etc.)
        #[arg(long)]
        role: Option<String>,

        /// Filter by GitHub issue number
        #[arg(long)]
        issue: Option<i32>,

        /// Show weekly trends instead of daily
        #[arg(long)]
        weekly: bool,

        /// Output format: table (default), json
        #[arg(long, default_value = "table")]
        format: String,
    },

    /// Validate role configuration completeness
    Validate {
        /// Workspace directory containing .loom/config.json
        #[arg(value_name = "WORKSPACE", default_value = ".")]
        workspace: String,

        /// Output format: text (default), json
        #[arg(long, default_value = "text")]
        format: String,

        /// Fail with exit code 2 if warnings found (for CI)
        #[arg(long)]
        strict: bool,

        /// Show verbose output including configured roles
        #[arg(long, short)]
        verbose: bool,
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

    // Crash recovery: Release stale claims on startup (Issue #1159)
    // Claims older than 1 hour without heartbeat are considered stale
    let stale_threshold_secs = std::env::var("LOOM_CLAIM_TTL_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3600); // Default: 1 hour

    match activity_db.release_stale_claims(stale_threshold_secs) {
        Ok(count) if count > 0 => {
            log::warn!(
                "Crash recovery: Released {count} stale claims (older than {stale_threshold_secs}s)"
            );
        }
        Ok(_) => {
            log::debug!("No stale claims to release on startup");
        }
        Err(e) => {
            log::warn!("Failed to release stale claims on startup: {e}");
        }
    }

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

/// Handle CLI commands (init, stats, validate modes)
#[allow(clippy::too_many_lines)]
fn handle_cli_command(command: Commands) -> Result<()> {
    match command {
        Commands::Validate {
            workspace,
            format,
            strict,
            verbose,
        } => handle_validate_command(&workspace, &format, strict, verbose),
        Commands::Stats {
            role,
            issue,
            weekly,
            format,
        } => handle_stats_command(role.as_deref(), issue, weekly, &format),
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
                    // Handle self-installation (Loom source repo)
                    if report.is_self_install {
                        println!("\n✅ Loom source repository detected!");
                        println!("\nMode: Validation only (self-installation)");
                        println!("\nValidating configuration...");

                        if let Some(ref validation) = report.validation {
                            // Print validation results
                            println!(
                                "  ✓ .loom/roles/    - {} role definitions found",
                                validation.roles_found.len()
                            );
                            println!(
                                "  ✓ .loom/scripts/  - {} scripts found",
                                validation.scripts_found.len()
                            );
                            println!(
                                "  ✓ .claude/commands/ - {} slash commands found",
                                validation.commands_found.len()
                            );

                            if validation.has_claude_md {
                                println!("  ✓ CLAUDE.md       - Present");
                            } else {
                                println!("  ✗ CLAUDE.md       - Missing");
                            }

                            if validation.has_agents_md {
                                println!("  ✓ AGENTS.md       - Present");
                            } else {
                                println!("  ✗ AGENTS.md       - Missing");
                            }

                            if validation.has_labels_yml {
                                println!("  ✓ .github/labels.yml - Present");
                            } else {
                                println!("  ✗ .github/labels.yml - Missing");
                            }

                            // Print any issues found
                            if validation.issues.is_empty() {
                                println!("\n✅ Loom source repository is properly configured");
                            } else {
                                println!("\n⚠️  Issues found:");
                                for issue in &validation.issues {
                                    println!("  - {issue}");
                                }
                            }

                            // Print role details
                            println!("\nRoles found: {}", validation.roles_found.join(", "));
                        }

                        println!(
                            "\nℹ️  Self-installation skips file copying to prevent data loss."
                        );
                        println!("   The Loom repo's .loom/ directory IS the source of truth.");
                        println!("\nTo use Loom orchestration:");
                        println!("  - Open Claude Code terminals with /builder, /judge, etc.");
                        println!("  - Or launch the Loom Tauri app with this workspace");

                        return Ok(());
                    }

                    // Normal installation output
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
                            println!(
                                "\n  ℹ️  Preserved files were not overwritten. To update them,"
                            );
                            println!("     delete them and run install again, or use --force.");
                        }
                        if !report.updated.is_empty() {
                            println!("\nFiles updated ({}):", report.updated.len());
                            for file in &report.updated {
                                println!("  ~ {file}");
                            }
                        }
                        if !report.verification_failures.is_empty() {
                            eprintln!(
                                "\n⚠️  Verification failures ({}):",
                                report.verification_failures.len()
                            );
                            for failure in &report.verification_failures {
                                eprintln!("  ✗ {failure}");
                            }
                            eprintln!("\n  Some scripts may not have been updated correctly.");
                            eprintln!(
                                "  Try running install again with --force, or copy manually from defaults/."
                            );
                        }
                    }

                    println!("\nNext steps:");
                    println!("  1. Commit the changes: git add -A && git commit -m 'Add Loom configuration'");
                    println!("  2. Choose your workflow:");
                    println!("     Manual Mode (recommended to start):");
                    println!("       cd {workspace_str} && claude");
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

/// Handle the stats subcommand - display agent effectiveness and activity metrics.
#[allow(clippy::too_many_lines)]
fn handle_stats_command(
    role: Option<&str>,
    issue: Option<i32>,
    weekly: bool,
    format: &str,
) -> Result<()> {
    // Get database path
    let loom_dir = dirs::home_dir()
        .ok_or_else(|| anyhow!("No home directory"))?
        .join(".loom");

    let db_path = loom_dir.join("activity.db");

    if !db_path.exists() {
        eprintln!("No activity database found at {}", db_path.display());
        eprintln!("Run the Loom daemon first to start collecting metrics.");
        return Ok(());
    }

    let db = ActivityDb::new(db_path)?;

    let is_json = format == "json";

    // If issue filter is specified, show cost per issue
    if let Some(issue_num) = issue {
        let costs = db.get_cost_per_issue(Some(issue_num))?;

        if is_json {
            println!("{}", serde_json::to_string_pretty(&costs)?);
        } else {
            println!("\n=== Cost Breakdown for Issue #{issue_num} ===\n");
            if costs.is_empty() {
                println!("No data found for issue #{issue_num}");
            } else {
                for cost in &costs {
                    println!("Issue #{}:", cost.issue_number);
                    println!("  Prompts:      {}", cost.prompt_count);
                    println!("  Total Cost:   ${:.4}", cost.total_cost);
                    println!("  Total Tokens: {}", cost.total_tokens);
                    if let Some(started) = &cost.started {
                        println!("  Started:      {}", started.format("%Y-%m-%d %H:%M"));
                    }
                    if let Some(completed) = &cost.completed {
                        println!("  Completed:    {}", completed.format("%Y-%m-%d %H:%M"));
                    }
                    println!();
                }
            }
        }
        return Ok(());
    }

    // If role filter is specified, show effectiveness for that role
    if let Some(role_filter) = role {
        let effectiveness = db.get_agent_effectiveness(Some(role_filter))?;

        if is_json {
            println!("{}", serde_json::to_string_pretty(&effectiveness)?);
        } else {
            println!("\n=== Agent Effectiveness: {role_filter} ===\n");
            if effectiveness.is_empty() {
                println!("No data found for role '{role_filter}'");
            } else {
                for agent in &effectiveness {
                    print_agent_effectiveness(agent);
                }
            }
        }
        return Ok(());
    }

    // If weekly flag, show weekly velocity
    if weekly {
        let velocity = db.get_weekly_velocity()?;

        if is_json {
            println!("{}", serde_json::to_string_pretty(&velocity)?);
        } else {
            println!("\n=== Weekly Velocity ===\n");
            if velocity.is_empty() {
                println!("No weekly data available.");
            } else {
                println!("{:<12} {:>10} {:>12}", "Week", "Prompts", "Cost (USD)");
                println!("{:-<36}", "");
                for week in &velocity {
                    println!("{:<12} {:>10} {:>12.4}", week.week, week.prompts, week.cost);
                }
            }
        }
        return Ok(());
    }

    // Default: show overall summary
    let summary = db.get_stats_summary()?;
    let effectiveness = db.get_agent_effectiveness(None)?;

    if is_json {
        #[derive(serde::Serialize)]
        struct FullStats {
            summary: activity::StatsSummary,
            effectiveness: Vec<activity::AgentEffectiveness>,
        }
        let full = FullStats {
            summary,
            effectiveness,
        };
        println!("{}", serde_json::to_string_pretty(&full)?);
    } else {
        println!("\n=== Loom Activity Summary ===\n");
        println!("Total Prompts:   {}", summary.total_prompts);
        println!("Total Cost:      ${:.4}", summary.total_cost);
        println!("Total Tokens:    {}", summary.total_tokens);
        println!("Issues Worked:   {}", summary.issues_count);
        println!("PRs Created:     {}", summary.prs_count);
        println!("Avg Success:     {:.1}%", summary.avg_success_rate);

        if !effectiveness.is_empty() {
            println!("\n=== Agent Effectiveness by Role ===\n");
            println!(
                "{:<12} {:>10} {:>10} {:>12} {:>12} {:>12}",
                "Role", "Prompts", "Success", "Rate", "Avg Cost", "Avg Time"
            );
            println!("{:-<70}", "");
            for agent in &effectiveness {
                println!(
                    "{:<12} {:>10} {:>10} {:>11.1}% {:>11.4} {:>10.1}s",
                    agent.agent_role,
                    agent.total_prompts,
                    agent.successful_prompts,
                    agent.success_rate,
                    agent.avg_cost,
                    agent.avg_duration_sec
                );
            }
        }

        // Show top 5 most expensive issues
        let top_issues = db.get_cost_per_issue(None)?;
        if !top_issues.is_empty() {
            println!("\n=== Top 5 Most Expensive Issues ===\n");
            println!("{:<8} {:>10} {:>12} {:>12}", "Issue", "Prompts", "Cost (USD)", "Tokens");
            println!("{:-<44}", "");
            for cost in top_issues.iter().take(5) {
                println!(
                    "#{:<7} {:>10} {:>12.4} {:>12}",
                    cost.issue_number, cost.prompt_count, cost.total_cost, cost.total_tokens
                );
            }
        }

        println!();
    }

    Ok(())
}

/// Print agent effectiveness in a readable format.
fn print_agent_effectiveness(agent: &activity::AgentEffectiveness) {
    println!("Role: {}", agent.agent_role);
    println!("  Total Prompts:      {}", agent.total_prompts);
    println!("  Successful Prompts: {}", agent.successful_prompts);
    println!("  Success Rate:       {:.1}%", agent.success_rate);
    println!("  Average Cost:       ${:.4}", agent.avg_cost);
    println!("  Average Duration:   {:.1}s", agent.avg_duration_sec);
    println!();
}

/// Handle the validate subcommand - check role configuration completeness.
fn handle_validate_command(
    workspace: &str,
    format: &str,
    strict: bool,
    verbose: bool,
) -> Result<()> {
    use role_validation::{format_validation_result, validate_from_file, ValidationMode};

    // Convert workspace path to absolute path
    let workspace_path = std::path::Path::new(workspace);
    let absolute_workspace = if workspace_path.is_absolute() {
        workspace_path.to_path_buf()
    } else {
        std::env::current_dir()?.join(workspace_path)
    };

    let config_path = absolute_workspace.join(".loom").join("config.json");

    if !config_path.exists() {
        if format == "json" {
            println!(r#"{{"error": "Config file not found: {}"}}"#, config_path.display());
        } else {
            eprintln!("Error: Config file not found: {}", config_path.display());
            eprintln!("\nMake sure you're in a Loom workspace or specify the path:");
            eprintln!("  loom-daemon validate /path/to/workspace");
        }
        std::process::exit(1);
    }

    let mode = if strict {
        ValidationMode::Strict
    } else {
        ValidationMode::Warn
    };

    let result = validate_from_file(&config_path, mode).map_err(|e| anyhow!("{e}"))?;

    if format == "json" {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        // Print header
        if verbose {
            println!("\nValidating role configuration...");
            println!("  Config: {}", config_path.display());
            println!();
        }

        // Print formatted result
        let output = format_validation_result(&result, verbose);
        if !output.is_empty() {
            print!("{output}");
        }

        // Print success message if no warnings
        if result.warnings.is_empty() && result.errors.is_empty() {
            println!("All role dependencies are satisfied.");
        }
    }

    // Determine exit code
    if !result.errors.is_empty() {
        std::process::exit(1);
    } else if !result.warnings.is_empty() && strict {
        std::process::exit(2);
    }

    Ok(())
}
