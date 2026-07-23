use loom_daemon::activity::{self, ActivityDb, StatsQueries};
use loom_daemon::epic_supervisor::{self, EpicSupervisor};
use loom_daemon::event_bus::EventBus;
use loom_daemon::health_monitor;
use loom_daemon::ipc::IpcServer;
use loom_daemon::issue_creation_mutex::IssueCreationMutex;
use loom_daemon::main_health_gate;
use loom_daemon::metrics_collector;
use loom_daemon::role_validation;
use loom_daemon::sweep_registry::{self, SweepRegistry, SweepRegistryConfig};
use loom_daemon::terminal::TerminalManager;
use loom_daemon::work_finder;
use loom_daemon::{extract_configured_terminal_ids, rotate_log_file};

use anyhow::{anyhow, Result};
use clap::{Parser, Subcommand};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

/// Loom daemon - terminal multiplexing and workspace orchestration
#[derive(Parser)]
#[command(name = "loom-daemon")]
#[command(about = "Loom daemon for AI-powered development orchestration", long_about = None)]
// Embed git commit + build timestamp alongside the crate version so
// `--version` distinguishes rebuilds of the same release. Motivated by
// issue #3470: stale daemon binaries are otherwise indistinguishable from
// fresh ones and cause hard-to-diagnose install regressions (#3287 class).
// `LOOM_DAEMON_GIT_COMMIT` and `LOOM_DAEMON_BUILD_TIME` are populated by
// `build.rs`; both fall back to "unknown" when the build host lacks the
// tooling, which is loud but harmless.
#[command(version = concat!(
    env!("CARGO_PKG_VERSION"),
    " (commit ",
    env!("LOOM_DAEMON_GIT_COMMIT"),
    ", built ",
    env!("LOOM_DAEMON_BUILD_TIME"),
    ")"
))]
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

    // Load configured terminal IDs for config-based session filtering (Issue #1952)
    // This prevents importing stale sessions from crashed daemons or other instances
    let workspace_from_env = std::env::var("LOOM_WORKSPACE").ok();
    let configured_ids = workspace_from_env
        .as_ref()
        .and_then(|workspace| extract_configured_terminal_ids(Path::new(workspace)));

    // Initialize terminal manager and clean up stale sessions
    let mut tm = TerminalManager::new();

    // Use config-based filtering if workspace config is available
    if let Some(ref ids) = configured_ids {
        tm.restore_from_tmux_with_filter(Some(ids))?;
    } else {
        // Fall back to legacy behavior (import all) when no config available
        log::warn!("No workspace config found - using legacy restore (all sessions)");
        tm.restore_from_tmux()?;
    }
    log::info!("Restored {} terminals", tm.list_terminals().len());

    match tm.clean_stale_sessions() {
        Ok(0) => log::debug!("No stale tmux sessions to clean"),
        Ok(count) => log::info!("Cleaned {count} stale tmux session(s) from previous run"),
        Err(e) => log::warn!("Failed to clean stale tmux sessions: {e}"),
    }

    let tm = Arc::new(Mutex::new(tm));

    // Start health monitoring (enabled by default)
    if let Some(interval) = health_monitor::check_env_enabled() {
        let (_health_handle, _health_state) = health_monitor::start_tmux_health_monitor(interval);
        log::info!("tmux health monitoring enabled (interval: {interval}s)");
        // Note: health_handle is dropped here, but the thread keeps running
        // health_state could be stored for querying crash status if needed
    }

    // Start GitHub metrics collection (if workspace is set)
    // Workspace can be set via LOOM_WORKSPACE environment variable (reuse variable from above)
    let db_path_str = db_path.to_str().map(std::string::ToString::to_string);

    if let (Some(workspace), Some(db_path_string)) = (workspace_from_env.as_deref(), db_path_str) {
        let _metrics_handle =
            metrics_collector::try_init_metrics_collector(Some(workspace), &db_path_string);
        // Note: metrics_handle is dropped here, but the thread keeps running if enabled
    }

    // Initialize the sweep registry (Issue #3452 — Phase A of #3449).
    // The registry tracks `/loom:sweep` children dispatched via the
    // `DispatchSweep` IPC request. It writes no daemon-side state file;
    // recovery on restart relies on lock dirs + sweep checkpoints + the
    // forge (labels). The reaper task polls live PIDs on a configurable
    // interval (`LOOM_SWEEP_REAPER_INTERVAL_SECS`, default 30s).
    let sweep_workspace = workspace_from_env
        .as_ref()
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let sweep_config = SweepRegistryConfig::new(sweep_workspace.clone());

    // Phase B (#3453): construct the in-memory pub/sub event bus *before*
    // the sweep registry so we can wire it in at construction time. The
    // bus is shared between the registry (publisher for reaper + dispatch
    // events) and the IPC server (publisher for `PublishEvent` requests
    // from sweep children, plus consumer for `SubscribeEvents` streams).
    let event_bus = Arc::new(EventBus::new());
    log::info!("event_bus: started in-memory pub/sub (capacity={})", event_bus.capacity());

    let mut sweep = SweepRegistry::with_event_bus(sweep_config, event_bus.clone());
    match sweep.reconstruct() {
        Ok(0) => log::debug!("sweep_registry: no sweeps to reconstruct"),
        Ok(n) => log::info!(
            "sweep_registry: reconstructed {n} sweep entr{}",
            if n == 1 { "y" } else { "ies" }
        ),
        Err(e) => log::warn!("sweep_registry: reconstruction failed: {e}"),
    }
    let sweep_registry = Arc::new(Mutex::new(sweep));
    let _reaper_handle = sweep_registry::spawn_reaper_task(sweep_registry.clone());

    // Epic supervisor loop (Issue #3872 — Phase 4 of epic #3842). Opt-in via
    // `LOOM_EPIC_SUPERVISOR`. The loop drives every open `loom:epic` issue
    // through its fork-join lifecycle by dispatching the enabled role each tick.
    //
    // It runs on a DEDICATED OS THREAD with its own current-thread runtime —
    // NOT `tokio::spawn` on this shared daemon runtime — because the concrete
    // `SpawnDispatcher::dispatch_role` is spawn-and-wait (`Command::status()`
    // blocks for the full lifetime of each Architect/Champion process, holding
    // the #3707 issue-creation mutex across the burst). Keeping that blocking
    // call off the shared runtime preserves the responsiveness of the event
    // bus, reaper, sweep registry, and IPC listener while a role process runs.
    let supervisor_handle = if epic_supervisor::supervisor_enabled() {
        match SweepRegistryConfig::new(sweep_workspace.clone()).resolve_spawn_bin() {
            Ok(spawn_bin) => {
                let source = epic_supervisor::forge::GhEpicSource::new();
                let dispatcher =
                    epic_supervisor::forge::SpawnDispatcher::new(spawn_bin, sweep_registry.clone());
                let supervisor = EpicSupervisor::new(source, dispatcher, IssueCreationMutex::new())
                    .with_event_bus(event_bus.clone());
                let interval = epic_supervisor::resolve_supervisor_interval();
                match epic_supervisor::spawn_supervisor_thread(supervisor, interval) {
                    Ok(handle) => {
                        log::info!("epic_supervisor: enabled (interval={}s)", interval.as_secs());
                        Some(handle)
                    }
                    Err(e) => {
                        log::error!("epic_supervisor: failed to start loop thread: {e}");
                        None
                    }
                }
            }
            Err(e) => {
                log::warn!("epic_supervisor: enabled but spawn binary unavailable: {e}");
                None
            }
        }
    } else {
        log::debug!("epic_supervisor: disabled (set LOOM_EPIC_SUPERVISOR=1 to enable)");
        None
    };
    // Shared shutdown flag so the signal handler can stop the loop cleanly.
    let supervisor_shutdown = supervisor_handle
        .as_ref()
        .map(epic_supervisor::SupervisorHandle::shutdown_token);
    // Keep the handle alive for the daemon's lifetime; its Drop signals stop.
    let _supervisor_handle = supervisor_handle;

    // Autonomous work-finder loop (Issue #3810 — Phase A of epic #3809; dynamic
    // concurrency scaling added in #3811 — Phase B). Opt-in via
    // `LOOM_WORK_FINDER`. Each tick queries the forge for open `loom:issue`
    // items and dispatches up to a **work-driven** cap — recomputed every tick as
    // `min(token-pool size, disk headroom, configured_max)` — through the same
    // `SweepRegistry::dispatch()` path the IPC `DispatchSweep` request uses.
    // `LOOM_WORK_FINDER_MAX_CONCURRENT` is repurposed (Phase A → B) from a fixed
    // target into the operator ceiling; the cap also never exceeds the token-pool
    // size (no account over-subscription) nor the scratch-volume disk headroom.
    //
    // Unlike the epic supervisor above, this runs as a plain `tokio::spawn`
    // interval task on the shared daemon runtime (like the reaper): every call
    // into `dispatch()` returns promptly (fire-and-forget child spawn), so the
    // finder never parks a runtime worker in a long blocking call.
    // Shared reactive main-health halt flag (Issue #3812 — Phase C of epic
    // #3809). Always constructed so it can be threaded into the work-finder;
    // when the gate loop below is disabled nothing ever flips it, so the
    // work-finder is never halted (zero behavior change with the gate off).
    let main_health_state = Arc::new(main_health_gate::MainHealthState::new());

    // Config surface (#3813): `.loom/config.json → autonomous.workFinder` lets a
    // repo enable/tune the loop from committed config with zero env vars, while
    // an operator env var still overrides for a single run (precedence env >
    // config > default). An absent `autonomous` block is byte-for-byte the
    // env-only behavior shipped in Phases A/B.
    let work_finder_config = work_finder::read_work_finder_config(&sweep_workspace);

    let _work_finder_handle = if work_finder::resolve_enabled(&work_finder_config) {
        let source = work_finder::GhWorkSource::new();
        let dispatcher = work_finder::RegistryDispatcher::new(sweep_registry.clone());
        let interval = work_finder::resolve_interval_with_config(&work_finder_config);
        let configured_max = work_finder::resolve_max_concurrent_with_config(&work_finder_config);
        log::info!(
            "work_finder: enabled (interval={}s, configured_max={configured_max}, \
             dynamic cap = min(pool, disk, configured_max))",
            interval.as_secs()
        );
        Some(work_finder::spawn_work_finder_task(
            source,
            dispatcher,
            interval,
            sweep_workspace.clone(),
            configured_max,
            main_health_state.clone(),
        ))
    } else {
        log::debug!("work_finder: disabled (set LOOM_WORK_FINDER=1 to enable)");
        None
    };

    // Reactive main-health backstop loop (Issue #3812 — Phase C of epic #3809).
    // Opt-in via `LOOM_MAIN_HEALTH_GATE` AND a `buildGate` block in
    // `.loom/config.json`. On a red `main` (a non-zero `buildGate.command`) it
    // sets `main_health_state` halted, which stops the work-finder above from
    // dispatching new sweeps until a green run clears it. The gate command runs
    // on a blocking thread (it may take minutes), so a plain `tokio::spawn`
    // interval task on the shared runtime is correct (like the reaper).
    // Config surface (#3813): `autonomous.mainHealthGate.enabled` can enable the
    // gate from committed config; `LOOM_MAIN_HEALTH_GATE` remains the master
    // on/off override (precedence env > config > default). The gate's *behavior*
    // (command, timeout) still comes from the separate `buildGate` block, so
    // Phase C's tested semantics are unchanged.
    let autonomous_gate_config = main_health_gate::read_autonomous_gate_config(&sweep_workspace);
    let _main_health_gate_handle = if main_health_gate::resolve_enabled(&autonomous_gate_config) {
        match main_health_gate::read_build_gate_config(&sweep_workspace) {
            Some(gate_config) => {
                let interval = main_health_gate::resolve_interval();
                log::info!(
                    "main_health_gate: enabled (interval={}s, command={:?}, timeout={}s)",
                    interval.as_secs(),
                    gate_config.command,
                    gate_config.timeout.as_secs()
                );
                let runner =
                    main_health_gate::CommandGateRunner::new(gate_config, sweep_workspace.clone());
                Some(main_health_gate::spawn_main_health_gate_task(
                    runner,
                    main_health_state.clone(),
                    interval,
                ))
            }
            None => {
                log::warn!(
                    "main_health_gate: LOOM_MAIN_HEALTH_GATE is set but no usable buildGate \
                     config in .loom/config.json (missing/disabled/empty command) — gate inactive"
                );
                None
            }
        }
    } else {
        log::debug!("main_health_gate: disabled (set LOOM_MAIN_HEALTH_GATE=1 or autonomous.mainHealthGate.enabled=true + a buildGate config to enable)");
        None
    };

    // Start IPC server
    let server = IpcServer::new(socket_path.clone(), tm, activity_db, sweep_registry, event_bus);

    // Setup signal handler for graceful shutdown. We listen for BOTH SIGINT
    // (Ctrl-C, interactive) and SIGTERM (`kill <pid>`, the default signal a
    // backgrounded daemon receives from `loom-daemon-stop.sh` — #3813). Either
    // one removes the socket and exits cleanly so a subsequent start does not
    // trip the singleton guard on a stale socket.
    //
    // In-flight `/loom:sweep` children are NOT cancelled on shutdown — they are
    // independent detached processes and survive a daemon restart by design
    // (killing the dispatcher must not kill dispatched work). This is the
    // documented "survive, don't drain" decision (see daemon-reference.md).
    let socket_path_clone = socket_path.clone();
    tokio::spawn(async move {
        let signal_name = wait_for_shutdown_signal().await;
        log::info!("Received {signal_name}, cleaning up...");
        // Signal the off-runtime epic supervisor loop to stop.
        if let Some(flag) = &supervisor_shutdown {
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        let _ = tokio::fs::remove_file(&socket_path_clone).await;
        log::info!("Socket cleaned up, exiting");
        std::process::exit(0);
    });

    log::info!("Loom daemon starting...");
    server.run().await?;

    Ok(())
}

/// Await either SIGINT (Ctrl-C) or, on Unix, SIGTERM (`kill <pid>`), returning a
/// short human-readable name for whichever fired first. On non-Unix platforms
/// only Ctrl-C is available. Introduced in #3813 so a backgrounded daemon shut
/// down via `kill` (SIGTERM) cleans up its socket exactly like an interactive
/// Ctrl-C, rather than being torn down by the default SIGTERM disposition with
/// the socket left behind.
async fn wait_for_shutdown_signal() -> &'static str {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        // If a signal stream cannot be installed, fall back to Ctrl-C only
        // rather than aborting shutdown handling entirely.
        match signal(SignalKind::terminate()) {
            Ok(mut sigterm) => {
                tokio::select! {
                    r = tokio::signal::ctrl_c() => {
                        if let Err(err) = r {
                            log::error!("Unable to listen for Ctrl-C: {err}");
                        }
                        "SIGINT (Ctrl-C)"
                    }
                    _ = sigterm.recv() => "SIGTERM",
                }
            }
            Err(err) => {
                log::error!("Unable to install SIGTERM handler ({err}); listening for Ctrl-C only");
                if let Err(err) = tokio::signal::ctrl_c().await {
                    log::error!("Unable to listen for Ctrl-C: {err}");
                }
                "SIGINT (Ctrl-C)"
            }
        }
    }
    #[cfg(not(unix))]
    {
        if let Err(err) = tokio::signal::ctrl_c().await {
            log::error!("Unable to listen for Ctrl-C: {err}");
        }
        "SIGINT (Ctrl-C)"
    }
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
    let log_path = dirs::home_dir()
        .ok_or_else(|| anyhow!("No home directory"))?
        .join(".loom/daemon.log");

    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Rotate log file if it exceeds 10MB (keeps last 10 files)
    rotate_log_file(&log_path, 10 * 1024 * 1024, 10)?;

    let log_file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;

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
                println!(
                    "  3. Setup repository scaffolding (CLAUDE.md, .claude/, .codex/, .github/)"
                );
                println!("  4. Update .gitignore with Loom ephemeral patterns");
                return Ok(());
            }

            println!("Initializing Loom workspace...");
            println!("  Workspace: {workspace_str}");
            println!("  Defaults:  {defaults}");

            match loom_daemon::init::initialize_workspace(workspace_str, &defaults, force) {
                Ok(report) => {
                    if report.is_self_install {
                        println!("\nLoom source repository detected!");
                        println!("\nMode: Validation only (self-installation)");
                        println!("\nValidating configuration...");

                        if let Some(ref validation) = report.validation {
                            println!(
                                "  .loom/roles/    - {} role definitions found",
                                validation.roles_found.len()
                            );
                            println!(
                                "  .loom/scripts/  - {} scripts found",
                                validation.scripts_found.len()
                            );
                            println!(
                                "  .claude/commands/loom/ - {} slash commands found",
                                validation.commands_found.len()
                            );

                            if validation.has_claude_md {
                                println!("  CLAUDE.md       - Present");
                            } else {
                                println!("  CLAUDE.md       - Missing");
                            }

                            if validation.has_labels_yml {
                                println!("  .github/labels.yml - Present");
                            } else {
                                println!("  .github/labels.yml - Missing");
                            }

                            if validation.issues.is_empty() {
                                println!("\nLoom source repository is properly configured");
                            } else {
                                println!("\nIssues found:");
                                for issue in &validation.issues {
                                    println!("  - {issue}");
                                }
                            }

                            println!("\nRoles found: {}", validation.roles_found.join(", "));
                        }

                        println!("\nSelf-installation skips file copying to prevent data loss.");
                        println!("   The Loom repo's .loom/ directory IS the source of truth.");
                        println!("\nTo use Loom orchestration:");
                        println!("  - Open Claude Code terminals with /builder, /judge, etc.");
                        println!("  - Or start the daemon: ./.loom/scripts/daemon.sh start");

                        return Ok(());
                    }

                    println!("\nLoom workspace initialized successfully!");
                    println!("\nFiles installed:");
                    println!("  .loom/          - Configuration directory");
                    println!("  .loom/config.json - Terminal configuration");
                    println!("  .loom/roles/    - Agent role definitions");
                    println!("  CLAUDE.md       - AI context documentation");
                    println!("  .claude/        - Claude Code configuration");
                    println!("  .codex/         - Codex configuration");
                    println!("  .github/        - GitHub labels and issue templates");
                    println!("  .gitignore      - Updated with Loom patterns");

                    if !report.added.is_empty()
                        || !report.preserved.is_empty()
                        || !report.removed.is_empty()
                    {
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
                            println!("\n  Preserved files were not overwritten. To update them,");
                            println!("     delete them and run install again, or use --force.");
                        }
                        if !report.updated.is_empty() {
                            println!("\nFiles updated ({}):", report.updated.len());
                            for file in &report.updated {
                                println!("  ~ {file}");
                            }
                        }
                        if !report.removed.is_empty() {
                            println!("\nFiles removed ({}):", report.removed.len());
                            for file in &report.removed {
                                println!("  - {file}");
                            }
                        }
                        if !report.verification_failures.is_empty() {
                            eprintln!(
                                "\nUnexpected file divergence ({}):",
                                report.verification_failures.len()
                            );
                            for failure in &report.verification_failures {
                                eprintln!("  {failure}");
                            }
                            eprintln!(
                                "\n  These files were copied from defaults but their installed"
                            );
                            eprintln!(
                                "  contents differ from the source. This is informational only —"
                            );
                            eprintln!(
                                "  installation completed. Inspect the listed files to confirm"
                            );
                            eprintln!("  they look correct.");
                        }
                    }

                    println!("\nNext steps:");
                    println!(
                        "  1. Commit the changes: git add -A && git commit -m 'Add Loom configuration'"
                    );
                    println!("  2. Choose your workflow:");
                    println!("     Manual Mode (recommended to start):");
                    println!("       cd {workspace_str} && claude");
                    println!("       Then use /builder, /judge, or other role commands");
                    println!("     Daemon Mode (autonomous orchestration):");
                    println!("       cd {workspace_str} && ./.loom/scripts/daemon.sh start");
                    println!("       Then in Claude Code: /loom");
                    Ok(())
                }
                Err(e) => {
                    eprintln!("\nFailed to initialize workspace: {e}");
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

fn print_agent_effectiveness(agent: &activity::AgentEffectiveness) {
    println!("Role: {}", agent.agent_role);
    println!("  Total Prompts:      {}", agent.total_prompts);
    println!("  Successful Prompts: {}", agent.successful_prompts);
    println!("  Success Rate:       {:.1}%", agent.success_rate);
    println!("  Average Cost:       ${:.4}", agent.avg_cost);
    println!("  Average Duration:   {:.1}s", agent.avg_duration_sec);
    println!();
}

fn handle_validate_command(
    workspace: &str,
    format: &str,
    strict: bool,
    verbose: bool,
) -> Result<()> {
    use role_validation::{format_validation_result, validate_from_file, ValidationMode};

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
        if verbose {
            println!("\nValidating role configuration...");
            println!("  Config: {}", config_path.display());
            println!();
        }

        let output = format_validation_result(&result, verbose);
        if !output.is_empty() {
            print!("{output}");
        }

        if result.warnings.is_empty() && result.errors.is_empty() {
            println!("All role dependencies are satisfied.");
        }
    }

    if !result.errors.is_empty() {
        std::process::exit(1);
    } else if !result.warnings.is_empty() && strict {
        std::process::exit(2);
    }

    Ok(())
}
