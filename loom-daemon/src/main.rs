use loom_daemon::activity::{self, ActivityDb, StatsQueries};
use loom_daemon::claim_reconciliation;
use loom_daemon::epic_supervisor;
use loom_daemon::event_bus::EventBus;
use loom_daemon::health_monitor;
use loom_daemon::ipc::IpcServer;
use loom_daemon::main_health_gate;
use loom_daemon::metrics_collector;
use loom_daemon::role_runner;
use loom_daemon::role_validation;
use loom_daemon::self_update;
use loom_daemon::sweep_registry::{self, SweepRegistry, SweepRegistryConfig};
use loom_daemon::terminal::TerminalManager;
use loom_daemon::token_ranking_refresh;
use loom_daemon::types::{DaemonStatusReport, Request, Response, SweepKind};
use loom_daemon::watch_registry;
use loom_daemon::work_finder;
use loom_daemon::workspace_pool::WorkspacePool;
use loom_daemon::{extract_configured_terminal_ids, rotate_log_file};

use anyhow::{anyhow, Result};
use clap::{Parser, Subcommand};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

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

    /// Show the running daemon's autonomous-mode status: in-flight sweeps, the
    /// three dynamic-cap inputs (token-pool size, disk headroom, configured
    /// ceiling) plus their `min` cap, the main-health-gate halt state, and
    /// per-token usage. Connects to the running daemon over its Unix socket
    /// (Issue #3891 — follow-up to #3813 Phase D).
    Status {
        /// Emit machine-readable JSON instead of the human-readable table.
        #[arg(long)]
        json: bool,

        /// Also show the forge-side pipeline snapshot per managed repo (Issue
        /// #3977): open `loom:issue` (queued), open `loom:building`
        /// (claimed), open PRs by `loom:review-requested` /
        /// `loom:changes-requested` / `loom:pr`, and PRs merged in the last
        /// 24h. Opt-in because it makes several `gh` calls per managed repo
        /// (client-side, after the fast IPC round-trip) rather than being
        /// bundled into the default view.
        #[arg(long)]
        pipeline: bool,
    },

    /// Manage the machine-level workspace registry (`~/.loom/workspaces.json`):
    /// the set of repos the one-per-machine daemon manages (Issue #3926 — phase
    /// 1 of #3835). Operates directly on the registry file, so it works whether
    /// or not the daemon is running; a running daemon re-reads the file on its
    /// next tick (hot-apply).
    Workspace {
        #[command(subcommand)]
        action: WorkspaceAction,
    },

    /// Manage insta-crash quarantines (Issue #3939): the in-memory pauses the
    /// daemon applies to issues whose sweeps insta-crash repeatedly. Connects to
    /// the running daemon over its Unix socket, since the quarantine state lives
    /// in the daemon's memory (not on disk).
    Quarantine {
        #[command(subcommand)]
        action: QuarantineAction,
    },

    /// Dispatch a `/loom:sweep <issue>` via the running daemon (Issue #3952): a
    /// first-class, non-MCP operator entry point over the same IPC `DispatchSweep`
    /// request the `mcp__loom__dispatch_sweep` tool uses. Registry tracking, the
    /// `loom:issue → loom:building` claim flip, and event publishing all come for
    /// free — this is the resilient replacement for the hand-rolled
    /// `LOOM_SWEEP_CLAIM_OWNED=<N>` + `spawn-claude.sh -p "/loom:sweep N"` pattern.
    ///
    /// Applies a bounded client-side ack timeout: if the daemon does not respond
    /// within a few seconds the command exits nonzero with a clear error instead
    /// of hanging (motivated by the 1800s MCP wedge in #3945).
    Dispatch {
        /// The issue number to dispatch a sweep for.
        #[arg(value_name = "ISSUE")]
        issue: u32,

        /// Target managed-workspace root (Issue #3929 plumbing). Omit to dispatch
        /// into the daemon's default workspace.
        #[arg(long, value_name = "PATH")]
        workspace: Option<String>,

        /// Pin the spawned child to an explicit model (e.g. `sonnet`, `opus`).
        /// Omit to let the daemon resolve `autonomous.model` / the shipped default.
        #[arg(long, value_name = "M")]
        model: Option<String>,

        /// Reasoning-effort override forwarded to the spawned child.
        #[arg(long, value_name = "E")]
        effort: Option<String>,

        /// Single parent issue for a stacked sweep (Issue #3729): the child
        /// branches its worktree/PR off `feature/issue-<P>` instead of the default
        /// branch.
        #[arg(long, value_name = "P")]
        depends_on: Option<u32>,
    },

    /// Manage durable operator watches on issue/PR terminal state (Issue #3971).
    /// A watch registered here is persisted to `~/.loom/watches.json` and polled
    /// by the running daemon, so it survives this shell — and even a daemon
    /// restart. Terminal resolutions land in `~/.loom/logs/watch-results.log`.
    /// Connects to the running daemon over its Unix socket.
    Watch {
        #[command(subcommand)]
        action: WatchAction,
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

/// Sub-actions for `loom-daemon workspace`.
#[derive(Subcommand)]
enum WorkspaceAction {
    /// Register a repo as a managed workspace.
    Add {
        /// Path to the repo root (relative or absolute; normalized on store).
        #[arg(value_name = "PATH")]
        path: String,

        /// Cross-repo dispatch priority tier (#3946): lower = higher priority.
        /// The autonomous work-finder and epic supervisor drain higher-priority
        /// repos first. Defaults to 100 when omitted.
        #[arg(long, value_name = "N", default_value_t = loom_daemon::workspace_registry::DEFAULT_WORKSPACE_PRIORITY)]
        priority: u32,

        /// Optional per-repo config overrides as a JSON object string.
        #[arg(long, value_name = "JSON")]
        config_overrides: Option<String>,
    },
    /// Set the dispatch priority tier of an already-registered workspace (#3946).
    SetPriority {
        /// Path to the repo root (normalized the same way as `add`).
        #[arg(value_name = "PATH")]
        path: String,

        /// New priority tier: lower = higher priority.
        #[arg(value_name = "N")]
        priority: u32,
    },
    /// Deregister a managed workspace by root.
    Remove {
        /// Path to the repo root (normalized the same way as `add`).
        #[arg(value_name = "PATH")]
        path: String,
    },
    /// List the managed workspaces.
    List {
        /// Emit machine-readable JSON instead of the human-readable table.
        #[arg(long)]
        json: bool,
    },
}

/// Sub-actions for `loom-daemon watch` (Issue #3971).
#[derive(Subcommand)]
enum WatchAction {
    /// Register a durable watch on an issue's or PR's terminal state.
    Add {
        /// The issue or PR number to watch.
        #[arg(value_name = "NUMBER")]
        number: u32,

        /// Watch a pull request instead of an issue.
        #[arg(long)]
        pr: bool,

        /// Forge slug `owner/name` (preferred — works cross-repo for a repo this
        /// machine may not manage). Omit to resolve from `--workspace-root` or the
        /// daemon's own cwd.
        #[arg(long, value_name = "OWNER/NAME")]
        repo: Option<String>,

        /// Workspace root the `gh` query runs in when `--repo` is absent.
        #[arg(long, value_name = "PATH")]
        workspace_root: Option<String>,

        /// Optional note surfaced in the recorded result line.
        #[arg(long, value_name = "TEXT")]
        note: Option<String>,
    },
    /// List the currently-registered durable watches.
    List {
        /// Emit machine-readable JSON instead of the human-readable table.
        #[arg(long)]
        json: bool,
    },
    /// Remove a registered watch by its id.
    Remove {
        /// The watch id (as printed by `watch add` / `watch list`).
        #[arg(value_name = "ID")]
        id: String,
    },
}

/// Sub-actions for `loom-daemon quarantine`.
#[derive(Subcommand)]
enum QuarantineAction {
    /// Clear an issue's insta-crash quarantine (Issue #3939): release the
    /// daemon's in-memory pause + insta-crash tally so the work finder
    /// re-qualifies it immediately (instead of waiting for the TTL) and restore
    /// `loom:issue` on the forge. Idempotent — clearing a non-quarantined issue
    /// is a no-op success.
    Clear {
        /// The issue number whose quarantine to clear.
        #[arg(value_name = "ISSUE")]
        issue: u32,

        /// Target managed-workspace root (Issue #3929). Omit to use the daemon's
        /// default workspace.
        #[arg(long, value_name = "PATH")]
        workspace_root: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Handle CLI commands (init mode)
    if let Some(command) = cli.command {
        return match command {
            // `status` connects to the running daemon over its Unix socket, so
            // it needs the async runtime (unlike the other sync subcommands).
            Commands::Status { json, pipeline } => handle_status_command(json, pipeline).await,
            // `quarantine` connects to the running daemon over its Unix socket
            // (the quarantine state is in-memory), so it needs the async runtime.
            Commands::Quarantine { action } => handle_quarantine_command(action).await,
            // `dispatch` connects to the running daemon over its Unix socket to
            // enqueue a sweep (Issue #3952), so it needs the async runtime.
            Commands::Dispatch {
                issue,
                workspace,
                model,
                effort,
                depends_on,
            } => handle_dispatch_command(issue, workspace, model, effort, depends_on).await,
            // `watch` connects to the running daemon over its Unix socket to
            // register/list/remove durable watches (Issue #3971).
            Commands::Watch { action } => handle_watch_command(action).await,
            other => handle_cli_command(other),
        };
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

    // Stale-`loom:building`-claim reconciliation across every managed
    // workspace (Issue #3953). `sweep.reconstruct()` above only recovers
    // entries this registry itself owns evidence for (locks/checkpoints); it
    // has nothing to say about a claim left behind by a sweep that died
    // between the previous daemon's exit and this one's start (rate-limit
    // kill, print-mode ceiling, an operator upgrade — the daemon-restart
    // gap where `loom-recover-orphans` used to find *no* authoritative
    // liveness source at all and refuse to reclaim anything, #3651).
    //
    // The machine-level sweep journal (`~/.loom/sweeps.json`, written by
    // every `dispatch()` — see `sweep_journal`) is that liveness source, and
    // it survives exactly the restart that wipes this in-memory registry.
    // This pass is a bounded, logged, best-effort startup sweep over every
    // `effective_roots()` workspace (empty registry ⇒ just this one). It
    // never blocks daemon startup — a `gh` hiccup in one repo is logged and
    // skipped, and the remaining repos are still reconciled.
    if claim_reconciliation::reconciliation_enabled() {
        let workspace_registry =
            loom_daemon::workspace_registry::WorkspaceRegistry::load_default().unwrap_or_default();
        let roots = workspace_registry.effective_roots(&sweep_workspace);
        let gh_bin = std::path::PathBuf::from("gh");
        let mut total_checked = 0usize;
        let mut total_reclaimed = 0usize;
        for root in &roots {
            let (checked, reclaimed) =
                claim_reconciliation::forge::reconcile_workspace(&gh_bin, root);
            total_checked += checked;
            total_reclaimed += reclaimed;
        }
        if total_reclaimed > 0 {
            log::info!(
                "claim_reconciliation: startup pass checked {total_checked} loom:building \
                 issue(s) across {} workspace(s), reclaimed {total_reclaimed} stale claim(s) (#3953)",
                roots.len()
            );
        } else {
            log::debug!(
                "claim_reconciliation: startup pass checked {total_checked} loom:building \
                 issue(s) across {} workspace(s), nothing to reclaim",
                roots.len()
            );
        }
    } else {
        log::info!(
            "claim_reconciliation: startup pass disabled ({}=0)",
            claim_reconciliation::RECONCILE_ENABLED_ENV
        );
    }

    // Startup-race mitigation (Issue #3887): resolve the dispatch stagger + the
    // watchdog knobs from `.loom/config.json → autonomous` with env override
    // (precedence env > config > default). The stagger serializes back-to-back
    // child startups so a burst dispatch does not trip the 0-HTTPS MCP-init
    // race; the watchdog is the self-healing backstop for any hang that slips
    // past it.
    let startup_race_config = sweep_registry::read_startup_race_config(&sweep_workspace);
    let dispatch_stagger = sweep_registry::resolve_dispatch_stagger(&startup_race_config);
    sweep.set_dispatch_stagger(dispatch_stagger);
    log::info!("sweep_registry: dispatch stagger = {}ms (#3887)", dispatch_stagger.as_millis());

    // Insta-crash quarantine (#3939): resolve env > config > default for the
    // default workspace so the reaper quarantines a repeatedly-insta-crashing
    // issue instead of letting the work finder re-dispatch it every tick.
    let quarantine_config = sweep_registry::resolve_quarantine_config(&sweep_workspace);
    sweep.set_quarantine_config(quarantine_config);
    log::info!(
        "sweep_registry: insta-crash quarantine {} (threshold={}, ttl={}s, insta-crash<{}s) (#3939)",
        if quarantine_config.enabled { "enabled" } else { "disabled" },
        quarantine_config.threshold,
        quarantine_config.ttl.as_secs(),
        quarantine_config.insta_crash_secs
    );

    let sweep_registry = Arc::new(Mutex::new(sweep));
    let _reaper_handle = sweep_registry::spawn_reaper_task(sweep_registry.clone());

    // Multi-workspace sweep-registry pool (Issue #3928 — phase b of #3835/#3926).
    // Both autonomous loops (work-finder + epic supervisor) share ONE pool so a
    // given repo has exactly one `SweepRegistry` instance — unifying in-flight
    // dedup and the reaper across both loops. The pool captures a handle to this
    // shared daemon runtime so every provisioned registry's reaper runs here even
    // when a workspace is first touched from the epic supervisor's OS thread. The
    // default workspace is *seeded* with the registry constructed above (also used
    // by the IPC `DispatchSweep` path), so the empty-registry case reuses it
    // byte-for-byte rather than building a second instance.
    let workspace_pool =
        Arc::new(WorkspacePool::new(event_bus.clone(), tokio::runtime::Handle::current()));
    workspace_pool.seed(sweep_workspace.clone(), sweep_registry.clone());

    // Startup watchdog (Issue #3887): auto-cancel + re-dispatch (once, bounded)
    // any daemon-dispatched sweep that hangs at startup with no progress. On by
    // default; disable with LOOM_SWEEP_WATCHDOG=0 or
    // `autonomous.watchdog.enabled = false`.
    let _watchdog_handle = if sweep_registry::resolve_watchdog_enabled(&startup_race_config) {
        let timeout = sweep_registry::resolve_watchdog_timeout(&startup_race_config);
        let interval = sweep_registry::resolve_watchdog_interval(&startup_race_config);
        // Review-phase stall watchdog (Issue #3910): a third backstop, resolved
        // independently (env > config > default, defaults on) and threaded into
        // the same tick. `None` disables it without touching the startup
        // watchdog. It catches a still-running sweep wedged in a hung
        // Judge/Doctor subagent (log silent past the stall timeout).
        let review_stall_timeout =
            if sweep_registry::resolve_review_stall_enabled(&startup_race_config) {
                Some(sweep_registry::resolve_review_stall_timeout(&startup_race_config))
            } else {
                log::info!("sweep_registry: review-phase stall watchdog disabled (#3910)");
                None
            };
        Some(sweep_registry::spawn_watchdog_task(
            sweep_registry.clone(),
            timeout,
            interval,
            review_stall_timeout,
        ))
    } else {
        log::info!("sweep_registry: startup watchdog disabled (#3887)");
        None
    };

    // Shared reactive main-health halt flag (Issue #3812 — Phase C of epic
    // #3809). Constructed here — before both the epic supervisor and the
    // work-finder — so it can be threaded into both dispatch paths. When the gate
    // loop (below) is disabled nothing ever flips it, so neither the supervisor
    // nor the work-finder is ever halted (zero behavior change with the gate off).
    // Per-repo halt state (#3930): one `MainHealthState` per registered repo,
    // keyed by normalized root, so a red `main` in one managed repo halts only
    // that repo's dispatch — never the siblings'. With an empty registry (the
    // common single-workspace case) exactly one root is keyed, reducing to the
    // pre-#3930 single-flag behavior byte-for-byte.
    let workspace_health_states = Arc::new(main_health_gate::WorkspaceHealthStates::new());

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
    // Multi-workspace fan-out (#3928): the supervisor loop resolves
    // `effective_roots()` each tick and drives one `EpicSupervisor` per registered
    // workspace (empty registry ⇒ the single `sweep_workspace`, byte-for-byte).
    // Per-root spawn-binary resolution now happens inside the loop thread, so no
    // single up-front `resolve_spawn_bin()` gate is needed here.
    let supervisor_handle = if epic_supervisor::supervisor_enabled() {
        let interval = epic_supervisor::resolve_supervisor_interval();
        match epic_supervisor::spawn_multi_supervisor_thread(
            workspace_pool.clone(),
            sweep_workspace.clone(),
            event_bus.clone(),
            workspace_health_states.clone(),
            interval,
        ) {
            Ok(handle) => {
                log::info!(
                    "epic_supervisor: enabled (multi-workspace, interval={}s)",
                    interval.as_secs()
                );
                Some(handle)
            }
            Err(e) => {
                log::error!("epic_supervisor: failed to start loop thread: {e}");
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
    // `min(token-pool size, disk headroom, cpu/load headroom, configured_max)`
    // (CPU/load term added in #3978) — through the same `SweepRegistry::dispatch()`
    // path the IPC `DispatchSweep` request uses. `LOOM_WORK_FINDER_MAX_CONCURRENT`
    // is repurposed (Phase A → B) from a fixed target into the operator ceiling;
    // the cap also never exceeds the token-pool size (no account
    // over-subscription), the scratch-volume disk headroom, nor the host's CPU
    // headroom (never starve concurrent sweep builds into starving the
    // main-health gate's own build, #3978).
    //
    // Unlike the epic supervisor above, this runs as a plain `tokio::spawn`
    // interval task on the shared daemon runtime (like the reaper): every call
    // into `dispatch()` returns promptly (fire-and-forget child spawn), so the
    // finder never parks a runtime worker in a long blocking call.
    // The shared reactive main-health halt flag (`main_health_state`) is
    // constructed above (before the epic supervisor) so both dispatch paths share
    // it. Config surface (#3813): `.loom/config.json → autonomous.workFinder` lets a
    // repo enable/tune the loop from committed config with zero env vars, while
    // an operator env var still overrides for a single run (precedence env >
    // config > default). An absent `autonomous` block is byte-for-byte the
    // env-only behavior shipped in Phases A/B.
    let work_finder_config = work_finder::read_work_finder_config(&sweep_workspace);

    let _work_finder_handle = if work_finder::resolve_enabled(&work_finder_config) {
        let interval = work_finder::resolve_interval_with_config(&work_finder_config);
        let configured_max = work_finder::resolve_max_concurrent_with_config(&work_finder_config);
        let per_token_concurrency = work_finder::resolve_per_token_concurrency(&work_finder_config);
        log::info!(
            "work_finder: enabled (multi-workspace, interval={}s, configured_max={configured_max}, \
             per_token_concurrency={per_token_concurrency}, \
             dynamic cap = min(healthy tokens × per-token, disk, cpu, configured_max), \
             global across workspaces)",
            interval.as_secs()
        );
        // Multi-workspace fan-out (#3928): re-reads `effective_roots()` each tick
        // and dispatches into each registered repo's own working tree via the
        // shared `workspace_pool`. Empty registry ⇒ the single `sweep_workspace`.
        Some(work_finder::spawn_multi_work_finder_task(
            workspace_pool.clone(),
            sweep_workspace.clone(),
            interval,
            configured_max,
            per_token_concurrency,
            workspace_health_states.clone(),
            event_bus.clone(),
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
    // Multi-workspace fan-out (#3930): the gate loop re-reads `effective_roots()`
    // each cycle and runs one gate check per registered root, writing into that
    // root's own `MainHealthState` in `workspace_health_states`. A red repo halts
    // only its own dispatch. Per-root enablement + `buildGate` config are read
    // from each repo's own `.loom/config.json` inside the loop, so no single
    // up-front buildGate resolution is needed. The startup master switch is still
    // the daemon's own workspace config (env > config), mirroring how the
    // work-finder / epic-supervisor loops are gated at startup by `sweep_workspace`.
    let autonomous_gate_config = main_health_gate::read_autonomous_gate_config(&sweep_workspace);
    let _main_health_gate_handle = if main_health_gate::resolve_enabled(&autonomous_gate_config) {
        let interval = main_health_gate::resolve_interval();
        log::info!("main_health_gate: enabled (multi-workspace, interval={}s)", interval.as_secs());
        Some(main_health_gate::spawn_multi_main_health_gate_task(
            workspace_health_states.clone(),
            sweep_workspace.clone(),
            interval,
        ))
    } else {
        log::debug!("main_health_gate: disabled (set LOOM_MAIN_HEALTH_GATE=1 or autonomous.mainHealthGate.enabled=true + a buildGate config to enable)");
        None
    };

    // Autonomous token-ranking refresh loop (Issue #3969): the daemon itself
    // periodically re-probes each registered repo's token pool and rewrites
    // `.loom/tokens/.ranking` (atomically — same script an operator's cron used
    // to run by hand: `probe-tokens.sh --ranking` / `loom-tokens check
    // --ranking`), so token selection's ranking tier stays fresh without a
    // standing manual/cron step. Unlike the work-finder and main-health-gate
    // loops above, this is **default-ON** — it only ever reads rate-limit
    // headers and rewrites a bookkeeping file with no dispatch side effect, so
    // an absent daemon-side refresher would silently regress every install back
    // to the stale-ranking failure mode this issue exists to fix. Config
    // surface: `autonomous.tokenRankingRefresh.{enabled,intervalSecs}`,
    // precedence env > config > default (env:
    // `LOOM_TOKEN_RANKING_REFRESH` / `LOOM_TOKEN_RANKING_REFRESH_INTERVAL_SECS`).
    // An operator cron running the identical script concurrently is harmless —
    // the underlying `loom-tokens check --ranking` write is atomic, so the two
    // refreshers can race to schedule a write but never to a torn file.
    let token_ranking_refresh_config =
        token_ranking_refresh::read_token_ranking_refresh_config(&sweep_workspace);
    let _token_ranking_refresh_handle =
        if token_ranking_refresh::resolve_enabled(&token_ranking_refresh_config) {
            let interval = token_ranking_refresh::resolve_interval(&token_ranking_refresh_config);
            log::info!(
                "token_ranking_refresh: enabled (multi-workspace, interval={}s)",
                interval.as_secs()
            );
            Some(token_ranking_refresh::spawn_multi_token_ranking_refresh_task(
                sweep_workspace.clone(),
                interval,
            ))
        } else {
            log::debug!(
                "token_ranking_refresh: disabled (set LOOM_TOKEN_RANKING_REFRESH=0 or \
             autonomous.tokenRankingRefresh.enabled=false to opt out)"
            );
            None
        };

    // Autonomous periodic support-role runner (Issue #4015): dispatches the
    // standalone support roles (Champion, Curator, Judge, Auditor, Guide)
    // host-side through `spawn-claude.sh` on their own per-role cadence,
    // drawing from the SAME rotated, health-ranked token pool sweeps already
    // use via `sweep_registry` — instead of relying solely on the GitHub
    // Actions cron workflows (`.github/workflows/loom-*.yml`), which
    // authenticate with a single static `CLAUDE_API_KEY` secret with no
    // rotation and no health-awareness. Opt-in via `LOOM_ROLE_RUNNER` (or
    // `autonomous.roleRunner.enabled=true`) — like the work-finder and
    // main-health-gate loops above, this has dispatch-affecting side effects
    // (each tick is a full `claude -p "/<role>"` session that can mutate
    // issues/PRs), so an absent config leaves the daemon's behavior
    // byte-for-byte unchanged. The Actions workflows remain a supported
    // fallback for deployments with no always-on daemon. One task per
    // enabled role is spawned, each on its own multi-workspace loop
    // (`role_runner::spawn_multi_role_task`) — mirrors the token-ranking
    // refresh loop's re-fan-out-every-tick shape.
    let role_runner_config = role_runner::read_role_runner_config(&sweep_workspace);
    let _role_runner_handles = if role_runner::resolve_enabled(&role_runner_config) {
        let roles = role_runner::resolve_roles(&role_runner_config);
        log::info!(
            "role_runner: enabled (multi-workspace, {} role(s): {})",
            roles.len(),
            roles.iter().map(|r| r.name).collect::<Vec<_>>().join(", ")
        );
        let handles: Vec<_> = roles
            .iter()
            .map(|spec| {
                let interval = role_runner::resolve_interval_for_role(spec, &role_runner_config);
                log::info!("role_runner: {} interval={}s", spec.name, interval.as_secs());
                role_runner::spawn_multi_role_task(*spec, sweep_workspace.clone(), interval)
            })
            .collect();
        Some(handles)
    } else {
        log::debug!(
            "role_runner: disabled (set LOOM_ROLE_RUNNER=1 or autonomous.roleRunner.enabled=true to enable)"
        );
        None
    };

    // Durable watch-monitor loop (Issue #3971): the daemon polls the forge for
    // the terminal state of operator-registered issue/PR watches
    // (`~/.loom/watches.json`) and appends resolutions to
    // `~/.loom/logs/watch-results.log`. Because the watch lives in a file the
    // long-lived daemon owns, an operator's watch survives their Claude Code
    // session dying — the failure this issue exists to fix. Like the
    // token-ranking refresh above (and unlike the dispatch-affecting work-finder
    // / main-health-gate loops) it is **default-ON**: it has no dispatch side
    // effect and makes ZERO forge calls until an operator registers a watch.
    // Config surface: `autonomous.watchMonitor.{enabled,intervalSecs,expirySecs}`,
    // precedence env > config > default (env: `LOOM_WATCH_MONITOR` /
    // `LOOM_WATCH_MONITOR_INTERVAL_SECS` / `LOOM_WATCH_MONITOR_EXPIRY_SECS`).
    let watch_monitor_config = watch_registry::read_watch_monitor_config(&sweep_workspace);
    let _watch_monitor_handle = if watch_registry::resolve_enabled(&watch_monitor_config) {
        let interval = watch_registry::resolve_interval(&watch_monitor_config);
        let expiry = watch_registry::resolve_expiry(&watch_monitor_config);
        log::info!(
            "watch_monitor: enabled (interval={}s, expiry={}s)",
            interval.as_secs(),
            expiry.as_secs()
        );
        Some(watch_registry::spawn_watch_monitor_task(
            watch_registry::GhWatchProbe::new(),
            interval,
            expiry,
        ))
    } else {
        log::debug!(
            "watch_monitor: disabled (set LOOM_WATCH_MONITOR=1 or \
             autonomous.watchMonitor.enabled=true to opt in)"
        );
        None
    };

    // Start IPC server. `workspace_health_states` is threaded in so the
    // `DaemonStatus` request can report each registered repo's own halt state
    // (#3930), and `sweep_workspace` is the `effective_roots` fallback for the
    // per-repo status breakdown — the same values the work-finder and gate loop
    // share above.
    let server = IpcServer::new(
        socket_path.clone(),
        tm,
        activity_db,
        sweep_registry,
        event_bus,
        workspace_health_states.clone(),
        workspace_pool.clone(),
        sweep_workspace.clone(),
    );

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
        Commands::Workspace { action } => handle_workspace_command(action),
        Commands::Status { .. } => {
            // Routed directly in `main()` (it needs the async runtime for the
            // socket round-trip), never dispatched through this sync handler.
            unreachable!("Status is handled in main() before handle_cli_command")
        }
        Commands::Quarantine { .. } => {
            // Routed directly in `main()` (it needs the async runtime for the
            // socket round-trip), never dispatched through this sync handler.
            unreachable!("Quarantine is handled in main() before handle_cli_command")
        }
        Commands::Dispatch { .. } => {
            // Routed directly in `main()` (it needs the async runtime for the
            // socket round-trip), never dispatched through this sync handler.
            unreachable!("Dispatch is handled in main() before handle_cli_command")
        }
        Commands::Watch { .. } => {
            // Routed directly in `main()` (it needs the async runtime for the
            // socket round-trip), never dispatched through this sync handler.
            unreachable!("Watch is handled in main() before handle_cli_command")
        }
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

/// Resolve the daemon's IPC socket path exactly as the running daemon does in
/// `main()`: honour `LOOM_SOCKET_PATH` (test override) first, else
/// `~/.loom/loom-daemon.sock`.
fn resolve_socket_path() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("LOOM_SOCKET_PATH") {
        return Ok(PathBuf::from(path));
    }
    let loom_dir = dirs::home_dir()
        .ok_or_else(|| anyhow!("No home directory"))?
        .join(".loom");
    Ok(loom_dir.join("loom-daemon.sock"))
}

/// Connect to the running daemon over its Unix socket, send a single
/// `DaemonStatus` request, and return the parsed report (Issue #3891).
///
/// Both the connect and the round-trip are individually bounded so an
/// unresponsive/wedged daemon cannot hang the CLI. Errors when the daemon is
/// unreachable (socket absent / not listening) or the response is malformed.
async fn query_daemon_status(socket_path: &Path) -> Result<DaemonStatusReport> {
    const TIMEOUT: Duration = Duration::from_secs(5);

    let stream = tokio::time::timeout(TIMEOUT, UnixStream::connect(socket_path))
        .await
        .map_err(|_| anyhow!("connect timed out after {}s", TIMEOUT.as_secs()))?
        .map_err(|e| anyhow!("connect failed: {e}"))?;
    let (reader, mut writer) = stream.into_split();

    let roundtrip = async move {
        let request_json = serde_json::to_string(&Request::DaemonStatus)?;
        writer.write_all(request_json.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;

        let mut lines = BufReader::new(reader).lines();
        let line = lines
            .next_line()
            .await?
            .ok_or_else(|| anyhow!("daemon closed the connection without responding"))?;
        let response: Response = serde_json::from_str(&line)?;
        match response {
            Response::DaemonStatus(report) => Ok(report),
            Response::Error { message } => Err(anyhow!("daemon error: {message}")),
            other => Err(anyhow!("unexpected response: {other:?}")),
        }
    };

    tokio::time::timeout(TIMEOUT, roundtrip)
        .await
        .map_err(|_| anyhow!("status round-trip timed out after {}s", TIMEOUT.as_secs()))?
}

/// Handle the `quarantine` subcommand (Issue #3939). Connects to the running
/// daemon over its Unix socket and dispatches the requested action. The
/// quarantine state is in the daemon's memory, so — unlike `workspace` — this
/// cannot operate on a file when the daemon is down.
async fn handle_quarantine_command(action: QuarantineAction) -> Result<()> {
    let socket_path = resolve_socket_path()?;
    match action {
        QuarantineAction::Clear {
            issue,
            workspace_root,
        } => {
            let request = Request::ClearQuarantine {
                issue,
                workspace_root,
            };
            match query_daemon(&socket_path, &request).await {
                Ok(Response::QuarantineCleared {
                    issue,
                    was_quarantined,
                }) => {
                    if was_quarantined {
                        println!(
                            "Cleared quarantine for issue #{issue} — it will re-qualify for \
                             dispatch and `loom:issue` has been restored on the forge."
                        );
                    } else {
                        println!("Issue #{issue} was not quarantined — nothing to clear (no-op).");
                    }
                    Ok(())
                }
                Ok(Response::Error { message }) => {
                    eprintln!("Daemon error: {message}");
                    std::process::exit(1);
                }
                Ok(other) => {
                    eprintln!("Unexpected response from daemon: {other:?}");
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!("Could not reach loom-daemon at {}: {e}", socket_path.display());
                    eprintln!();
                    eprintln!("Is the daemon running? Start it with:");
                    eprintln!("  ./.loom/scripts/cli/loom-daemon-start.sh");
                    std::process::exit(1);
                }
            }
        }
    }
}

/// Handle the `watch` subcommand (Issue #3971). Connects to the running daemon
/// over its Unix socket and registers/lists/removes durable watches. The watches
/// are persisted machine-level (`~/.loom/watches.json`), so — like the daemon's
/// other file-backed state — they survive both this shell and a daemon restart;
/// the resolution report lands in `~/.loom/logs/watch-results.log`.
async fn handle_watch_command(action: WatchAction) -> Result<()> {
    use loom_daemon::watch_registry::WatchKind;

    let socket_path = resolve_socket_path()?;
    // `json_out` only applies to `list`; captured here so the request build below
    // does not have to smuggle it back out.
    let mut json_out = false;
    let request = match action {
        WatchAction::Add {
            number,
            pr,
            repo,
            workspace_root,
            note,
        } => Request::RegisterWatch {
            kind: if pr { WatchKind::Pr } else { WatchKind::Issue },
            number,
            repo,
            workspace_root,
            note,
        },
        WatchAction::List { json } => {
            json_out = json;
            Request::ListWatches
        }
        WatchAction::Remove { id } => Request::RemoveWatch { id },
    };

    match query_daemon(&socket_path, &request).await {
        Ok(Response::WatchRegistered {
            watch,
            already_present,
        }) => {
            if already_present {
                println!("Already watching {} (id {}) — no-op.", watch.target_label(), watch.id);
            } else {
                println!("Registered watch on {}", watch.target_label());
                println!("  id:   {}", watch.id);
                println!("Terminal state will be recorded to {}", watch_results_log_hint());
            }
            Ok(())
        }
        Ok(Response::WatchList { watches }) => {
            if json_out {
                println!("{}", serde_json::to_string_pretty(&watches)?);
            } else if watches.is_empty() {
                println!("No durable watches registered.");
            } else {
                println!("{} durable watch(es):", watches.len());
                for w in &watches {
                    let note = w
                        .note
                        .as_deref()
                        .map(|n| format!(" — {n}"))
                        .unwrap_or_default();
                    println!("  {}  {}{}", w.id, w.target_label(), note);
                }
                println!();
                println!("Resolutions are recorded to {}", watch_results_log_hint());
            }
            Ok(())
        }
        Ok(Response::WatchRemoved { id, was_present }) => {
            if was_present {
                println!("Removed watch {id}.");
            } else {
                println!("No watch with id {id} — nothing to remove (no-op).");
            }
            Ok(())
        }
        Ok(Response::Error { message }) => {
            eprintln!("Daemon error: {message}");
            std::process::exit(1);
        }
        Ok(other) => {
            eprintln!("Unexpected response from daemon: {other:?}");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("Could not reach loom-daemon at {}: {e}", socket_path.display());
            eprintln!();
            eprintln!("Is the daemon running? Start it with:");
            eprintln!("  ./.loom/scripts/cli/loom-daemon-start.sh");
            std::process::exit(1);
        }
    }
}

/// Best-effort human hint for where watch results are recorded (honors the env
/// override). Purely cosmetic — never fails.
fn watch_results_log_hint() -> String {
    watch_registry::default_results_log_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "~/.loom/logs/watch-results.log".to_string())
}

/// Connect to the running daemon over its Unix socket, send a single `request`,
/// and return the parsed `Response`. Both the connect and the round-trip are
/// individually bounded so an unresponsive/wedged daemon cannot hang the CLI.
/// Mirrors `query_daemon_status` but for arbitrary single-frame requests.
async fn query_daemon(socket_path: &Path, request: &Request) -> Result<Response> {
    query_daemon_bounded(socket_path, request, Duration::from_secs(5)).await
}

/// Like [`query_daemon`] but with a caller-supplied bound on both the connect
/// and the round-trip (Issue #3952). Extracted so the `dispatch` subcommand can
/// name its own ack budget and so the timeout path is unit-testable against a
/// deliberately-unresponsive fake socket without a multi-second wait.
async fn query_daemon_bounded(
    socket_path: &Path,
    request: &Request,
    timeout: Duration,
) -> Result<Response> {
    let stream = tokio::time::timeout(timeout, UnixStream::connect(socket_path))
        .await
        .map_err(|_| anyhow!("connect timed out after {}s", timeout.as_secs()))?
        .map_err(|e| anyhow!("connect failed: {e}"))?;
    let (reader, mut writer) = stream.into_split();

    let request_json = serde_json::to_string(request)?;
    let roundtrip = async move {
        writer.write_all(request_json.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;

        let mut lines = BufReader::new(reader).lines();
        let line = lines
            .next_line()
            .await?
            .ok_or_else(|| anyhow!("daemon closed the connection without responding"))?;
        let response: Response = serde_json::from_str(&line)?;
        Ok::<Response, anyhow::Error>(response)
    };

    tokio::time::timeout(timeout, roundtrip)
        .await
        .map_err(|_| anyhow!("round-trip timed out after {}s", timeout.as_secs()))?
}

/// Default bounded ack budget for the `dispatch` subcommand (Issue #3952).
///
/// Dispatch is emphatically **not** an immediate ack: `SweepRegistry::dispatch()`
/// runs synchronously before replying, and its own documented internal budget for
/// a legitimate, successful dispatch is comfortably multi-second. It flips the
/// label via a blocking `gh issue edit` network round-trip, applies up to a 2s
/// dispatch stagger (`DEFAULT_DISPATCH_STAGGER_MS`) under concurrent dispatch,
/// and polls up to 5s (`TOKEN_NAME_CAPTURE_TIMEOUT`) for the child's account
/// name (an explicitly anticipated graceful-degradation window) before falling
/// back to `UNKNOWN_TOKEN_NAME`. A 5s client bound had essentially zero headroom
/// over that and would false-fail on a real success. We therefore mirror
/// `mcp-loom`'s `DISPATCH_TIMEOUT_MS` (`mcp-loom/src/tools/sweeps.ts`) of 30s for
/// the identical underlying IPC call: real margin over the worst case, while
/// still a bounded, finite value that never reproduces the ~1800s wedge of
/// #3945. On expiry the CLI exits nonzero with a clear "is loom-daemon running?"
/// message.
const DISPATCH_ACK_TIMEOUT: Duration = Duration::from_secs(30);

/// Env override for the dispatch ack budget, sharing the exact name
/// `mcp-loom` uses (`LOOM_DAEMON_IPC_TIMEOUT_MS`) so a single operator-facing
/// convention tunes the client-side IPC timeout across both surfaces.
const DAEMON_IPC_TIMEOUT_ENV: &str = "LOOM_DAEMON_IPC_TIMEOUT_MS";

/// Resolve the effective dispatch ack timeout.
///
/// Mirrors `mcp-loom`'s `Math.max(DISPATCH_TIMEOUT_MS, resolveDaemonIpcTimeoutMs())`
/// semantics for `dispatch_sweep`: a positive-integer-millisecond
/// `LOOM_DAEMON_IPC_TIMEOUT_MS` can only ever *raise* the bound above the 30s
/// floor (for a slow forge / heavily-loaded daemon), never lower it — lowering
/// it would reintroduce exactly the false-"did not ack" negative this widening
/// fixes. An absent, empty, non-numeric, zero, or negative value falls back to
/// the {@link DISPATCH_ACK_TIMEOUT} floor.
fn resolve_dispatch_ack_timeout() -> Duration {
    if let Ok(raw) = std::env::var(DAEMON_IPC_TIMEOUT_ENV) {
        if let Ok(ms) = raw.trim().parse::<u64>() {
            if ms > 0 {
                return Duration::from_millis(ms).max(DISPATCH_ACK_TIMEOUT);
            }
        }
    }
    DISPATCH_ACK_TIMEOUT
}

/// Build the `DispatchSweep` IPC request from the `dispatch` subcommand's args
/// (Issue #3952). Pure and side-effect-free so flag plumbing
/// (`--workspace`/`--model`/`--effort`/`--depends-on`) is unit-testable without
/// a socket. Mirrors the field mapping the `mcp__loom__dispatch_sweep` tool uses
/// so both operator surfaces enqueue byte-for-byte-equivalent requests.
fn build_dispatch_request(
    issue: u32,
    workspace: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    depends_on: Option<u32>,
) -> Request {
    Request::DispatchSweep {
        kind: SweepKind::Issue(issue),
        // The daemon derives its own idempotency key from the issue when this is
        // absent; an operator dispatching by hand has no key to supply.
        idempotency_key: None,
        model,
        effort,
        depends_on,
        workspace_root: workspace,
    }
}

/// Handle the `dispatch` subcommand (Issue #3952). Connects to the running
/// daemon over its Unix socket and enqueues a sweep via the same `DispatchSweep`
/// request the MCP `dispatch_sweep` tool uses — but with a bounded client-side
/// ack timeout so a wedged daemon can never hang the CLI (the #3945 failure
/// mode). On success prints the sweep id + per-sweep log path and exits 0.
async fn handle_dispatch_command(
    issue: u32,
    workspace: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    depends_on: Option<u32>,
) -> Result<()> {
    let socket_path = resolve_socket_path()?;
    let request = build_dispatch_request(issue, workspace, model, effort, depends_on);
    let ack_timeout = resolve_dispatch_ack_timeout();

    match query_daemon_bounded(&socket_path, &request, ack_timeout).await {
        Ok(Response::SweepDispatched {
            sweep_id,
            pid,
            token_name,
            log_path,
        }) => {
            println!("Dispatched sweep for issue #{issue}");
            println!("  sweep id:  {sweep_id}");
            println!("  pid:       {pid}");
            // Surface a degraded token-name capture distinctly: the daemon fell
            // back to `UNKNOWN_TOKEN_NAME` because the child hadn't logged its
            // account-selection line within the capture window — a hint the
            // dispatch was slow on the daemon side, not that it failed.
            if token_name == sweep_registry::UNKNOWN_TOKEN_NAME {
                println!("  token:     {token_name} (account not captured before ack — slow child startup)");
            } else {
                println!("  token:     {token_name}");
            }
            println!("  log path:  {}", log_path.display());
            println!();
            println!("Tail the sweep log with:");
            println!("  tail -f {}", log_path.display());
            Ok(())
        }
        Ok(Response::Error { message }) => {
            eprintln!("Daemon rejected the dispatch: {message}");
            std::process::exit(1);
        }
        Ok(Response::StructuredError(err)) => {
            eprintln!("Daemon rejected the dispatch: {}", err.message);
            std::process::exit(1);
        }
        Ok(other) => {
            eprintln!("Unexpected response from daemon: {other:?}");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!(
                "Daemon did not ack the dispatch within {}s ({e}) — is loom-daemon running?",
                ack_timeout.as_secs()
            );
            eprintln!();
            eprintln!("Start it with:");
            eprintln!("  ./.loom/scripts/cli/loom-daemon-start.sh");
            std::process::exit(1);
        }
    }
}

/// Collect per-token usage by shelling out to `loom-tokens check --json`,
/// mirroring `probe-tokens.sh`: prefer the `loom-tokens` binary on PATH, else
/// fall back to `python3 -m loom_tools.cli.loom_tokens`. Best-effort — returns
/// `None` on any failure (binary absent, non-zero exit, unparseable output) so
/// the status view still renders without the usage table.
fn collect_token_usage() -> Option<serde_json::Value> {
    let attempts: [(&str, &[&str]); 2] = [
        ("loom-tokens", &["check", "--json"]),
        ("python3", &["-m", "loom_tools.cli.loom_tokens", "check", "--json"]),
    ];
    for (bin, args) in attempts {
        let Ok(output) = Command::new(bin).args(args).output() else {
            continue;
        };
        if !output.status.success() {
            continue;
        }
        if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&output.stdout) {
            return Some(value);
        }
    }
    None
}

/// Handle the `status` subcommand — render the running daemon's autonomous-mode
/// operability snapshot (Issue #3891). Fetches the daemon-native part over IPC
/// and layers on the client-side per-token usage probe.
///
/// `pipeline` opts into the forge-side pipeline snapshot (Issue #3977): per
/// managed repo, open `loom:issue`/`loom:building` counts, open PR counts by
/// review-state label, and PRs merged in the last 24h. Like the per-token
/// usage table, this is collected client-side (several `gh` calls per repo)
/// rather than inside the IPC handler, and is opt-in specifically because
/// those extra forge calls are too slow to bundle into the default view.
async fn handle_status_command(json: bool, pipeline: bool) -> Result<()> {
    let socket_path = resolve_socket_path()?;

    let report = match query_daemon_status(&socket_path).await {
        Ok(report) => report,
        Err(e) => {
            if json {
                let err = serde_json::json!({
                    "error": format!(
                        "could not reach loom-daemon at {}: {e}",
                        socket_path.display()
                    ),
                });
                println!("{}", serde_json::to_string_pretty(&err)?);
            } else {
                eprintln!("Could not reach loom-daemon at {}: {e}", socket_path.display());
                eprintln!();
                eprintln!("Is the daemon running? Start it with:");
                eprintln!("  ./.loom/scripts/cli/loom-daemon-start.sh");
            }
            std::process::exit(1);
        }
    };

    // Per-token usage is a slow per-account network probe the daemon deliberately
    // does NOT perform inside the IPC handler; collect it client-side here.
    let token_usage = collect_token_usage();

    // Self-update staleness (#3968): purely local, read-only — compares the
    // commit baked into THIS `loom-daemon --status` binary against the source
    // checkout's current HEAD, when that checkout is still on this machine.
    // Advisory only; never triggers a rebuild or restart (see
    // `.loom/scripts/cli/loom-daemon-update.sh` for the opt-in update flow).
    let update = self_update::check();

    // Forge-side pipeline snapshot (#3977) — opt-in, fetched in the same
    // priority order as `report.per_repo` so the rendered table lines up with
    // the "Managed repos" dispatch table above it.
    let pipeline_snapshots = if pipeline {
        let roots: Vec<PathBuf> = report.per_repo.iter().map(|r| r.root.clone()).collect();
        let source = Arc::new(loom_daemon::pipeline_snapshot::GhPipelineSource::new());
        Some(loom_daemon::pipeline_snapshot::collect_pipeline_snapshots(source, roots).await)
    } else {
        None
    };

    if json {
        print_status_json(&report, token_usage.as_ref(), &update, pipeline_snapshots.as_deref())?;
    } else {
        print_status_human(&report, token_usage.as_ref(), &update, pipeline_snapshots.as_deref());
    }
    Ok(())
}

/// The capacity figures the whole status view shares, resolved from a single
/// source so the summary, the healthy-tokens cap input, and the per-token table
/// never contradict each other (issue #3936).
///
/// Preference order:
/// 1. **fresh probe** — when a client-side `loom-tokens check --json` succeeded
///    (the *same* data that renders the per-token table), the health counts are
///    derived from it via [`loom_daemon::capacity::summarize_probe`], applying
///    the near-ceiling threshold uniformly. This is the accurate *current*
///    capacity and matches the table row-for-row.
/// 2. **daemon ranking** — when no fresh probe is available but the daemon
///    reported a parsed `.loom/tokens/.ranking`, fall back to its snapshot.
/// 3. **raw pool** — no probe and no ranking: the pre-#3902 flat pool basis.
struct ResolvedCapacity {
    /// Where the figures came from — one of `"probe"`, `"ranking"`, `"pool"`.
    source: &'static str,
    /// Whether any account-health data (probe or ranking) was available.
    ranking_present: bool,
    total: usize,
    healthy: usize,
    exhausted: usize,
    /// Health-adjusted token axis (healthy accounts, or the raw pool as a
    /// fallback) — the "healthy tokens" input to the dynamic concurrency cap.
    token_axis_limit: usize,
    /// The effective dynamic cap consistent with `token_axis_limit`:
    /// `min(token_axis_limit, disk_headroom, cpu_headroom, configured_max)`
    /// (CPU term added in #3978).
    effective_cap: usize,
    /// Whether the token axis is the binding (minimum) constraint.
    token_bound: bool,
}

/// Resolve the shared capacity figures for a status render (#3936). Prefers the
/// fresh client-side probe over the daemon's possibly-stale ranking snapshot so
/// the summary count, the cap's healthy-tokens input, and the per-token table
/// all agree.
fn resolve_capacity(
    report: &DaemonStatusReport,
    token_usage: Option<&serde_json::Value>,
) -> ResolvedCapacity {
    // Tier 1: fresh probe — the same source as the per-token table.
    if let Some(usage) = token_usage {
        if let Some(accounts) = usage.get("accounts").and_then(serde_json::Value::as_array) {
            let pairs: Vec<(&str, Option<f64>)> = accounts
                .iter()
                .map(|a| {
                    let status = a
                        .get("status")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("?");
                    let util_7d = a.get("7d_utilization").and_then(serde_json::Value::as_f64);
                    (status, util_7d)
                })
                .collect();
            let cap = loom_daemon::capacity::summarize_probe(pairs.iter().copied());
            let token_axis_limit = cap.healthy;
            // The token axis of the cap is `healthy × per-token` (#3947); treat a
            // pre-#3947 wire `0` as the effective floor of 1.
            let factor = report.per_token_concurrency.max(1);
            let token_axis_effective = token_axis_limit.saturating_mul(factor);
            // The CPU term (#3978) is policy-floored at 1 by
            // `cpu_headroom::cpu_headroom` on every real computation, so a wire
            // value of exactly `0` unambiguously means "an older daemon that
            // predates #3978 didn't send this field" (`#[serde(default)]`) —
            // not a genuine zero-headroom reading. Treat it as unconstrained
            // rather than collapsing the cap to 0 against a daemon that never
            // computed a CPU term at all.
            let cpu_headroom = if report.cpu_headroom == 0 {
                usize::MAX
            } else {
                report.cpu_headroom
            };
            let effective_cap = token_axis_effective
                .min(report.disk_headroom)
                .min(cpu_headroom)
                .min(report.configured_max);
            let token_bound = token_axis_effective <= report.disk_headroom
                && token_axis_effective <= cpu_headroom
                && token_axis_effective <= report.configured_max;
            return ResolvedCapacity {
                source: "probe",
                ranking_present: true,
                total: cap.total,
                healthy: cap.healthy,
                exhausted: cap.exhausted,
                token_axis_limit,
                effective_cap,
                token_bound,
            };
        }
    }

    // Tier 2: daemon-reported ranking snapshot.
    if report.capacity.ranking_present {
        return ResolvedCapacity {
            source: "ranking",
            ranking_present: true,
            total: report.capacity.total_accounts,
            healthy: report.capacity.healthy_accounts,
            exhausted: report.capacity.exhausted_accounts,
            token_axis_limit: report.capacity.token_axis_limit,
            effective_cap: report.dynamic_cap,
            token_bound: report.capacity.token_bound,
        };
    }

    // Tier 3: no probe, no ranking — raw pool basis (pre-#3902 behavior).
    ResolvedCapacity {
        source: "pool",
        ranking_present: false,
        total: report.token_pool_size,
        healthy: 0,
        exhausted: 0,
        token_axis_limit: report.token_pool_size,
        effective_cap: report.dynamic_cap,
        token_bound: report.capacity.token_bound,
    }
}

/// Emit the combined status (daemon report + per-token usage) as JSON.
fn print_status_json(
    report: &DaemonStatusReport,
    token_usage: Option<&serde_json::Value>,
    update: &self_update::SelfUpdateStatus,
    pipeline: Option<&[loom_daemon::pipeline_snapshot::RepoPipelineSnapshot]>,
) -> Result<()> {
    let rc = resolve_capacity(report, token_usage);
    let combined = serde_json::json!({
        "in_flight_count": report.in_flight.len(),
        "in_flight": report.in_flight,
        "dynamic_cap": {
            "token_pool_size": report.token_pool_size,
            "disk_headroom": report.disk_headroom,
            // CPU/load headroom term (#3978) — see the field docs on
            // `DaemonStatusReport::cpu_headroom` for the pre-#3978 `0` ⇒
            // "field absent" wire-compat convention.
            "cpu_headroom": report.cpu_headroom,
            "logical_cpus": report.logical_cpus,
            "loadavg_1m": report.loadavg_1m,
            "configured_max": report.configured_max,
            "per_token_concurrency": report.per_token_concurrency.max(1),
            "token_axis_effective": rc.token_axis_limit.saturating_mul(report.per_token_concurrency.max(1)),
            "effective": rc.effective_cap,
        },
        "capacity": {
            "source": rc.source,
            "ranking_present": rc.ranking_present,
            "total_accounts": rc.total,
            "healthy_accounts": rc.healthy,
            "exhausted_accounts": rc.exhausted,
            "token_axis_limit": rc.token_axis_limit,
            "token_bound": rc.token_bound,
        },
        "main_health_gate": {
            "halted": report.main_health_gate_halted,
            // "Not evaluated" is distinct from "halted" (verified-red main) —
            // #3950 AC3. Both can be true at once: a prior halt from a
            // genuinely red run persists untouched while a later tick can't
            // even evaluate (dirty tree, timeout, missing tool, broken `git`).
            "not_evaluated": report.main_health_gate_not_evaluated,
            // Which failure class actually blocked evaluation (#3974 AC2).
            "not_evaluated_reason": report.main_health_gate_not_evaluated_reason,
        },
        // Per-repo breakdown across every registered managed workspace (#3930).
        "per_repo": report.per_repo.iter().map(|r| serde_json::json!({
            "root": r.root,
            "priority": r.priority,
            "in_flight_count": r.in_flight_count,
            "health_gate_halted": r.health_gate_halted,
            "health_gate_not_evaluated": r.health_gate_not_evaluated,
            "health_gate_not_evaluated_reason": r.health_gate_not_evaluated_reason,
        })).collect::<Vec<_>>(),
        // Forge-side pipeline snapshot (#3977) — present only when `--pipeline`
        // was passed; `null` otherwise so a consumer can tell "not requested"
        // apart from "requested but empty".
        "pipeline": pipeline.map(|snapshots| snapshots.iter().map(|s| serde_json::json!({
            "root": s.root,
            "queued": s.queued,
            "building": s.building,
            "review_requested": s.review_requested,
            "changes_requested": s.changes_requested,
            "approved": s.approved,
            "merged_24h": s.merged_24h,
            "error": s.error,
        })).collect::<Vec<_>>()),
        "token_usage": token_usage,
        // Self-update staleness (#3968) — read-only, local-only comparison of
        // this binary's baked-in commit vs. the source checkout's HEAD.
        "self_update": {
            "built_commit": update.built_commit,
            "source_commit": update.source_commit,
            "update_available": update.update_available,
        },
    });
    println!("{}", serde_json::to_string_pretty(&combined)?);
    Ok(())
}

/// Render the main-health gate summary line for `loom-daemon status`.
///
/// `halted` means a gate run **completed** and found `main` verified-red — the
/// only state that pauses dispatch. `not_evaluated` means the most recent tick
/// could not produce a verdict at all; `reason` (`"<class>: <detail>"`, #3974
/// AC2) names *why*. Before #3974 this line asserted "workspace tree is dirty"
/// for every skip, which reported a clean tree as dirty whenever the real cause
/// was a timeout, a missing build tool, or a broken `git`.
fn format_gate_status(halted: bool, not_evaluated: bool, reason: Option<&str>) -> String {
    let cause =
        reason.map_or_else(|| "cause unrecorded".to_string(), std::string::ToString::to_string);
    match (halted, not_evaluated) {
        (true, true) => format!(
            "HALTED (main verified red — new dispatch paused) + NOT EVALUATED ({cause}) — \
             the gate cannot currently confirm main is still red, or check for recovery"
        ),
        (true, false) => {
            "HALTED (main verified red — new dispatch paused; in-flight sweeps keep running)"
                .to_string()
        }
        (false, true) => format!(
            "not evaluated ({cause}) — the gate could not run, which is NOT evidence about \
             main; dispatch is NOT halted by this"
        ),
        (false, false) => "clear (dispatch allowed)".to_string(),
    }
}

/// Emit the combined status as a human-readable table.
fn print_status_human(
    report: &DaemonStatusReport,
    token_usage: Option<&serde_json::Value>,
    update: &self_update::SelfUpdateStatus,
    pipeline: Option<&[loom_daemon::pipeline_snapshot::RepoPipelineSnapshot]>,
) {
    println!("\n=== Loom Autonomous Daemon Status ===\n");

    println!("In-flight sweeps: {}", report.in_flight.len());
    if report.in_flight.is_empty() {
        println!("  (none)");
    } else {
        println!("  {:<30} {:>7} {:>8}  {:<20} PHASE", "SWEEP", "ISSUE", "PID", "TOKEN");
        println!("  {:-<75}", "");
        for s in &report.in_flight {
            let issue = match &s.kind {
                SweepKind::Issue(n) => format!("#{n}"),
                SweepKind::PrSet(_) => "prs".to_string(),
            };
            let phase = s.latest_phase.as_deref().unwrap_or("-");
            println!(
                "  {:<30} {:>7} {:>8}  {:<20} {}",
                s.sweep_id, issue, s.pid, s.token_name, phase
            );
        }
    }

    // Capacity figures resolved from a single source (fresh probe when
    // available, else the daemon's ranking snapshot) so the cap's healthy-tokens
    // input, the Token-capacity summary, and the Per-token table all agree (#3936).
    let rc = resolve_capacity(report, token_usage);

    let factor = report.per_token_concurrency.max(1);
    println!("\nDynamic concurrency cap: {}", rc.effective_cap);
    println!(
        "  = min(healthy {} × per-token {} = {}, disk headroom {}, cpu headroom {}, \
         configured max {})",
        rc.token_axis_limit,
        factor,
        rc.token_axis_limit.saturating_mul(factor),
        report.disk_headroom,
        report.cpu_headroom,
        report.configured_max
    );
    // CPU/load headroom detail (#3978 AC4: "status shows current
    // loadavg/CPU headroom next to disk headroom"). `cpu_headroom == 0` with
    // `logical_cpus == 0` means an older daemon (pre-#3978) never sent these
    // fields — nothing to show.
    if report.logical_cpus > 0 {
        match report.loadavg_1m {
            Some(load) => println!(
                "  cpu headroom: {} concurrent-sweep slot(s) ({} logical cores, 1m loadavg {load:.2})",
                report.cpu_headroom, report.logical_cpus
            ),
            None => println!(
                "  cpu headroom: {} concurrent-sweep slot(s) ({} logical cores, loadavg \
                 unavailable on this platform — static capacity only)",
                report.cpu_headroom, report.logical_cpus
            ),
        }
    }

    // Token-capacity backpressure section (#3902, source-unified in #3936).
    println!("\nToken capacity:");
    if rc.ranking_present {
        let src = if rc.source == "probe" {
            "live probe: loom-tokens check --json"
        } else {
            "from .loom/tokens/.ranking"
        };
        println!(
            "  {}/{} accounts healthy, {} exhausted/near-ceiling ({src})",
            rc.healthy, rc.total, rc.exhausted
        );
        // When a fresh probe disagrees with the daemon's ranking-based cap, the
        // ranking is stale — the daemon may still be dispatching against the old
        // (higher) count. Surface it so the operator re-probes (#3936).
        if rc.source == "probe"
            && report.capacity.ranking_present
            && report.capacity.healthy_accounts != rc.healthy
        {
            println!(
                "  note: daemon dispatch cap still uses a stale .ranking ({} healthy); \
                 refresh it with `loom-tokens check --ranking`.",
                report.capacity.healthy_accounts
            );
        }
        if rc.token_bound {
            if rc.healthy == 0 {
                println!(
                    "  token-bound: NO healthy accounts — new dispatch deferred until capacity \
                     returns. Add accounts (~/.claude-monitor/accounts.env + `loom-tokens \
                     bootstrap`) or buy API credits, then `loom-tokens check --ranking`."
                );
            } else {
                println!(
                    "  token-bound: tokens are the binding constraint on throughput. Add accounts \
                     or API credits to dispatch more concurrently."
                );
            }
        } else {
            println!("  not token-bound (tokens are not the current bottleneck)");
        }
    } else {
        println!(
            "  (no ranking — run `loom-tokens check --ranking`; token pool size {} used as the \
             health basis)",
            report.token_pool_size
        );
    }

    // "Halted" (a completed gate run found main verified-red) and "not
    // evaluated" (the gate could not run this tick) are distinct states that can
    // co-occur (#3950 AC3): a prior halt persists untouched while an
    // environmental failure blocks the *next* evaluation. The not-evaluated
    // cause is reported verbatim from the gate (#3974 AC2) — pre-#3974 this
    // line hard-coded "workspace tree is dirty" for every skip, which
    // misreported timeouts / missing tools / broken `git` as a dirty tree.
    let gate = format_gate_status(
        report.main_health_gate_halted,
        report.main_health_gate_not_evaluated,
        report.main_health_gate_not_evaluated_reason.as_deref(),
    );
    println!("\nMain-health gate: {gate}");

    // Per-repo breakdown across every registered managed workspace (#3930). In
    // the common single-workspace case this is one line for the daemon's own
    // workspace; with `loom-daemon workspace add <path>` it lists every managed
    // repo, its in-flight count, and its own gate state.
    println!(
        "\nManaged repos: {} (priority: lower = higher dispatch priority)",
        report.per_repo.len()
    );
    if report.per_repo.is_empty() {
        println!("  (none)");
    } else {
        println!("  {:>4}  {:>9}  {:<13}  REPO", "PRIO", "IN-FLIGHT", "GATE");
        println!("  {:-<60}", "");
        for r in &report.per_repo {
            // Same halted/not-evaluated distinction as the top-level summary
            // above, condensed for the table column (#3950 AC3).
            let gate = match (r.health_gate_halted, r.health_gate_not_evaluated) {
                (true, true) => "HALTED+UNEVAL",
                (true, false) => "HALTED",
                (false, true) => "not-evaluated",
                (false, false) => "clear",
            };
            println!(
                "  {:>4}  {:>9}  {:<13}  {}",
                r.priority,
                r.in_flight_count,
                gate,
                r.root.display()
            );
            // Name the failure class behind a not-evaluated repo (#3974 AC2) so
            // the operator can tell "dirty tree" from "cargo not on PATH".
            if let Some(reason) = &r.health_gate_not_evaluated_reason {
                println!("        gate not evaluated — {reason}");
            }
            // Insta-crash quarantine (#3939): list the issues this repo is
            // currently refusing to re-dispatch so a stalled-but-nonempty backlog
            // is explained. Auto-releases on a TTL (or `loom:blocked` removal).
            if !r.quarantined_issues.is_empty() {
                let list = r
                    .quarantined_issues
                    .iter()
                    .map(|n| format!("#{n}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                println!("        quarantined (insta-crash, #3939): {list}");
            }
        }
    }

    // Forge-side pipeline snapshot (#3977) — opt-in via `--pipeline`. Rendered
    // in the same order as the "Managed repos" table above (both iterate
    // `report.per_repo`), so the two tables line up row-for-row.
    if let Some(snapshots) = pipeline {
        println!("\nForge pipeline (per repo, --pipeline):");
        if snapshots.is_empty() {
            println!("  (none)");
        } else {
            println!(
                "  {:>6}  {:>8}  {:>6}  {:>7}  {:>5}  {:>9}  REPO",
                "QUEUED", "BUILDING", "REVIEW", "CHNG-RQ", "PR", "MERGED24H"
            );
            println!("  {:-<75}", "");
            for s in snapshots {
                use loom_daemon::pipeline_snapshot::format_count;
                println!(
                    "  {:>6}  {:>8}  {:>6}  {:>7}  {:>5}  {:>9}  {}",
                    format_count(s.queued),
                    format_count(s.building),
                    format_count(s.review_requested),
                    format_count(s.changes_requested),
                    format_count(s.approved),
                    format_count(s.merged_24h),
                    s.root.display()
                );
                if let Some(err) = &s.error {
                    println!("        forge query failed for one or more metrics ({err}) — unreachable fields shown as ?");
                }
            }
        }
    }

    println!("\nPer-token usage:");
    match token_usage {
        Some(value) => print_token_usage_table(value),
        None => println!(
            "  (unavailable — `loom-tokens check --json` failed or the token pool is not bootstrapped)"
        ),
    }

    // Self-update staleness (#3968) — read-only, local-only. Never implies an
    // auto-restart; run `.loom/scripts/cli/loom-daemon-update.sh` to act on it.
    print!("\nSelf-update: built from {}", update.built_commit);
    match (update.source_commit.as_deref(), update.update_available) {
        (Some(source), Some(true)) => println!(
            " — UPDATE AVAILABLE (source checkout HEAD is {source}); run \
             `./.loom/scripts/cli/loom-daemon-update.sh` to rebuild + provision + restart"
        ),
        (Some(source), Some(false)) => println!(" — up to date with source HEAD ({source})"),
        _ => println!(" (source checkout not found on this machine; staleness unknown)"),
    }

    println!();
}

/// Render the `loom-tokens check --json` report (`{ "accounts": [ { name,
/// status, 5h_utilization, 7d_utilization, 7d_reset } ] }`) as a small table.
/// Falls back to pretty-printed JSON if the shape is unexpected.
fn print_token_usage_table(value: &serde_json::Value) {
    let Some(accounts) = value.get("accounts").and_then(serde_json::Value::as_array) else {
        // Unexpected shape — surface the raw JSON rather than dropping it.
        if let Ok(pretty) = serde_json::to_string_pretty(value) {
            for line in pretty.lines() {
                println!("  {line}");
            }
        }
        return;
    };

    if accounts.is_empty() {
        println!("  (no accounts probed)");
        return;
    }

    println!("  {:<22} {:<14} {:>8} {:>8}", "ACCOUNT", "STATUS", "5h", "7d");
    println!("  {:-<54}", "");
    for acct in accounts {
        let name = acct
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("?");
        let raw_status = acct
            .get("status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("?");
        let util_7d = acct
            .get("7d_utilization")
            .and_then(serde_json::Value::as_f64);
        // Apply the same near-ceiling override the summary uses so a 99%-7d
        // `available` row never renders `available` here (#3936).
        let status = loom_daemon::capacity::effective_probe_status(raw_status, util_7d);
        let fmt_pct = |key: &str| {
            acct.get(key)
                .and_then(serde_json::Value::as_f64)
                .map_or_else(|| "-".to_string(), |u| format!("{:.0}%", u * 100.0))
        };
        println!(
            "  {:<22} {:<14} {:>8} {:>8}",
            name,
            status,
            fmt_pct("5h_utilization"),
            fmt_pct("7d_utilization")
        );
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

/// Handle the `workspace` subcommand — mutate/inspect the machine-level
/// workspace registry (`~/.loom/workspaces.json`) directly on the filesystem.
/// This runs whether or not the daemon is up; a running daemon re-reads the
/// same file on its next tick (hot-apply), and its `RegisterWorkspace` /
/// `DeregisterWorkspace` / `ListWorkspaces` IPC handlers touch the same file.
fn handle_workspace_command(action: WorkspaceAction) -> Result<()> {
    use loom_daemon::workspace_registry::{AddOutcome, WorkspaceRegistry};

    let path = loom_daemon::workspace_registry::default_registry_path()?;

    match action {
        WorkspaceAction::Add {
            path: repo_path,
            priority,
            config_overrides,
        } => {
            let overrides = match config_overrides {
                Some(raw) => Some(
                    serde_json::from_str::<serde_json::Value>(&raw)
                        .map_err(|e| anyhow!("--config-overrides is not valid JSON: {e}"))?,
                ),
                None => None,
            };
            let mut registry = WorkspaceRegistry::load(&path)?;
            match registry.add_with_priority(
                std::path::Path::new(&repo_path),
                overrides,
                priority,
            )? {
                AddOutcome::AlreadyPresent { canonical } => {
                    println!("Already registered: {}", canonical.display());
                    println!(
                        "  (priority unchanged — use `loom-daemon workspace set-priority {} <N>` \
                         to retier it)",
                        canonical.display()
                    );
                }
                AddOutcome::Added {
                    canonical,
                    looks_like_workspace,
                } => {
                    registry.save(&path)?;
                    println!("Registered workspace: {} (priority {priority})", canonical.display());
                    if !looks_like_workspace {
                        eprintln!(
                            "  warning: {} has no .git or .loom — register it anyway, but confirm \
                             it is a Loom-managed repo (run `loom-daemon init` there if not).",
                            canonical.display()
                        );
                    }
                }
            }
            Ok(())
        }
        WorkspaceAction::SetPriority {
            path: repo_path,
            priority,
        } => {
            let mut registry = WorkspaceRegistry::load(&path)?;
            if registry.set_priority(std::path::Path::new(&repo_path), priority) {
                registry.save(&path)?;
                println!("Set priority of {repo_path} to {priority}");
            } else {
                eprintln!(
                    "Not registered (no-op): {repo_path}\n  Register it first with \
                     `loom-daemon workspace add {repo_path} --priority {priority}`."
                );
            }
            Ok(())
        }
        WorkspaceAction::Remove { path: repo_path } => {
            let mut registry = WorkspaceRegistry::load(&path)?;
            if registry.remove(std::path::Path::new(&repo_path)) {
                registry.save(&path)?;
                println!("Deregistered workspace: {repo_path}");
            } else {
                println!("Not registered (no-op): {repo_path}");
            }
            Ok(())
        }
        WorkspaceAction::List { json } => {
            let registry = WorkspaceRegistry::load(&path)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&registry)?);
            } else if registry.workspaces.is_empty() {
                println!("No managed workspaces registered.");
                println!("Registry file: {}", path.display());
                println!("\nAdd one with:  loom-daemon workspace add <repo-path>");
            } else {
                println!("Managed workspaces ({}):", registry.workspaces.len());
                println!("Registry file: {}", path.display());
                println!("(priority: lower = higher dispatch priority; default 100)\n");
                println!("  {:>4}  WORKSPACE", "PRIO");
                println!("  {:-<60}", "");
                // Display in dispatch-priority order (#3946) — the same order the
                // autonomous loops drain — without mutating stored insertion order.
                let mut ordered: Vec<&_> = registry.workspaces.iter().collect();
                ordered.sort_by(|a, b| {
                    a.priority
                        .cmp(&b.priority)
                        .then_with(|| a.root.cmp(&b.root))
                });
                for ws in ordered {
                    let overrides = if ws.config_overrides.is_some() {
                        " (has config overrides)"
                    } else {
                        ""
                    };
                    println!("  {:>4}  {}{overrides}", ws.priority, ws.root.display());
                }
            }
            Ok(())
        }
    }
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

#[cfg(test)]
mod dispatch_tests {
    //! Tests for the `loom-daemon dispatch` subcommand (Issue #3952): flag
    //! plumbing into the `DispatchSweep` IPC request, a successful round-trip
    //! against a fake daemon, and the bounded-timeout path against a
    //! deliberately-unresponsive socket (the #3945 wedge must never hang).
    use super::{
        build_dispatch_request, format_gate_status, query_daemon_bounded,
        resolve_dispatch_ack_timeout, DAEMON_IPC_TIMEOUT_ENV, DISPATCH_ACK_TIMEOUT,
    };
    use loom_daemon::types::{Request, Response, SweepKind};
    use serial_test::serial;
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    /// `build_dispatch_request` maps every flag into the right `DispatchSweep`
    /// field — the core plumbing acceptance criterion.
    #[test]
    fn build_dispatch_request_plumbs_all_flags() {
        let request = build_dispatch_request(
            3952,
            Some("/some/repo".to_string()),
            Some("sonnet".to_string()),
            Some("high".to_string()),
            Some(3945),
        );
        match request {
            Request::DispatchSweep {
                kind,
                idempotency_key,
                model,
                effort,
                depends_on,
                workspace_root,
            } => {
                assert_eq!(kind, SweepKind::Issue(3952));
                assert_eq!(idempotency_key, None);
                assert_eq!(model.as_deref(), Some("sonnet"));
                assert_eq!(effort.as_deref(), Some("high"));
                assert_eq!(depends_on, Some(3945));
                assert_eq!(workspace_root.as_deref(), Some("/some/repo"));
            }
            other => panic!("expected DispatchSweep, got {other:?}"),
        }
    }

    /// With no optional flags the request carries only the issue kind and leaves
    /// every override `None`, so the daemon applies its own defaults.
    #[test]
    fn build_dispatch_request_defaults_are_none() {
        let request = build_dispatch_request(42, None, None, None, None);
        match request {
            Request::DispatchSweep {
                kind,
                model,
                effort,
                depends_on,
                workspace_root,
                ..
            } => {
                assert_eq!(kind, SweepKind::Issue(42));
                assert!(model.is_none());
                assert!(effort.is_none());
                assert!(depends_on.is_none());
                assert!(workspace_root.is_none());
            }
            other => panic!("expected DispatchSweep, got {other:?}"),
        }
    }

    /// A fake daemon that accepts one connection, verifies the received request
    /// is the expected `DispatchSweep`, and replies with `SweepDispatched`. This
    /// exercises the full client round-trip: request serialization + flag
    /// plumbing on the wire + response parsing.
    #[tokio::test]
    async fn round_trip_parses_dispatched_response_and_forwards_flags() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind");

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let (reader, mut writer) = stream.into_split();
            let mut lines = BufReader::new(reader).lines();
            let line = lines.next_line().await.expect("read").expect("line");
            let request: Request = serde_json::from_str(&line).expect("parse request");
            // Assert the flags arrived intact on the wire.
            match request {
                Request::DispatchSweep {
                    kind,
                    model,
                    effort,
                    depends_on,
                    workspace_root,
                    ..
                } => {
                    assert_eq!(kind, SweepKind::Issue(3952));
                    assert_eq!(model.as_deref(), Some("sonnet"));
                    assert_eq!(effort.as_deref(), Some("high"));
                    assert_eq!(depends_on, Some(3945));
                    assert_eq!(workspace_root.as_deref(), Some("/repo"));
                }
                other => panic!("expected DispatchSweep, got {other:?}"),
            }
            let response = Response::SweepDispatched {
                sweep_id: "sweep-abc".to_string(),
                pid: 12345,
                token_name: "agent-2.token".to_string(),
                log_path: std::path::PathBuf::from(".loom/logs/sweep-issue-3952.log"),
            };
            let json = serde_json::to_string(&response).expect("serialize");
            writer.write_all(json.as_bytes()).await.expect("write");
            writer.write_all(b"\n").await.expect("newline");
            writer.flush().await.expect("flush");
        });

        let request = build_dispatch_request(
            3952,
            Some("/repo".to_string()),
            Some("sonnet".to_string()),
            Some("high".to_string()),
            Some(3945),
        );
        let response = query_daemon_bounded(&socket_path, &request, Duration::from_secs(5))
            .await
            .expect("round-trip ok");

        match response {
            Response::SweepDispatched {
                sweep_id,
                pid,
                token_name,
                log_path,
            } => {
                assert_eq!(sweep_id, "sweep-abc");
                assert_eq!(pid, 12345);
                assert_eq!(token_name, "agent-2.token");
                assert_eq!(log_path, std::path::PathBuf::from(".loom/logs/sweep-issue-3952.log"));
            }
            other => panic!("expected SweepDispatched, got {other:?}"),
        }

        server.await.expect("server task");
    }

    /// An unresponsive daemon — accepts the connection but never replies —
    /// must trip the bounded round-trip timeout rather than hang (the #3945
    /// failure mode). The client returns an `Err` well before any 1800s wedge.
    #[tokio::test]
    async fn unresponsive_socket_times_out() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind");

        // Accept the connection but deliberately never write a response, holding
        // the stream open so the client's read blocks until its own timeout.
        let server = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.expect("accept");
            tokio::time::sleep(Duration::from_secs(30)).await;
        });

        let request = build_dispatch_request(3952, None, None, None, None);
        let started = std::time::Instant::now();
        let result = query_daemon_bounded(&socket_path, &request, Duration::from_millis(200)).await;
        let elapsed = started.elapsed();

        assert!(result.is_err(), "expected a timeout error, got {result:?}");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("timed out"), "error should mention the timeout, got: {msg}");
        // It must return promptly (well under a second), never hang.
        assert!(elapsed < Duration::from_secs(2), "timeout took too long: {elapsed:?}");

        server.abort();
    }

    /// A missing socket (no daemon listening at all) surfaces a connect error
    /// promptly — the operator's "is the daemon running?" case.
    #[tokio::test]
    async fn absent_socket_errors_fast() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("nonexistent.sock");

        let request = build_dispatch_request(3952, None, None, None, None);
        let result = query_daemon_bounded(&socket_path, &request, Duration::from_millis(500)).await;

        assert!(result.is_err(), "expected a connect error, got {result:?}");
    }

    /// The documented middle case (issue #3952 review): a **legitimate, slow**
    /// dispatch. The daemon does real synchronous work before acking — a
    /// `gh issue edit` label flip, up to a 2s dispatch stagger, and up to a 5s
    /// token-name capture window — so a successful ack can land well past the
    /// old hardcoded 5s client bound. This fake daemon sleeps *past* that old
    /// 5s bound before replying `SweepDispatched`, and the CLI's real default
    /// budget ([`DISPATCH_ACK_TIMEOUT`], 30s) must still parse it as the success
    /// it is — never misreport a real dispatch as "did not ack". Neither the
    /// instant-ack round-trip nor the permanently-unresponsive stub exercises
    /// this path.
    #[tokio::test]
    async fn slow_but_legitimate_ack_succeeds() {
        // Sleep beyond the *old* 5s hardcoded bound so this test would have
        // false-failed before the widening, proving the regression is fixed.
        const SLOW_ACK: Duration = Duration::from_millis(5_500);

        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind");

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let (reader, mut writer) = stream.into_split();
            let mut lines = BufReader::new(reader).lines();
            let _ = lines.next_line().await.expect("read").expect("line");
            // Model the daemon's synchronous pre-ack work (label flip + stagger +
            // token-name poll) taking longer than the retired 5s client bound.
            tokio::time::sleep(SLOW_ACK).await;
            let response = Response::SweepDispatched {
                sweep_id: "sweep-slow".to_string(),
                pid: 4242,
                token_name: "agent-1.token".to_string(),
                log_path: std::path::PathBuf::from(".loom/logs/sweep-issue-3952.log"),
            };
            let json = serde_json::to_string(&response).expect("serialize");
            writer.write_all(json.as_bytes()).await.expect("write");
            writer.write_all(b"\n").await.expect("newline");
            writer.flush().await.expect("flush");
        });

        let request = build_dispatch_request(3952, None, None, None, None);
        let started = std::time::Instant::now();
        // Use the CLI's real default budget — the exact value a plain
        // `loom-daemon dispatch` resolves to with no env override.
        let result = query_daemon_bounded(&socket_path, &request, DISPATCH_ACK_TIMEOUT).await;
        let elapsed = started.elapsed();

        match result {
            Ok(Response::SweepDispatched { sweep_id, .. }) => {
                assert_eq!(sweep_id, "sweep-slow");
            }
            other => panic!("expected a slow-but-successful SweepDispatched, got {other:?}"),
        }
        // The daemon genuinely took longer than the retired 5s bound — this
        // asserts the test exercised the slow path rather than acking instantly.
        assert!(
            elapsed >= Duration::from_secs(5),
            "slow-ack test should have waited past the old 5s bound, took: {elapsed:?}"
        );

        server.await.expect("server task");
    }

    /// The 30s floor is the default when the override env var is absent — real
    /// headroom over the daemon's documented worst-case internal dispatch budget.
    #[test]
    #[serial]
    fn resolve_dispatch_ack_timeout_defaults_to_floor() {
        std::env::remove_var(DAEMON_IPC_TIMEOUT_ENV);
        assert_eq!(resolve_dispatch_ack_timeout(), DISPATCH_ACK_TIMEOUT);
        assert_eq!(DISPATCH_ACK_TIMEOUT, Duration::from_secs(30));
    }

    /// `LOOM_DAEMON_IPC_TIMEOUT_MS` mirrors `mcp-loom`'s `Math.max` semantics:
    /// it can only *raise* the bound above the 30s floor (never lower it, which
    /// would reintroduce the false-negative), and any invalid / non-positive
    /// value falls back to the floor. A single test owns this env var so its
    /// mutation never races a parallel reader.
    #[test]
    #[serial]
    fn resolve_dispatch_ack_timeout_env_raises_only() {
        // Above the floor → raised.
        std::env::set_var(DAEMON_IPC_TIMEOUT_ENV, "60000");
        assert_eq!(resolve_dispatch_ack_timeout(), Duration::from_secs(60));

        // Below the floor → clamped up to the 30s floor (never lowered).
        std::env::set_var(DAEMON_IPC_TIMEOUT_ENV, "1000");
        assert_eq!(resolve_dispatch_ack_timeout(), DISPATCH_ACK_TIMEOUT);

        // Zero / negative / non-numeric / empty → floor.
        for bad in ["0", "-5", "garbage", "", "  "] {
            std::env::set_var(DAEMON_IPC_TIMEOUT_ENV, bad);
            assert_eq!(
                resolve_dispatch_ack_timeout(),
                DISPATCH_ACK_TIMEOUT,
                "value {bad:?} should fall back to the floor"
            );
        }

        std::env::remove_var(DAEMON_IPC_TIMEOUT_ENV);
    }

    // ===================================================================
    // Main-health gate status line (#3950 AC3 shape, #3974 AC2 cause)
    // ===================================================================

    #[test]
    fn format_gate_status_names_the_actual_not_evaluated_cause() {
        // Pre-#3974 this line asserted "workspace tree is dirty" for EVERY
        // skip, so a `git fetch` failure on a completely clean tree was
        // reported as a dirty tree. The cause is now passed through verbatim.
        let s = format_gate_status(
            false,
            true,
            Some("git-failure: `git -C /repo fetch origin main` failed (exit 128)"),
        );
        assert!(s.contains("git-failure"), "got: {s}");
        assert!(s.contains("exit 128"), "got: {s}");
        assert!(!s.contains("dirty"), "must not assume a dirty tree: {s}");
        assert!(s.contains("NOT evidence about"), "got: {s}");
        assert!(s.contains("NOT halted"), "an unevaluated gate does not halt: {s}");

        // A dirty tree still reads as a dirty tree — because the gate said so.
        let s = format_gate_status(false, true, Some("dirty-tree: [ M src/main.rs]"));
        assert!(s.contains("dirty-tree"), "got: {s}");
        assert!(s.contains("src/main.rs"), "got: {s}");
    }

    #[test]
    fn format_gate_status_covers_all_four_states() {
        assert_eq!(format_gate_status(false, false, None), "clear (dispatch allowed)");

        let halted = format_gate_status(true, false, None);
        assert!(halted.starts_with("HALTED"), "got: {halted}");
        assert!(halted.contains("verified red"), "got: {halted}");

        // Both at once: a prior verified-red halt persists while the next tick
        // cannot evaluate.
        let both = format_gate_status(true, true, Some("timeout: gate command timed out"));
        assert!(both.contains("HALTED"), "got: {both}");
        assert!(both.contains("NOT EVALUATED"), "got: {both}");
        assert!(both.contains("timeout"), "got: {both}");

        // A missing cause degrades gracefully rather than inventing one.
        let no_cause = format_gate_status(false, true, None);
        assert!(no_cause.contains("cause unrecorded"), "got: {no_cause}");
    }
}
