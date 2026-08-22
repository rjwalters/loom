//! The daemon's own service bootstrap and IPC accept-loop body (Issue
//! #4712 — split out of `main.rs` so `main.rs` stays a thin clap surface).
//! `main()` (in `main.rs`) is the real entry point; it delegates here for
//! everything past CLI-command dispatch: logging setup, tmux/loom-dir
//! resolution, every autonomous subsystem's startup wiring, and the IPC
//! accept loop that only returns on a startup failure.

use loom_daemon::activity::ActivityDb;
use loom_daemon::admission_brake;
use loom_daemon::auto_update;
use loom_daemon::autonomy_marker;
use loom_daemon::claim_reconciliation;
use loom_daemon::config_resolver;
use loom_daemon::credential_preflight;
use loom_daemon::daemon_heartbeat;
use loom_daemon::epic_supervisor;
use loom_daemon::event_bus::EventBus;
use loom_daemon::health_monitor;
use loom_daemon::host_breaker;
use loom_daemon::idle_exit;
use loom_daemon::install_self_check;
use loom_daemon::ipc::IpcServer;
use loom_daemon::main_health_gate;
use loom_daemon::metrics_collector;
use loom_daemon::observability;
use loom_daemon::orphan_process_reaper;
use loom_daemon::primary_checkout_reaper;
use loom_daemon::quarantine_reconciliation;
use loom_daemon::rate_limit_breaker;
use loom_daemon::role_collision;
use loom_daemon::role_runner;
use loom_daemon::sweep_registry::{self, SweepRegistry, SweepRegistryConfig};
use loom_daemon::terminal::TerminalManager;
use loom_daemon::token_ranking_refresh;
use loom_daemon::watch_registry;
use loom_daemon::watchdog_provisioning_guard;
use loom_daemon::work_finder;
use loom_daemon::workspace_pool::WorkspacePool;
use loom_daemon::worktree_reaper;
use loom_daemon::{extract_configured_terminal_ids, rotate_log_file};

use anyhow::{anyhow, Result};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use crate::cli;
use crate::{handle_cli_command, Cli, Commands, FleetAction};
use clap::Parser;

use cli::dispatch::handle_dispatch_command;
use cli::dispatch_backoff::handle_dispatch_backoff_command;
use cli::quarantine::handle_quarantine_command;
use cli::restart::handle_restart_command;
use cli::serve_cmd::handle_serve_command;
use cli::status::{handle_fleet_roll_command, handle_fleet_status_command, handle_status_command};
use cli::watch::handle_watch_command;

/// Decide whether a freshly minted/cached `GH_TOKEN` value warrants a
/// `std::env::set_var` write, given the value currently exported in the process
/// environment (#4456).
///
/// The `github-app` refresh tick fires every `GITHUB_APP_REFRESH_INTERVAL`
/// (~5min), but the shell helper only re-mints when the cached token is close
/// to expiry (~hourly). A cache hit still reports [`GithubAppOutcome::Minted`]
/// with the *unchanged* token, so writing unconditionally rewrites `GH_TOKEN`
/// ~10 of every ~11 ticks with an identical value. Each redundant `set_var`
/// under the multithreaded tokio runtime opens a (tiny) `environ`
/// use-after-free window against the `gh`/`git` children the daemon constantly
/// spawns, for no benefit.
///
/// Returns `true` only when the minted value actually differs from `current`
/// (including the case where nothing is exported yet, `current == None`). This
/// is a pure seam so the guard can be unit-tested without mutating the
/// process-global environment (which would add to the test-race hazard tracked
/// by #4385).
fn gh_token_needs_update(current: Option<&str>, minted: &str) -> bool {
    current != Some(minted)
}

#[cfg(test)]
mod gh_token_guard_tests {
    //! Tests for the #4456 value-changed guard on the `github-app` refresh
    //! tick. Deliberately a *pure* function test — it does not touch the
    //! process-global `GH_TOKEN`, so it neither races nor adds to the
    //! test-env-mutation hazard tracked by #4385.
    use super::gh_token_needs_update;

    #[test]
    fn cache_hit_same_value_skips_write() {
        // The ~10-of-11 common case: the shell helper returned the unchanged
        // cached token, so no `set_var` is warranted.
        assert!(!gh_token_needs_update(Some("ghs_abc"), "ghs_abc"));
    }

    #[test]
    fn genuine_rotation_writes() {
        // A real ~hourly rotation: the minted value differs -> write.
        assert!(gh_token_needs_update(Some("ghs_old"), "ghs_new"));
    }

    #[test]
    fn unset_env_writes() {
        // Nothing exported yet (Err(VarError) -> None): treat as different so
        // the first post-boot tick still exports the token.
        assert!(gh_token_needs_update(None, "ghs_first"));
    }

    #[test]
    fn empty_current_differs_from_nonempty() {
        // An empty exported value is not equal to a real minted token.
        assert!(gh_token_needs_update(Some(""), "ghs_real"));
    }
}

/// The daemon's real entry point: CLI dispatch, then full daemon startup ending
/// in the IPC accept loop (which only returns on a startup failure — a running
/// daemon leaves via one of the `std::process::exit` paths instead).
pub(crate) async fn run_daemon() -> Result<()> {
    let cli = Cli::parse();

    // Handle CLI commands (init mode)
    if let Some(command) = cli.command {
        return match command {
            // `status` connects to the running daemon over its Unix socket, so
            // it needs the async runtime (unlike the other sync subcommands).
            Commands::Status {
                json,
                pipeline,
                timeout_secs,
            } => handle_status_command(json, pipeline, timeout_secs).await,
            // `health` (#4761) needs the async runtime for the same reason
            // `status` does — one IPC round-trip — plus a bounded forge fan-out
            // for the queue-depth/throughput sections.
            Commands::Health { since, json } => {
                cli::health::handle_health_command(since, json).await
            }
            // `peer-claims` connects to the running daemon over its Unix
            // socket for the same `DaemonStatus` round-trip `status` performs
            // (Issue #5921), so it needs the async runtime for the same
            // reason.
            Commands::PeerClaims { json } => {
                cli::peer_claims_cmd::handle_peer_claims_command(json).await
            }
            // `quarantine` connects to the running daemon over its Unix socket
            // (the quarantine state is in-memory), so it needs the async runtime.
            Commands::Quarantine { action } => handle_quarantine_command(action).await,
            // `dispatch-backoff` connects to the running daemon over its Unix
            // socket (the per-issue backoff state is in-memory, Issue
            // #4485/#6192), so it needs the async runtime for the same reason
            // `quarantine` does.
            Commands::DispatchBackoff { action } => handle_dispatch_backoff_command(action).await,
            // `dispatch` connects to the running daemon over its Unix socket to
            // enqueue a sweep (Issue #3952), so it needs the async runtime.
            Commands::Dispatch {
                issue,
                workspace,
                model,
                effort,
                depends_on,
                force,
            } => handle_dispatch_command(issue, workspace, model, effort, depends_on, force).await,
            // `cancel` connects to the running daemon over its Unix socket to
            // terminate a sweep (Issue #4980) — the `dispatch` sibling, same
            // socket, same reason it needs the async runtime.
            Commands::Cancel {
                sweep_id,
                issue,
                grace,
                workspace,
            } => cli::cancel::handle_cancel_command(sweep_id, issue, grace, workspace).await,
            // `watch` connects to the running daemon over its Unix socket to
            // register/list/remove durable watches (Issue #3971).
            Commands::Watch { action } => handle_watch_command(action).await,
            // `serve` binds a local HTTP listener and, per request, connects to
            // the running daemon over its Unix socket for a fresh `DaemonStatus`
            // snapshot (Issue #4391), so it needs the async runtime.
            Commands::Serve {
                port,
                bind,
                allow_non_loopback,
                peers,
            } => handle_serve_command(port, &bind, allow_non_loopback, &peers).await,
            // `restart` connects to the running daemon over its Unix socket to
            // trigger the supervised restart primitive (Issue #4054), or a
            // scheduled drain-and-restart (Issue #4090).
            Commands::Restart {
                drain,
                timeout,
                force_after_timeout,
                abort_drain,
                then_exit,
                reload_supervisor,
            } => {
                handle_restart_command(
                    drain,
                    timeout,
                    force_after_timeout,
                    abort_drain,
                    then_exit,
                    reload_supervisor,
                )
                .await
            }
            // `fleet status` collects the local host's own status over the
            // daemon's Unix socket (issue #4342), so — unlike `fleet
            // add-worker`, which is pure ssh/filesystem and stays on the sync
            // `handle_cli_command` path — it needs the async runtime too.
            Commands::Fleet {
                action: FleetAction::Status { json, timeout_secs },
            } => handle_fleet_status_command(json, timeout_secs).await,
            // `fleet roll` fans out concurrently across hosts (issue #5504),
            // bounding each host with a `tokio::time::timeout` the same way
            // `fleet status` does, so it needs the async runtime too.
            Commands::Fleet {
                action:
                    FleetAction::Roll {
                        host,
                        all,
                        timeout,
                        json,
                    },
            } => handle_fleet_roll_command(host, all, timeout, json).await,
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
        let loom_dir = resolve_loom_dir()?;
        (loom_dir, socket_path)
    } else {
        let loom_dir = resolve_loom_dir()?;
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
    //
    // `sweep_workspace` seeds the *default* registry only. As of #4299, an
    // explicit-`workspace_root`-absent `DispatchSweep`/`dispatch_sweep`
    // request no longer blindly trusts this cwd-derived value as its target:
    // `ipc::resolve_dispatch_registry` consults the on-disk workspace
    // registry first and only falls back to this default when the registry
    // is empty or this root is itself registered. See that function's doc
    // comment for the full precedence.
    let sweep_workspace = workspace_from_env
        .as_ref()
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| std::path::PathBuf::from("."));

    // #6499: a loud, top-of-boot-block diagnosis of the legacy
    // `.loom/config.json` tier — every existing repo's sole populated config
    // tier, and the one a host-local patch (safehouse/observability paths,
    // `daemon.delegatedTo`, …) lives in ahead of #5457's durable fix. A parse
    // failure here (most commonly an abandoned `git stash pop` conflict
    // leaving live `<<<<<<<`/`=======`/`>>>>>>>` markers in the tracked file)
    // makes `resolve_effective_config` silently fall back to `{}` for this
    // tier on every one of its dozens of call sites, each logging only a
    // `WARN` — easy to miss in normal boot volume, and the exact failure mode
    // that left observability/safehouse/roleRunner silently disabled on two
    // fleet hosts for ~70 minutes before anyone noticed. This check runs once
    // at boot (not once per resolve) and names the consequence explicitly.
    if let Some(diagnosis) = config_resolver::diagnose_legacy_config(&sweep_workspace) {
        log::error!(
            "config_resolver: {} is unreadable/malformed ({diagnosis}) — \
             observability/safehouse/roleRunner config lost, running on defaults for the \
             remainder of this process's life. Run `jq . {}` to see the parse error; if it \
             shows literal conflict markers, resolve them manually (keep this host's own \
             paths) and restart the daemon. See .loom/docs/troubleshooting.md for the full \
             recovery procedure.",
            sweep_workspace
                .join(config_resolver::LEGACY_CONFIG_REL)
                .display(),
            sweep_workspace
                .join(config_resolver::LEGACY_CONFIG_REL)
                .display(),
        );
    }

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

    // #6171: record the daemon's own primary workspace root once, before any
    // forge call that could trigger a forced per-owner credential refresh
    // (`credential_preflight::force_refresh_owner_credential`) — that helper
    // anchors `.loom/scripts/lib/github-app-token.sh` and
    // `.loom/gh-config-by-owner/*` here regardless of which managed repo's
    // checkout root the refresh was triggered from.
    credential_preflight::register_primary_workspace_root(&sweep_workspace);

    // Startup forge-credential preflight (Issue #4005; GitHub App identity
    // mechanism added by #4430). Resolved once, here — immediately before the
    // claim-reconciliation pass below, the daemon's first `gh` consumer — so
    // a headless/SSH-only start with neither an exported GH_TOKEN/GITHUB_TOKEN
    // nor an unlockable GUI login keychain is diagnosed loudly at boot
    // instead of surfacing as silent per-tick 401s for the life of the
    // process. Non-fatal and bounded (same posture as the reconciliation pass
    // itself): a `gh` hiccup here never blocks startup.
    //
    // #4430: when a GitHub App id + private key are configured (env or
    // `forge.githubApp.*` config — see `defaults/scripts/lib/github-app-token.sh`),
    // this mints a short-lived installation token and exports it as this
    // process's own `GH_TOKEN`, so every `gh`/`git` child the daemon spawns
    // from this point on inherits it. Absent that config, `owner_repo`
    // resolves fine but the shell helper reports "not_configured" and
    // `run_with_github_app` falls through to the byte-identical pre-#4430
    // `run(...)` path below — no behavior change for any host that hasn't
    // opted in.
    let credential_preflight_probe = credential_preflight::RealGhAuthProbe {
        gh_bin: "gh".to_string(),
        cwd: sweep_workspace.clone(),
    };
    let github_app_script = credential_preflight::resolve_github_app_script(&sweep_workspace);
    let github_app_owner_repo = credential_preflight::nwo_from_git_remote(&sweep_workspace);
    let github_app_preflight = match &github_app_script {
        Some(script_path) => {
            let minter = credential_preflight::RealGithubAppMinter {
                script_path: script_path.clone(),
                cwd: sweep_workspace.clone(),
            };
            credential_preflight::run_with_github_app(
                &credential_preflight_probe,
                &minter,
                github_app_owner_repo.as_deref(),
            )
        }
        // No `github-app-token.sh` on disk at all (stale install predating
        // #4430, or a workspace root with no `.loom`/`defaults` tree) —
        // exactly the pre-#4430 path, with zero extra subprocess overhead.
        None => credential_preflight::GithubAppPreflight {
            report: credential_preflight::run(&credential_preflight_probe),
            minted_gh_token: None,
        },
    };
    // #4458: deliver the minted installation token via a daemon-owned
    // `GH_CONFIG_DIR` (`credential_preflight::publish_github_app_token`)
    // instead of exporting it as this process's `GH_TOKEN`. Every `gh` child
    // this daemon spawns (`Command::new("gh")` without `env_clear`, ~76
    // call sites — see the issue body's census) re-reads
    // `$GH_CONFIG_DIR/hosts.yml` fresh on each invocation, so the refresh
    // tick below becomes a pure file rewrite with **zero** recurring
    // `std::env::set_var` calls — the UB race (concurrent `setenv` vs. the
    // `environ` reads inside every concurrently spawned `Command`) this issue
    // eliminates. `github_app_gh_config_dir_active` tracks whether the
    // one-time env activation below has happened yet, so it fires **at most
    // once** for the life of the process (see the tick closure further down
    // for the rare case where activation happens there instead of here).
    //
    // The one remaining `std::env::set_var` (`GH_CONFIG_DIR` itself, plus the
    // paired `remove_var` calls) is that single activation: it runs here,
    // before the refresh-tick task (or any other tokio task that spawns
    // `gh`/`git`) has been created, so there is no concurrent `Command::spawn`
    // in this process yet to race the `environ` write — the residual window
    // this issue's acceptance criteria allow a startup site to keep, given a
    // comment naming the race and the rationale for accepting it. Clearing
    // `GH_TOKEN`/`GITHUB_TOKEN` here (not just setting `GH_CONFIG_DIR`)
    // matters too: `gh` checks the env vars *before* `GH_CONFIG_DIR`'s
    // `hosts.yml`, so an operator-exported ambient token would otherwise
    // silently outrank the just-minted app token — the pre-#4458 code
    // achieved the same override by unconditionally overwriting `GH_TOKEN`.
    let github_app_config_dir = credential_preflight::github_app_gh_config_dir(&sweep_workspace);
    let mut github_app_gh_config_dir_active = false;
    if let Some(token) = &github_app_preflight.minted_gh_token {
        match credential_preflight::publish_github_app_token(&github_app_config_dir, token) {
            Ok(()) => {
                std::env::remove_var("GH_TOKEN");
                std::env::remove_var("GITHUB_TOKEN");
                std::env::set_var("GH_CONFIG_DIR", &github_app_config_dir);
                github_app_gh_config_dir_active = true;
            }
            Err(e) => {
                // Fail open, matching the mint-failure posture elsewhere in
                // this preflight: never block startup, never leave the
                // daemon worse off than the pre-#4430 ambient path.
                log::warn!(
                    "credential_preflight: failed to publish github-app token to {} ({e}); \
                     falling back to ambient gh auth — #4458",
                    github_app_config_dir.display()
                );
            }
        }
    }
    let credential_preflight = github_app_preflight.report;

    // Per-owner managed-repo credentials (#5401). The GitHub App mechanism
    // above mints exactly ONE installation token, keyed on this daemon's
    // *workspace root* owner (`github_app_owner_repo`) and delivered via the
    // process-global `GH_CONFIG_DIR`. An installation token is scoped to its
    // own installation's repos, so every managed repo owned by a DIFFERENT
    // account/org (the reporter's fleet spans `rjwalters/*` and `2AMLogic/*`)
    // is unreachable for private data under it — public reads succeed
    // anonymously while private reads and writes silently 404, which is what
    // made a wrong-owner fleet look uniformly healthy (#5401's evidence).
    //
    // The fix: mint a SECOND (third, …) installation token — one per distinct
    // owner among the managed repos — and publish each into its own per-owner
    // `GH_CONFIG_DIR` (reusing #4458's tested delivery path), registering
    // every cross-owner checkout root against it so the daemon's per-repo
    // `gh`/`git` call sites (`credential_preflight::apply_gh_config_for_root`)
    // select the right credential per child. The `mint(owner_repo)` machinery
    // already resolves the correct installation per owner/repo; it was simply
    // never called per owner before now.
    //
    // Only runs when the App mechanism is actually the active credential this
    // run (`minted_gh_token.is_some()`) — an ambient `gh` auth / PAT host is
    // not subject to the single-installation limit. A single-owner fleet
    // yields no plans, registers nothing, and is byte-identical to pre-#5401.
    // Each established plan is registered into
    // `credential_preflight::register_owner_refresh_source` (#6171), the
    // shared registry the refresh task below re-reads every tick — rather
    // than a `Vec` captured once here — so an owner discovered dynamically
    // after startup (via `force_refresh_owner_credential`'s 404-recovery
    // path) is picked up by the SAME periodic freshness tick as one
    // established here, with no separate task to spawn or track.
    if let (Some(root_owner_repo), Some(script_path), true) = (
        &github_app_owner_repo,
        &github_app_script,
        github_app_preflight.minted_gh_token.is_some(),
    ) {
        let root_owner = credential_preflight::owner_of_nwo(root_owner_repo).to_string();
        let cross_owner_registry =
            loom_daemon::workspace_registry::WorkspaceRegistry::load_default().unwrap_or_default();
        let managed: Vec<(std::path::PathBuf, String)> = cross_owner_registry
            .effective_roots(&sweep_workspace)
            .into_iter()
            .filter_map(|root| {
                credential_preflight::nwo_from_git_remote(&root).map(|nwo| (root, nwo))
            })
            .collect();
        let plans = credential_preflight::plan_cross_owner_credentials(&root_owner, &managed);
        if !plans.is_empty() {
            let minter = credential_preflight::RealGithubAppMinter {
                script_path: script_path.clone(),
                cwd: sweep_workspace.clone(),
            };
            for plan in plans {
                let owner_dir = credential_preflight::github_app_gh_config_dir_for_owner(
                    &sweep_workspace,
                    &plan.owner,
                );
                match credential_preflight::GithubAppMinter::mint(
                    &minter,
                    &plan.representative_owner_repo,
                ) {
                    credential_preflight::GithubAppOutcome::Minted {
                        token,
                        installation_id,
                        app_id,
                        ..
                    } => match credential_preflight::publish_github_app_token(&owner_dir, &token) {
                        Ok(()) => {
                            for root in &plan.roots {
                                credential_preflight::register_root_gh_config_dir(root, &owner_dir);
                            }
                            // #5431: also register the owner slug so call sites
                            // that target the repo by `--repo <owner/repo>`
                            // (fleet::drain) or an `owner/repo`-in-path `gh api`
                            // (telemetry::visibility) — with no checkout-root
                            // `current_dir` to key off — select this owner's
                            // credential too.
                            credential_preflight::register_owner_gh_config_dir(
                                &plan.owner,
                                &owner_dir,
                            );
                            // #6171: also register this owner as a periodic
                            // refresh source — see the comment on the
                            // registration loop above.
                            credential_preflight::register_owner_refresh_source(
                                &plan.representative_owner_repo,
                                &owner_dir,
                            );
                            log::info!(
                                "credential_preflight: per-owner github-app credential established \
                                 for {} ({} managed repo(s), app {app_id} installation \
                                 {installation_id}) — those cross-owner repos are now reachable \
                                 (#5401)",
                                plan.owner,
                                plan.roots.len()
                            );
                        }
                        Err(e) => {
                            log::warn!(
                                "credential_preflight: minted a per-owner github-app token for {} \
                                 but could not publish it to {} ({e}); its {} managed repo(s) \
                                 remain unreachable — #5401",
                                plan.owner,
                                owner_dir.display(),
                                plan.roots.len()
                            );
                        }
                    },
                    credential_preflight::GithubAppOutcome::Error(reason) => {
                        log::warn!(
                            "credential_preflight: could not mint a per-owner github-app token for \
                             {} ({reason}); its {} managed repo(s) ({:?}) remain unreachable — the \
                             app may not be installed on {}'s account/org (public reads may still \
                             work; private reads and writes will 404) — #5401",
                            plan.owner,
                            plan.roots.len(),
                            plan.roots,
                            plan.owner
                        );
                    }
                    credential_preflight::GithubAppOutcome::NotConfigured => {
                        // Unreachable in practice: `minted_gh_token.is_some()`
                        // above already proved the app is configured. Treat as
                        // a no-op rather than a failure.
                    }
                }
            }
        }
    }

    // #4430: ticks every `GITHUB_APP_REFRESH_INTERVAL` (~5min) to keep the
    // minted installation token fresh across its ~1h lifetime for a
    // long-running daemon. Ticks that don't rotate the token
    // never *write* anything: the `NotConfigured` arm is a true no-op, and a
    // cache-hit tick (the shell helper's own cache still has plenty of life —
    // ~10 of every ~11 ticks) compares against `current_token` (a plain local
    // captured by this task, never the process env — #4456's original
    // intent, now trivially true since there is no shared `GH_TOKEN` env var
    // left to read) and skips the write because the minted value is
    // unchanged. Only a genuine rotation (value differs) rewrites
    // `hosts.yml`; a mint failure is logged and the loop keeps whatever
    // credential `GH_CONFIG_DIR` already has on disk (fail-open, matching the
    // startup preflight's posture).
    if let (Some(script_path), Some(owner_repo)) = (&github_app_script, &github_app_owner_repo) {
        let script_path = script_path.clone();
        let cwd = sweep_workspace.clone();
        let owner_repo = owner_repo.clone();
        let config_dir = github_app_config_dir.clone();
        let mut current_token = github_app_preflight.minted_gh_token.clone();
        let mut config_dir_active = github_app_gh_config_dir_active;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(credential_preflight::GITHUB_APP_REFRESH_INTERVAL).await;
                let script_path = script_path.clone();
                let cwd = cwd.clone();
                let owner_repo = owner_repo.clone();
                let outcome = tokio::task::spawn_blocking(move || {
                    let minter = credential_preflight::RealGithubAppMinter { script_path, cwd };
                    credential_preflight::GithubAppMinter::mint(&minter, &owner_repo)
                })
                .await;
                match outcome {
                    Ok(credential_preflight::GithubAppOutcome::Minted {
                        token,
                        installation_id,
                        app_id,
                        ..
                    }) => {
                        // #5630: a successful mint (fresh or cache hit) ends this
                        // source's credential-failure streak, so the main-health
                        // gate goes back to trusting its forge answers on the very
                        // next tick. A publish failure below re-opens it — an
                        // unpublished token is just as un-refreshed on disk.
                        credential_preflight::record_forge_credential_success(
                            credential_preflight::CREDENTIAL_SOURCE_PRIMARY,
                        );
                        // #4456/#4458: only rewrite when the value actually
                        // rotated, and do it as a pure file write — no
                        // `std::env::set_var` anywhere in this arm once
                        // `config_dir_active` is already true (the common
                        // case: activation happened at startup above).
                        if gh_token_needs_update(current_token.as_deref(), &token) {
                            // Rare edge case: the app becomes configured
                            // *after* startup (e.g. the shell helper's own
                            // config appears mid-run) without the daemon
                            // restarting, so activation didn't happen above.
                            // This is a second, still-bounded (at most once
                            // per process) `std::env::set_var` window — the
                            // recurring, steady-state race this issue targets
                            // is the one eliminated; this cold-start
                            // transition is unchanged in kind from the
                            // pre-#4458 code's own equivalent one-time
                            // `set_var` in this same arm.
                            if !config_dir_active {
                                std::env::remove_var("GH_TOKEN");
                                std::env::remove_var("GITHUB_TOKEN");
                                std::env::set_var("GH_CONFIG_DIR", &config_dir);
                                config_dir_active = true;
                            }
                            match credential_preflight::publish_github_app_token(
                                &config_dir,
                                &token,
                            ) {
                                Ok(()) => {
                                    current_token = Some(token);
                                    log::debug!(
                                        "credential_preflight: github-app refresh tick rotated \
                                         GH_TOKEN (app {app_id} installation {installation_id}) \
                                         — #4430"
                                    );
                                }
                                Err(e) => {
                                    // #5630: the mint succeeded but the token never
                                    // reached disk, so `GH_CONFIG_DIR` still holds the
                                    // aging one — same staleness as a failed mint.
                                    credential_preflight::record_forge_credential_failure(
                                        credential_preflight::CREDENTIAL_SOURCE_PRIMARY,
                                        &format!("could not publish the minted token: {e}"),
                                    );
                                    log::warn!(
                                        "credential_preflight: github-app refresh tick failed to \
                                         publish token to {} ({e}); GH_CONFIG_DIR left unchanged \
                                         (app {app_id} installation {installation_id}) — #4458",
                                        config_dir.display()
                                    );
                                }
                            }
                        } else {
                            log::debug!(
                                "credential_preflight: github-app refresh tick cache hit, \
                                 GH_TOKEN unchanged (app {app_id} installation {installation_id}) \
                                 — #4456"
                            );
                        }
                    }
                    Ok(credential_preflight::GithubAppOutcome::NotConfigured) => {
                        // Cheap no-op tick -- nothing to refresh.
                    }
                    Ok(credential_preflight::GithubAppOutcome::Error(reason)) => {
                        // #5630: the credential on disk is now aging with no
                        // successful refresh behind it. Record the streak so the
                        // main-health gate holds its previous verdicts instead of
                        // reading the resulting forge failures as "every repo's
                        // main just went red".
                        credential_preflight::record_forge_credential_failure(
                            credential_preflight::CREDENTIAL_SOURCE_PRIMARY,
                            &reason,
                        );
                        log::warn!(
                            "credential_preflight: github-app refresh tick failed ({reason}); \
                             GH_TOKEN left unchanged — main-health gate verdicts are held for up \
                             to {}s while this persists — #4430/#5630",
                            credential_preflight::resolve_forge_credential_stale_grace().as_secs()
                        );
                    }
                    Err(e) => {
                        // A join error means the mint never completed either, so
                        // the credential is just as un-refreshed as an Error arm.
                        credential_preflight::record_forge_credential_failure(
                            credential_preflight::CREDENTIAL_SOURCE_PRIMARY,
                            &format!("github-app refresh task join error: {e}"),
                        );
                        log::warn!(
                            "credential_preflight: github-app refresh task join error: {e} — #4430"
                        );
                    }
                }
            }
        });
    }

    // #5401: keep each per-owner credential fresh across its ~1h lifetime,
    // ticking every `GITHUB_APP_REFRESH_INTERVAL` (~5min) — the same cadence
    // as the workspace-root owner's tick above. Re-mint the
    // owner's representative repo and republish into its `GH_CONFIG_DIR` — a
    // pure file rewrite via #4458's delivery path; the root->dir registration
    // is path-stable and never needs re-registering. A fail-open loop
    // (matching the startup posture): a mint/publish miss logs and keeps
    // whatever token that owner's dir already holds.
    //
    // #6171: spawned whenever the App mechanism itself is configured
    // (`github_app_script.is_some()`), NOT only when a per-owner plan
    // happened to exist at startup — the source list
    // (`credential_preflight::owner_refresh_sources()`) is re-read from the
    // shared registry on every tick, so an owner discovered later via
    // `force_refresh_owner_credential`'s 404-recovery path (including a
    // fleet that started single-owner and only later grew a cross-owner
    // workspace) is picked up automatically, without a second task to spawn.
    // An empty registry makes a tick a cheap no-op loop, same cost as the
    // pre-#6171 early return.
    if let Some(script_path) = &github_app_script {
        let script_path = script_path.clone();
        let cwd = sweep_workspace.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(credential_preflight::GITHUB_APP_REFRESH_INTERVAL).await;
                let owners = credential_preflight::owner_refresh_sources();
                for (owner_repo, config_dir) in &owners {
                    let script_path = script_path.clone();
                    let cwd = cwd.clone();
                    let owner_repo_for_mint = owner_repo.clone();
                    let outcome = tokio::task::spawn_blocking(move || {
                        let minter = credential_preflight::RealGithubAppMinter { script_path, cwd };
                        credential_preflight::GithubAppMinter::mint(&minter, &owner_repo_for_mint)
                    })
                    .await;
                    // #5630: each owner's credential is its own refresh
                    // source. A saturated host times these out alongside the
                    // primary tick, and their repos' forge calls go just as
                    // wrong — so they feed the same staleness tracker, keyed
                    // separately so a healthy owner's success cannot clear a
                    // failing owner's streak.
                    let source = credential_preflight::credential_source_for_owner(owner_repo);
                    match outcome {
                        Ok(credential_preflight::GithubAppOutcome::Minted { token, .. }) => {
                            if let Err(e) =
                                credential_preflight::publish_github_app_token(config_dir, &token)
                            {
                                credential_preflight::record_forge_credential_failure(
                                    &source,
                                    &format!("could not publish the minted token: {e}"),
                                );
                                log::warn!(
                                    "credential_preflight: per-owner github-app refresh could \
                                         not publish the token for {owner_repo} to {} ({e}); its \
                                         credential left unchanged — #5401",
                                    config_dir.display()
                                );
                            } else {
                                credential_preflight::record_forge_credential_success(&source);
                            }
                        }
                        Ok(credential_preflight::GithubAppOutcome::NotConfigured) => {
                            // Cheap no-op — nothing to refresh for this owner.
                        }
                        Ok(credential_preflight::GithubAppOutcome::Error(reason)) => {
                            credential_preflight::record_forge_credential_failure(&source, &reason);
                            log::warn!(
                                "credential_preflight: per-owner github-app refresh for \
                                     {owner_repo} failed ({reason}); its credential left \
                                     unchanged — main-health gate verdicts are held for up to \
                                     {}s while this persists — #5401/#5630",
                                credential_preflight::resolve_forge_credential_stale_grace()
                                    .as_secs()
                            );
                        }
                        Err(e) => {
                            credential_preflight::record_forge_credential_failure(
                                &source,
                                &format!("per-owner refresh task join error: {e}"),
                            );
                            log::warn!(
                                "credential_preflight: per-owner github-app refresh task join \
                                     error for {owner_repo}: {e} — #5401"
                            );
                        }
                    }
                }
            }
        });
    }

    // GitHub rate-limit circuit breaker (#4429). Registered here — before the
    // startup reconciliation passes just below — because *every* gh-polling
    // consumer (reconciliation, work-finder, epic supervisor, role runner)
    // starts after this point and consults the global handle. Unlike the host
    // breaker (registered inside the work-finder branch, which is its sole
    // sampler), rate-limit failures can be observed by any of those loops, so
    // the breaker exists whether or not the work-finder is enabled.
    let rate_limit_config = rate_limit_breaker::resolve_config_for(&sweep_workspace);
    rate_limit_breaker::register_global(std::sync::Arc::new(
        rate_limit_breaker::SharedRateLimitBreaker::new(rate_limit_config),
    ));
    log::info!(
        "rate_limit_breaker: enabled={} (fallback_cooldown_secs={}) — forge polling pauses \
         until the API window resets when the shared rate limit is exhausted (#4429)",
        rate_limit_config.enabled,
        rate_limit_config.fallback_cooldown_secs,
    );

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
    // every `dispatch()` — see `sweep_journal`) is A liveness source, and it
    // survives exactly the restart that wipes this in-memory registry — but
    // it, like every other evidence source `claim_reconciliation::decide`/
    // `plan` consult, is HOST-scoped: it can only prove a claim dead *on
    // this host*, and is structurally blind to a still-running sweep on a
    // *different* host. #3651's fail-safe ("absent evidence means treat
    // every claim as ALIVE") was therefore only *syntactically* satisfied by
    // this journal alone — a peer host's live claim always looks like "no
    // evidence" to it. Epic #6165 Phase 2 (Issue #6286) closed that gap with
    // a genuinely FLEET-scoped liveness source: the lease record (`<!--
    // loom:lease host=... sweep=... -->`, Issue #6179/#6180), consulted as
    // the final gate in `claim_reconciliation::forge::
    // reconcile_workspace_with_coordination` before any reclaim actually
    // fires — see that module's top doc comment for the full picture.
    //
    // Stated plainly (Epic #6165 Phase 4, Issue #6318): #3651's invariant is
    // now upheld by the LEASE, not by this journal. The journal is
    // INSUFFICIENT EVIDENCE for a peer host's claim — it was never designed
    // to be fleet-scoped and remains exactly as useful as it always was for
    // its original purpose, reconciling THIS host's own claims after a
    // restart (the `DeadPid`/`DeadRunRegistry` branches above stay journal-
    // driven and unchanged). What changed is only that a peer's claim no
    // longer falls through to the age-based grace period on this journal's
    // silence alone — the lease gate answers that question with fleet-wide,
    // forge-timestamped evidence instead.
    //
    // This pass is a bounded, logged, best-effort sweep over every
    // `effective_roots()` workspace (empty registry ⇒ just this one). It
    // never blocks daemon startup — a `gh` hiccup in one repo is logged and
    // skipped, and the remaining repos are still reconciled.
    //
    // Promoted from a startup-only pass to ALSO run on an interval (Issue
    // #4348): a daemon restart is not the only way a `loom:building` claim's
    // sweep can die. A manually/externally spawned detached sweep killed by
    // an external `SIGKILL` (the incident that motivated #4348) never writes
    // the journal entry above, but its checkpoint's `task_id` joined against
    // `.loom/sweep-run/<task_id>.json` gives `claim_reconciliation` the same
    // provable-death answer — see that module's doc comment for the full
    // evidence-source precedence. `run_reconciliation_pass` (this call) and
    // the periodic task spawned just below share one implementation, so the
    // startup and periodic behavior are identical EXCEPT for the
    // `is_startup` flag (Issue #6615): only this call passes `true`, which
    // lets a `loom:building` claim with zero liveness evidence (no journal
    // entry, no run-registry pid) be reclaimed immediately instead of
    // waiting out the multi-hour age gate — total absence of evidence right
    // after a restart is much stronger evidence of a claim orphaned by a
    // crash between `begin_issue_dispatch`'s label flip and
    // `finish_issue_dispatch`'s journal write than the identical absence is
    // during steady-state operation (where the periodic pass below, passing
    // `false`, still protects a manually-spawned `/loom:sweep` with no
    // journal entry yet).
    claim_reconciliation::run_reconciliation_pass(&sweep_workspace, true);
    let _claim_reconciliation_handle =
        claim_reconciliation::spawn_periodic_reconciliation_task(sweep_workspace.clone());

    // Stranded-quarantine reconciliation across every managed workspace
    // (Issue #4110). The insta-crash quarantine (#3939) is memory-only, so a
    // restart drops the in-memory pause while the `loom:blocked` label it
    // applied survives on the forge — with nothing left to release it, the
    // issue is permanently invisible to the work finder. This pass scans
    // every registered workspace's open `loom:blocked` issues and releases
    // the ones carrying a daemon-authored quarantine comment back to
    // `loom:issue`; a human's manual `loom:blocked` (no such comment) is
    // never touched. Reuses the same `workspace_registry` roots resolved
    // above for claim reconciliation.
    if quarantine_reconciliation::reconciliation_enabled() {
        let workspace_registry =
            loom_daemon::workspace_registry::WorkspaceRegistry::load_default().unwrap_or_default();
        let roots = workspace_registry.effective_roots(&sweep_workspace);
        let gh_bin = std::path::PathBuf::from("gh");
        let mut total_checked = 0usize;
        let mut total_released = 0usize;
        for root in &roots {
            let (checked, released) =
                quarantine_reconciliation::forge::reconcile_workspace(&gh_bin, root);
            total_checked += checked;
            total_released += released;
        }
        if total_released > 0 {
            log::info!(
                "quarantine_reconciliation: startup pass checked {total_checked} loom:blocked \
                 issue(s) across {} workspace(s), released {total_released} stranded \
                 quarantine(s) (#4110)",
                roots.len()
            );
        } else {
            log::debug!(
                "quarantine_reconciliation: startup pass checked {total_checked} loom:blocked \
                 issue(s) across {} workspace(s), nothing to release",
                roots.len()
            );
        }
    } else {
        log::info!(
            "quarantine_reconciliation: startup pass disabled ({}=0)",
            quarantine_reconciliation::RECONCILE_ENABLED_ENV
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

    // Startup-proof occupancy grace (Issue #4003): a freshly-dispatched sweep
    // counts toward the work-finder's admission budget regardless of progress
    // for this long; past it, a sweep with zero observed startup-proof signal
    // (no worktree/checkpoint/log-past-header) stops consuming a slot, well
    // before the (unchanged) 300s startup watchdog would cancel/re-dispatch it.
    let startup_proof_grace = sweep_registry::resolve_startup_proof_grace(&startup_race_config);
    sweep.set_startup_proof_grace(startup_proof_grace);
    log::info!(
        "sweep_registry: startup-proof occupancy grace = {}s (#4003)",
        startup_proof_grace.as_secs()
    );

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

    // Per-issue dispatch backoff (#4485): resolve env > config > default for the
    // default workspace so a failing issue's re-dispatch cadence is bounded even
    // when its deaths land in a quarantine carve-out (#4122 exhaustion, #4386
    // pre-flight) and never reach the 3-strike quarantine threshold.
    let dispatch_backoff_config = sweep_registry::resolve_dispatch_backoff_config(&sweep_workspace);
    sweep.set_dispatch_backoff_config(dispatch_backoff_config);
    log::info!(
        "sweep_registry: per-issue dispatch backoff {} (base={}s, max={}s) (#4485)",
        if dispatch_backoff_config.enabled {
            "enabled"
        } else {
            "disabled"
        },
        dispatch_backoff_config.base.as_secs(),
        dispatch_backoff_config.max.as_secs()
    );

    // Claude-wrapper pre-flight-death workspace tripwire (#4386): resolve
    // env > config > default for the default workspace so a fleet-wide,
    // cross-issue spawn failure (e.g. a stale `.mcp.json`) trips a visible
    // advisory instead of reading as an idle-healthy daemon.
    let preflight_tripwire_config =
        sweep_registry::resolve_preflight_tripwire_config(&sweep_workspace);
    sweep.set_preflight_tripwire_config(preflight_tripwire_config);
    log::info!(
        "sweep_registry: pre-flight-death tripwire threshold={} (#4386)",
        preflight_tripwire_config.threshold
    );

    // Cross-host collision detection + enforcement (#4085, upgraded from
    // detection-only by #5789): resolve env > config > default(off) for the
    // default workspace. When enabled, a confirmed pre-flip collision now
    // backs off the dispatch instead of only recording it.
    let detect_collisions = sweep_registry::resolve_collision_detection(&sweep_workspace);
    sweep.set_collision_detection(detect_collisions);
    log::info!(
        "sweep_registry: cross-host collision detection {} (#4085)",
        if detect_collisions {
            "enabled"
        } else {
            "disabled"
        }
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

    // Optional cross-host soft-claim coordination (#4028): a dedicated safehouse
    // connection that advertises this daemon's dispatch claims and consumes peer
    // advertisements into a shared TTL-bounded view, so peer daemons back off
    // before the non-atomic `loom:building` label flip would let them race.
    // Byte-for-byte no-op when `safehouse.enabled` is false/absent. Injected into
    // the seeded default registry here (the IPC `DispatchSweep` path); every other
    // provisioned registry is injected in `get_or_provision`.
    //
    // Deliberately started **before** `start_safehouse_narration` below (Issue
    // #6352): the narration sink reads back this call's publisher + view
    // synchronously (`WorkspacePool::start_safehouse_narration` locks
    // `peer_coord`) to build its fleet-wide completion-dedup handle, so peer
    // coordination must already be established by the time narration starts
    // — reversing this order would silently leave completion dedup
    // per-host-only even with `safehouse.enabled` true.
    workspace_pool.start_peer_coordination(&sweep_workspace);
    {
        let mut reg = sweep_registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        workspace_pool.inject_peer_coordination(&mut reg);
    }

    // Optional safehouse fleet-comms narration (#3997): subscribe the shared
    // event bus and narrate sweep-lifecycle transitions into an E2E Matrix room.
    // Byte-for-byte no-op when `safehouse.enabled` is false/absent. The activity
    // DB handle (#4497) is the same `Arc` the IPC server gets, shared so the
    // public-feed `completion` envelope can carry a best-effort per-issue token
    // total; the sink only ever reads, on the blocking pool. Started after peer
    // coordination above (Issue #6352) so it can pick up the fleet-wide
    // completion-dedup handle.
    workspace_pool.start_safehouse_narration(&sweep_workspace, Some(activity_db.clone()));

    // Restart-survivorship capacity seed (Issue #6262). A daemon restart leaves
    // in-flight sweeps running but rebuilds capacity accounting from scratch, so
    // any survivor the rebuild misses is a slot the work finder believes is free
    // and immediately refills — the mechanism behind the 2026-08-14 "28 running
    // vs cap 12" incident. Two things happen here, both strictly BEFORE the work
    // finder / epic supervisor / role runner / drain supervisor are spawned
    // below:
    //
    //   1. Every registered root's `SweepRegistry` is provisioned, so the
    //      lock-based `reconstruct()` — still the primary mechanism — runs for
    //      ALL roots at one deterministic point rather than as a side effect of
    //      whichever consumer touches each root first.
    //   2. Any still-live sweep the machine-level journal (`~/.loom/sweeps.json`)
    //      records for a managed root that the lock pass did NOT recover is
    //      unioned in. `reconstruct` drops a lock whose `owner.json` is missing
    //      or unparseable without asking whether a process is still running, and
    //      a lock released while the child lived leaves no lock at all — in both
    //      cases the survivor is invisible to it. `claim_reconciliation` above is
    //      NOT the backstop for that: it reconciles the forge labels of sweeps it
    //      can prove are *dead*, and never re-admits a live one into capacity
    //      accounting.
    //
    // Best-effort and never fatal — see `startup_adoption`'s module docs.
    {
        let roots = loom_daemon::workspace_registry::WorkspaceRegistry::load_default()
            .unwrap_or_default()
            .effective_roots(&sweep_workspace);
        let _ = loom_daemon::startup_adoption::seed_capacity_from_journal(&workspace_pool, &roots);
    }

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

    // Shared drain-and-restart state (Issue #4090). Constructed here — before the
    // epic supervisor, work-finder, and role runner — so its flag can be threaded
    // into all three dispatch producers AND into the IPC server (which sets/aborts
    // it and renders it in `loom-daemon status`). With no drain requested the flag
    // stays `false`, so every producer's halt check is byte-for-byte unchanged.
    let drain_state = Arc::new(loom_daemon::ipc::DrainState::new());
    let drain_flag = drain_state.flag();

    // Shared role-runner in-progress guard (#4364): one set, cloned into both
    // the work-finder's idle-edge path and every interval role loop, so an
    // interval run and an idle-triggered run never overlap for the same
    // (root, role). In-process shared state only — no event-bus topic.
    let role_in_progress = role_runner::new_in_progress_guard();
    // #6102: publish the same set as a process-global so `loom-daemon status`
    // can report the live role-agent count alongside in-flight sweeps, without
    // threading this `Arc` through the IPC server. Registered unconditionally
    // (even when the role runner is disabled) so the reported count is honestly
    // `0` rather than "unknown" — and so it is registered exactly once, at the
    // single construction site, rather than from whichever loop happens to spawn.
    role_runner::register_global_in_progress(role_in_progress.clone());

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
            drain_flag.clone(),
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
    // `min(disk headroom, ram headroom, configured_max)` (the CPU/load term
    // added in #3978 was removed in #4512; the token-pool term was removed in
    // #5270) — through the same `SweepRegistry::dispatch()` path the IPC
    // `DispatchSweep` request uses. `LOOM_WORK_FINDER_MAX_CONCURRENT` is
    // repurposed (Phase A → B) from a fixed target into the operator ceiling;
    // the cap also never exceeds the scratch-volume disk headroom nor the
    // host's RAM headroom (never starve concurrent sweep builds into starving
    // the main-health gate's own build, #3978).
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
        // #6203: also resolve *which layer* supplied `configured_max` (env /
        // config / default) so the startup log below can tell an operator
        // whether their `.loom/config.json` edit was actually picked up —
        // this value is captured once here and threaded into the loop as a
        // frozen `usize` (it does not itself hot-apply; see the work_finder
        // module docs and daemon-reference.md's "Dynamic concurrency
        // scaling" section for the restart-required rationale).
        let (configured_max, configured_max_source) =
            work_finder::resolve_max_concurrent_with_source(&work_finder_config);
        // Retired CPU-headroom knobs (#4512): `cpuUtilizationTarget` /
        // `estCoresPerSweep` (and their env twins) are accepted-but-ignored, so
        // a fleet's committed config keeps parsing across the upgrade. Warn
        // once, here at startup, naming exactly what is being ignored and what
        // to tune instead.
        work_finder::warn_deprecated_cpu_knobs(&work_finder_config);
        // Per-tick admission (ramp) cap (#4234): resolved once at startup from
        // the same `work_finder_config`, precedence env > config > default —
        // the same startup-capture pattern as the CPU knobs above. Bounds how
        // many *new* sweeps a single tick may admit, independent of how large
        // the dynamic cap computes to that tick — see
        // `work_finder::WORK_FINDER_MAX_ADMISSIONS_PER_TICK_ENV` for the full
        // ramp-lag rationale (#4231's second wave).
        let max_admissions_per_tick =
            work_finder::resolve_max_admissions_per_tick_with_config(&work_finder_config);
        // #4084: hold new dispatch off a root while its build-gate run is in
        // flight, so a fresh sweep build does not race the gate's own build for
        // cores (the contention that timed the gate out under #4073's mild
        // niceness). Resolved once at startup from the same primary workspace
        // config as the gate's master switch (env > config > default(on)).
        let suppress_dispatch_during_gate = main_health_gate::resolve_suppress_dispatch_during_gate(
            &main_health_gate::read_autonomous_gate_config(&sweep_workspace),
        );
        // Host-distress circuit breaker (#4235): resolve its config once at
        // startup (env > config > default, default-ON) from the daemon's primary
        // workspace and register the process-global handle. The work-finder loop
        // (spawned just below) samples load-per-core into it each tick and
        // consults it as a second dispatch suppressor; `loom-daemon status` and
        // the `dispatch_sweep` IPC handler read it via the same global. Registered
        // only when the work-finder is enabled, since the loop is the breaker's
        // sole sampler — a daemon with no work-finder never trips it (and its
        // dispatch_sweep sees a Closed/absent breaker: zero behavior change).
        let host_breaker_config = host_breaker::resolve_config_for(&sweep_workspace);
        host_breaker::register_global(std::sync::Arc::new(host_breaker::SharedHostBreaker::new(
            host_breaker_config,
        )));
        log::info!(
            "host_breaker: enabled={} (load_per_core_trip={:.2}, sustain_ticks={}, cooldown_secs={})",
            host_breaker_config.enabled,
            host_breaker_config.load_per_core_threshold,
            host_breaker_config.sustain_ticks,
            host_breaker_config.cooldown_secs,
        );
        // Saturation admission brake (#4903): same startup-resolve +
        // process-global-registration shape as the breaker above, and registered
        // in the same place for the same reason — the work-finder loop is its
        // sole sampler, so a daemon with no work-finder never engages it. The
        // brake is the *point-in-time* half of the load-aware pair: it holds NEW
        // admissions while the host is already saturated and releases the moment
        // it recovers, where the breaker latches on sustained distress and holds
        // through a cool-down. Neither ever touches a running sweep.
        let admission_brake_config = admission_brake::resolve_config_for(&sweep_workspace);
        admission_brake::register_global(std::sync::Arc::new(
            admission_brake::SharedAdmissionBrake::new(admission_brake_config),
        ));
        log::info!(
            "admission_brake: enabled={} (load_per_core_hold={:.2}; holds NEW sweep admissions \
             only — in-flight sweeps are never preempted; starvation escape hatch: warn at \
             {}s, yield one tick at {}s of held+0-in-flight, #5715)",
            admission_brake_config.enabled,
            admission_brake_config.load_per_core_threshold,
            admission_brake_config.starvation_warn_secs,
            admission_brake_config.starvation_escape_secs,
        );
        log::info!(
            "work_finder: enabled (multi-workspace, interval={}s, \
             configured_max={configured_max} (source={configured_max_source}), \
             max_admissions_per_tick={max_admissions_per_tick}, build_slots={}, \
             dynamic cap = min(disk, ram, configured_max) — token axis is \
             selection-only, not a cap, since #5270, \
             global across workspaces; configured_max is startup-read only — \
             a config edit requires a daemon restart to take effect, #6203)",
            interval.as_secs(),
            loom_daemon::build_slot::resolve_slots()
        );
        // Multi-workspace fan-out (#3928): re-reads `effective_roots()` each tick
        // and dispatches into each registered repo's own working tree via the
        // shared `workspace_pool`. Empty registry ⇒ the single `sweep_workspace`.
        Some(work_finder::spawn_multi_work_finder_task(
            workspace_pool.clone(),
            sweep_workspace.clone(),
            interval,
            configured_max,
            max_admissions_per_tick,
            workspace_health_states.clone(),
            suppress_dispatch_during_gate,
            event_bus.clone(),
            drain_flag.clone(),
            role_in_progress.clone(),
        ))
    } else {
        log::debug!("work_finder: disabled (set LOOM_WORK_FINDER=1 to enable)");
        None
    };

    // Idle-edge role triggering (#4364) is inert without the work-finder loop:
    // the work finder is the sole source of the per-root idle signal, so an
    // `autonomous.roleRunner.onIdle` set with no work finder enabled can never
    // fire. Warn once at startup so a misconfiguration surfaces in the log
    // rather than silently doing nothing. (The interval cadence still runs.)
    if !work_finder::resolve_enabled(&work_finder_config)
        && !role_runner::resolve_on_idle_roles(&role_runner::read_role_runner_config(
            &sweep_workspace,
        ))
        .is_empty()
    {
        log::warn!(
            "role_runner: autonomous.roleRunner.onIdle is set but the work finder is disabled — \
             idle-edge triggering has no idle signal to observe and will never fire (enable the \
             work finder with LOOM_WORK_FINDER=1 or autonomous.workFinder.enabled=true). The \
             interval cadence is unaffected."
        );
    }

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
    // to run by hand: `probe-tokens.sh --ranking` / `loom-daemon tokens check
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
    // the underlying `loom-daemon tokens check --ranking` write is atomic, so the two
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

    // Periodic merged-PR worktree reaper (Issue #4876). Before this loop the
    // ONLY trigger for "auto-removed when their PR merges" (CLAUDE.md's stated
    // contract) was `merge-pr.sh`'s synchronous `_remove_loom_worktree()` — so a
    // PR merged on a different HOST, in a different CHECKOUT, through the forge
    // UI/API, or while the daemon was down left its worktree behind forever.
    // Across a three-host fleet that leaked 44 worktrees (81% disk) on one
    // worker and 35 on the primary host. This loop makes every host reap its own
    // worktrees from forge state on a cadence, independent of who merged.
    //
    // Multi-workspace like the health gate / ranking refresh: it walks
    // `effective_roots()` each tick, so cleanup is NOT limited to the daemon's
    // attached workspace (a daemon attached to `~/GitHub/anvil` still reaps
    // `~/GitHub/loom` when that repo is registered).
    //
    // Default-ON (like the ranking refresh, unlike the work finder / gate): it
    // restores an already-documented behavior, and its removal decision is
    // `worktree_ops::clean::classify_worktree` under `--safe, !--force` PLUS a
    // `.loom-managed` sentinel requirement the interactive CLI does not apply —
    // i.e. a strict subset of what `loom-daemon clean --safe` would remove.
    // Opt out with `LOOM_WORKTREE_REAPER=0` / `autonomous.worktreeReaper.enabled=false`.
    //
    // The same loop also carries the orphaned-PROCESS reaper (#5110): an agent's
    // background process tree can outlive the agent, reparent to `systemd`, and
    // keep working forever (one such driver held a worker at load 65 for 5h52m).
    // Both passes answer the same question about the same worktrees — "does
    // anything still own issue-N?" — so they share a tick. Each has its own
    // enable knob, and either one being on is enough to start the loop.
    //
    // It also carries the primary-checkout reaper (#5268): a registered root's
    // OWN `HEAD` can get parked on a dead branch (no PR ever opened, or a PR
    // that closed without merging) and stay there forever — nothing else
    // touches the primary checkout's branch. Same tick, same "each has its own
    // enable knob, any one being on starts the loop" shape.
    let worktree_reaper_config = worktree_reaper::read_worktree_reaper_config(&sweep_workspace);
    let process_reaper_config = orphan_process_reaper::read_config(&sweep_workspace);
    let primary_checkout_reaper_config = primary_checkout_reaper::read_config(&sweep_workspace);
    let worktree_reaper_on = worktree_reaper::resolve_enabled(&worktree_reaper_config);
    let process_reaper_on = orphan_process_reaper::resolve_enabled(&process_reaper_config);
    let primary_checkout_reaper_on =
        primary_checkout_reaper::resolve_enabled(&primary_checkout_reaper_config);
    let _worktree_reaper_handle =
        if worktree_reaper_on || process_reaper_on || primary_checkout_reaper_on {
            let interval = worktree_reaper::resolve_interval(&worktree_reaper_config);
            log::info!(
                "worktree_reaper: enabled (multi-workspace, interval={}s, worktrees={}, \
                 orphan_processes={}, primary_checkout={})",
                interval.as_secs(),
                worktree_reaper_on,
                process_reaper_on,
                primary_checkout_reaper_on
            );
            Some(worktree_reaper::spawn_multi_worktree_reaper_task(
                sweep_workspace.clone(),
                interval,
            ))
        } else {
            log::debug!(
                "worktree_reaper: disabled (set LOOM_WORKTREE_REAPER=0 or \
                 autonomous.worktreeReaper.enabled=false to opt out; \
                 LOOM_ORPHAN_PROCESS_REAPER=0 / autonomous.processReaper.enabled=false \
                 for the orphaned-process pass; LOOM_PRIMARY_CHECKOUT_REAPER=0 / \
                 autonomous.primaryCheckoutReaper.enabled=false for the primary-checkout pass)"
            );
            None
        };

    // Install/host invariant self-check (#5035): periodically verify the
    // install/host invariants the daemon's own operation depends on — the MCP
    // bundle loads, `.loom/runtimes/` is populated for the configured runtimes,
    // and the token `.ranking` is fresh — repairing the mechanically-safe drift
    // and filing one deduped issue per non-repairable condition. **Default-off**
    // (FLAGS-OFF) and **report-only by default** even when enabled, so the check
    // can be trusted before it is allowed to act. Same multi-workspace
    // spawn_blocking shape as the reaper; the checks are filesystem reads and
    // any repair is timeout-guarded, so a pass can never wedge dispatch.
    let install_self_check_config = install_self_check::read_config(&sweep_workspace);
    let _install_self_check_handle =
        if install_self_check::resolve_enabled(&install_self_check_config) {
            let interval = install_self_check::resolve_interval(&install_self_check_config);
            let repair = install_self_check::resolve_repair(&install_self_check_config);
            log::info!(
                "install_self_check: enabled (multi-workspace, interval={}s, repair={})",
                interval.as_secs(),
                repair
            );
            Some(install_self_check::spawn_multi_install_self_check_task(
                sweep_workspace.clone(),
                interval,
            ))
        } else {
            log::debug!(
                "install_self_check: disabled (set LOOM_INSTALL_SELF_CHECK=1 or \
                 autonomous.installSelfCheck.enabled=true to opt in)"
            );
            None
        };

    // Declared-cadence liveness heartbeat (Issue #4011): the daemon touches
    // `<loom_dir>/daemon.heartbeat` on a fixed cadence so a host-side watchdog
    // (`loom-daemon-watchdog.sh`, a second StartInterval launchd job) can detect
    // "a daemon should be running but isn't" WITHOUT talking to the daemon — a
    // dead daemon cannot report its own death, so the reporter must live outside
    // this process. Default-ON like the token-ranking refresh / watch-monitor
    // loops (it only writes a small bookkeeping file with no dispatch side
    // effect); opt out with `LOOM_DAEMON_HEARTBEAT=0` /
    // `autonomous.heartbeat.enabled=false`. We deliberately do NOT reuse the
    // token-ranking `.ranking` mtime as an accidental heartbeat: that is a
    // config-disableable side effect, so a detector keyed to it would silently
    // stop working when that loop is turned off.
    let heartbeat_config = daemon_heartbeat::read_heartbeat_config(&sweep_workspace);
    // Resolved once here so the healing marker below (#4331) and the running
    // heartbeat loop agree on the cadence the watchdog derives its staleness
    // threshold from — even if the loop itself is disabled.
    let heartbeat_interval = daemon_heartbeat::resolve_interval(&heartbeat_config);
    let _heartbeat_handle = if daemon_heartbeat::resolve_enabled(&heartbeat_config) {
        log::info!("daemon_heartbeat: enabled (interval={}s)", heartbeat_interval.as_secs());
        daemon_heartbeat::spawn_heartbeat_task(heartbeat_interval)
    } else {
        log::debug!(
            "daemon_heartbeat: disabled (set LOOM_DAEMON_HEARTBEAT=1 or \
             autonomous.heartbeat.enabled=true to opt in)"
        );
        None
    };

    // Startup autonomy-desired marker healing (Issue #4331). The marker is the
    // durable "a daemon is EXPECTED on this host" signal the watchdog + status
    // key off (#4011). `loom-daemon restart` (#4054), the in-daemon self-update
    // loop, and a bare launchd/systemd relaunch all bring up a fresh daemon
    // WITHOUT re-running the start script's `write_intent_marker` — so an ABSENT
    // marker was never healed, leaving a supervised daemon running with crash
    // protection silently disarmed. This single startup choke point covers all
    // three paths: if the daemon is supervised and the marker is absent, re-write
    // it. An unsupervised (`--foreground`/nohup/debug) run never writes one — it
    // must not arm the host-side pager for a process nothing will relaunch.
    match autonomy_marker::heal_on_startup(heartbeat_interval.as_secs()) {
        Some(autonomy_marker::HealOutcome::Healed(path)) => log::warn!(
            "autonomy_marker: HEALED an absent autonomy-desired marker at {} — a supervised \
             daemon was running with crash protection disarmed (restart-primitive / self-update / \
             bare relaunch never re-writes it). The watchdog and `loom-daemon status` now see this \
             daemon as EXPECTED again (#4331).",
            path.display()
        ),
        Some(autonomy_marker::HealOutcome::AlreadyPresent) => {
            log::debug!("autonomy_marker: marker already present — no healing needed (#4331)")
        }
        Some(autonomy_marker::HealOutcome::UnsupervisedSkip) => log::debug!(
            "autonomy_marker: unsupervised run (no LOOM_DAEMON_SUPERVISOR) — deliberately NOT \
             writing an autonomy-desired marker (#4331)"
        ),
        Some(autonomy_marker::HealOutcome::WriteFailed { path, error }) => log::warn!(
            "autonomy_marker: failed to heal the autonomy-desired marker at {} (logged, never \
             fatal; the daemon keeps running): {error} (#4331)",
            path.display()
        ),
        None => log::warn!(
            "autonomy_marker: could not resolve a loom dir (no LOOM_SOCKET_PATH / home) — \
             skipping marker healing for this run (#4331)"
        ),
    }

    // Watchdog-provisioning-guard loop (Issue #5405): #5343's
    // heal_watchdog_provisioning_gap() only fires as a side effect of
    // RE-RUNNING loom-daemon-start.sh — a host that was provisioned before
    // #5343 and simply keeps running forever never gets healed, because
    // nothing host-resident ever notices the gap. This periodic loop is that
    // host-resident notice: every interval it re-invokes
    // `loom-daemon-start.sh --heal-watchdog-only` (a narrow entry point that
    // performs ONLY the watchdog heal and can never reach the daemon-start
    // path, so it can never disturb this running daemon), which is a cheap
    // no-op on any host that already has a marker+watchdog pair and a pure
    // filesystem read on any host with no autonomy-desired marker at all.
    // Default-on like the marker healing above (crash-protection class, not
    // a FLAGS-OFF dispatch-affecting loop) — opt out with
    // LOOM_WATCHDOG_PROVISIONING_GUARD=0 or
    // autonomous.watchdogProvisioningGuard.enabled=false.
    let watchdog_guard_config = watchdog_provisioning_guard::read_config(&sweep_workspace);
    let _watchdog_provisioning_guard_handle =
        if watchdog_provisioning_guard::resolve_enabled(&watchdog_guard_config) {
            let interval = watchdog_provisioning_guard::resolve_interval(&watchdog_guard_config);
            Some(watchdog_provisioning_guard::spawn_watchdog_provisioning_guard_task(
                sweep_workspace.clone(),
                interval,
                watchdog_provisioning_guard::default_heal_timeout(),
            ))
        } else {
            log::debug!(
                "watchdog_provisioning_guard: disabled (set LOOM_WATCHDOG_PROVISIONING_GUARD=0 or \
                 autonomous.watchdogProvisioningGuard.enabled=false to opt out) (#5405)"
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
    // fallback for deployments with no always-on daemon.
    //
    // One task is spawned per [`role_runner::DEFAULT_ROLES`] entry,
    // unconditionally — each on its own multi-workspace loop
    // (`role_runner::spawn_multi_role_task`), mirroring the token-ranking
    // refresh loop's re-fan-out-every-tick shape. This intentionally does
    // **not** filter the spawned set through
    // `role_runner::resolve_roles(&role_runner_config)` (issue #5654):
    // `role_runner`'s own module doc and `resolve_roles`'s doc both describe
    // `autonomous.roleRunner.roles` as a **per-repo** allowlist, resolved
    // fresh inside `spawn_multi_role_task`'s loop from each *registered*
    // root's own config (`role_runner.rs`'s per-tick `resolve_roles(&config)`
    // check) — but until #5654, the SET OF LOOPS SPAWNED HERE was gated on
    // `resolve_roles(&role_runner_config)` where `role_runner_config` comes
    // from `sweep_workspace` alone (this daemon's own home/`LOOM_WORKSPACE`
    // directory, which is not even guaranteed to be one of the registered
    // fleet roots — see `WorkspaceRegistry::effective_roots`). A role absent
    // from *that one directory's* pinned `roles` list therefore never got a
    // loop at all, silently zeroing it out fleet-wide regardless of what any
    // of the other registered repos wanted — the root cause of "doctor is
    // never admitted on one host while hermit/auditor run" (#5654). Spawning
    // unconditionally for every `DEFAULT_ROLES` entry restores the
    // documented per-repo model: the home workspace's own
    // `roleRunner.roles` now only affects the home workspace's own
    // participation (via the existing per-root check), never any other
    // repo's.
    let role_runner_config = role_runner::read_role_runner_config(&sweep_workspace);
    let _role_runner_handles = if role_runner::resolve_enabled(&role_runner_config) {
        // Purely informational (#5654): the home workspace's own resolved
        // list no longer gates which loops are spawned below, but logging it
        // alongside the full `DEFAULT_ROLES` set makes a divergence between
        // the two visible at startup instead of only inferable from a
        // per-repo `DEBUG` line at tick time.
        let home_resolved = role_runner::resolve_roles(&role_runner_config);
        log::info!(
            "role_runner: enabled (multi-workspace, spawning all {} DEFAULT_ROLES loop(s): {}); \
             {}'s own resolved autonomous.roleRunner.roles = [{}] ({} entries) — informational \
             only, no longer gates which loops are spawned (#5654)",
            role_runner::DEFAULT_ROLES.len(),
            role_runner::DEFAULT_ROLES
                .iter()
                .map(|r| r.name)
                .collect::<Vec<_>>()
                .join(", "),
            sweep_workspace.display(),
            home_resolved
                .iter()
                .map(|r| r.name)
                .collect::<Vec<_>>()
                .join(", "),
            home_resolved.len(),
        );
        // Cross-host role-tick collision detection (#4623), the role-runner
        // counterpart of #4085's sweep-dispatch probe. Resolved per registered
        // root at tick time (a root can opt in/out on its own); this line
        // reports the daemon workspace's own resolution so an operator can
        // confirm the toggle took effect. Detection only — a detected
        // collision is logged/counted, never acted on.
        log::info!(
            "role_runner: cross-host collision detection {} for {} (#4623; \
             LOOM_ROLE_RUNNER_DETECT_COLLISIONS > autonomous.roleRunner.collisionDetection > \
             autonomous.collisionDetection.enabled)",
            if role_collision::resolve_detection_enabled(&sweep_workspace) {
                "ENABLED"
            } else {
                "disabled"
            },
            sweep_workspace.display()
        );
        let handles: Vec<_> = role_runner::DEFAULT_ROLES
            .iter()
            .map(|spec| {
                // #6204: log the resolved cadence *with the tier that supplied
                // it* (built-in / config / env). A uniform interval across
                // every role is the expected shape of an override — but with
                // no source in the line it reads as the per-role built-ins
                // having silently stopped applying, which is how #6204 was
                // filed against an inherited `LOOM_ROLE_RUNNER_INTERVAL_SECS`.
                let interval = role_runner::resolve_interval_for_role(spec, &role_runner_config);
                log::info!(
                    "{}",
                    role_runner::resolved_interval_log_line(
                        &sweep_workspace,
                        spec,
                        &role_runner_config
                    )
                );
                role_runner::spawn_multi_role_task(
                    *spec,
                    sweep_workspace.clone(),
                    interval,
                    drain_flag.clone(),
                    role_in_progress.clone(),
                )
            })
            .collect();
        Some(handles)
    } else {
        // #6469: this MUST land at `info!`, not `debug!` — a fleet running at
        // INFO (the normal level) silently boots with zero trace that role
        // loops are off, which is exactly the "the daemon is silent about the
        // single most consequential switch on the host" symptom the issue was
        // filed against. `log_role_runner_disabled` names the tier that
        // resolved the off state and the scope, mirroring the enabled
        // branch's own `log::info!` above.
        role_runner::log_role_runner_disabled(&sweep_workspace, &role_runner_config);
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

    // Autonomous self-update loop (Issue #4055 — Phase 3 of #4017). Opt-in via
    // `LOOM_AUTO_UPDATE` / `autonomous.autoUpdate.enabled`. When the daemon's own
    // source checkout advances past the commit this binary was built from, the
    // loop rebuilds + provisions (reusing `loom-daemon-update.sh --no-restart`)
    // and rolls onto the fresh binary via #4090's drain path — in-flight sweeps
    // finish first and survive in the registry. Gated on a clean tree, a settle
    // window, zero in-flight sweeps (`ipc::count_in_flight_sweeps`), and exponential
    // backoff with a terminal give-up state, all surfaced in `loom-daemon status`.
    //
    // Unlike the per-workspace loops above this is spawned exactly **once** for the
    // whole daemon (its subject is the daemon process itself — one binary, one
    // source checkout, one restart), NOT a `spawn_multi_*` per-workspace fan-out.
    // Config is read from the daemon's default workspace, like the sibling readers.
    // Default OFF (side effects on the running process). Cloned handles here because
    // `event_bus` is moved into `IpcServer::new` below.
    let auto_update_config = auto_update::read_auto_update_config(&sweep_workspace);
    let _auto_update_handle = if auto_update::resolve_enabled(&auto_update_config) {
        let interval = auto_update::resolve_interval(&auto_update_config);
        let settle = auto_update::resolve_settle(&auto_update_config);
        let defer_deadline = auto_update::resolve_defer_deadline(&auto_update_config);
        log::info!(
            "auto_update: enabled (interval={}s, settle={}s, deferDeadline={}s)",
            interval.as_secs(),
            settle.as_secs(),
            defer_deadline.as_secs()
        );
        let probe = auto_update::ScriptAutoUpdateProbe::new(
            workspace_pool.clone(),
            sweep_workspace.clone(),
        );
        let trigger = auto_update::IpcDrainTrigger::new(
            drain_state.clone(),
            workspace_pool.clone(),
            sweep_workspace.clone(),
            event_bus.clone(),
            tokio::runtime::Handle::current(),
        );
        let status = std::sync::Arc::new(auto_update::AutoUpdateStatus::new(true));
        Some(auto_update::spawn_auto_update_task(
            probe,
            trigger,
            status,
            interval,
            settle,
            defer_deadline,
        ))
    } else {
        log::debug!(
            "auto_update: disabled (set LOOM_AUTO_UPDATE=1 or autonomous.autoUpdate.enabled=true to opt in)"
        );
        None
    };

    // Independent, opt-in idle exit (#4467). The daemon only exits; the host
    // guard retains sole authority to power off.
    let idle_exit_config = idle_exit::read_config(&sweep_workspace);
    let _idle_exit_handle = if idle_exit::resolve_enabled(&idle_exit_config) {
        let minutes = idle_exit::resolve_minutes(&idle_exit_config);
        let starvation = idle_exit::resolve_starvation(&idle_exit_config);
        if loom_daemon::ipc::detect_supervisor().as_deref() == Some("launchd") {
            log::warn!(
                "idle_exit: ENABLED under launchd; KeepAlive:SuccessfulExit relaunches exit(0), \
                 so idle exit is meaningful only under on-failure-style supervision"
            );
        }
        log::info!("idle_exit: enabled (idle_minutes={minutes}, on_token_starvation={starvation})");
        Some(idle_exit::spawn_task(
            idle_exit_config,
            sweep_workspace.clone(),
            workspace_pool.clone(),
            role_in_progress.clone(),
            event_bus.clone(),
            socket_path.clone(),
        ))
    } else {
        log::debug!("idle_exit: disabled (opt in with autonomous.idleExit.enabled=true)");
        None
    };

    // Pluggable telemetry exporter (#4705, epic #4702 Phase 1). Off by
    // default (FLAGS-OFF, `observability.enabled=true` to opt in) — a bus
    // subscriber plus a queue-drain sender, mirroring the idle_exit wiring
    // immediately above. `Instant::now()` here approximates daemon uptime for
    // the periodic `host.health` sample; see `observability::spawn_task`'s
    // doc comment for why an exact daemon-start timestamp is not threaded
    // through for this.
    let observability_config = observability::read_config(&sweep_workspace);
    let _observability_handles = observability::spawn_task(
        &observability_config,
        sweep_workspace.clone(),
        &event_bus,
        std::time::Instant::now(),
        workspace_pool.clone(),
    );

    // Start IPC server. `workspace_health_states` is threaded in so the
    // `DaemonStatus` request can report each registered repo's own halt state
    // (#3930), and `sweep_workspace` is the `effective_roots` fallback for the
    // per-repo status breakdown — the same values the work-finder and gate loop
    // share above. `credential_preflight` (#4005) is threaded in so
    // `DaemonStatus` can report the startup credential resolution computed
    // once above, without reading logs.
    let server = IpcServer::new(
        socket_path.clone(),
        tm,
        activity_db,
        sweep_registry,
        event_bus,
        workspace_health_states.clone(),
        workspace_pool.clone(),
        sweep_workspace.clone(),
        credential_preflight,
        drain_state.clone(),
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
        // Issue #6129: name this explicitly in the daemon's own log, not just
        // in loom-daemon-stop.sh's stdout — a raw `systemctl --user stop
        // loom-daemon` / `launchctl bootout` never runs that script, so its
        // stdout notice is the only place an operator who bypassed the
        // wrapper would ever see this. In-flight sweep children and
        // scheduled role-agent ticks (Champion/Curator/Judge/Doctor/Guide/…)
        // are intentionally left running by this stop (see the module-level
        // "survive, don't drain" comment above) — on a Linux systemd --user
        // host they can also be architecturally DETACHED from this process
        // (a `systemd-run --user --scope` scope parented to the user
        // manager, not to this daemon; issue #5111's CPU-quota mechanism).
        // `loom-daemon-quiesce.sh` is the explicit, single-command way to
        // stop dispatch AND every one of those children, the same way on
        // launchd and systemd.
        log::info!(
            "In-flight sweeps and role agents (if any) are intentionally left running by this stop. \
             To also stop them, run: .loom/scripts/cli/loom-daemon-quiesce.sh (issue #6129)."
        );
        // Exit code carries shutdown intent (Issue #4054, Curator Finding 1):
        // under launchd `KeepAlive:SuccessfulExit` a clean exit(0) triggers a
        // relaunch, so a signal-driven stop MUST exit non-zero — otherwise an
        // operator stop would race launchd into relaunching the daemon. Only the
        // `RestartDaemon` primitive exits 0. SIGTERM => 143, SIGINT => 130.
        let code = if signal_name == "SIGTERM" {
            loom_daemon::ipc::EXIT_SIGTERM
        } else {
            loom_daemon::ipc::EXIT_SIGINT
        };
        // #6388: this process cannot know operator INTENT from a signal alone
        // (a stray SIGTERM — e.g. an unrelated test run — looks identical to
        // a deliberate stop here) — only the autonomy-desired marker records
        // intent, and only loom-daemon-watchdog.sh reads it. So this log line
        // states the fact ("signal received"), not a judgment about why.
        log::info!(
            "Socket cleaned up, exiting {code} ({signal_name} received — no supervised relaunch)"
        );
        std::process::exit(code);
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

/// Resolve the daemon's loom directory: the parent of `LOOM_SOCKET_PATH` when
/// that env var is set (test isolation), else `$HOME/.loom`. Pure — no side
/// effects (no directory creation), so it's safe to call from `setup_logging()`
/// (which runs before `main()`'s own `loom_dir`/`socket_path` resolution block)
/// as well as from `main()` and `resolve_socket_path()` without duplicating the
/// branching logic three times over.
pub(crate) fn resolve_loom_dir() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("LOOM_SOCKET_PATH") {
        let socket_path = PathBuf::from(path);
        let loom_dir = socket_path
            .parent()
            .ok_or_else(|| anyhow!("Socket path has no parent directory"))?
            .to_path_buf();
        return Ok(loom_dir);
    }
    let loom_dir = dirs::home_dir()
        .ok_or_else(|| anyhow!("No home directory"))?
        .join(".loom");
    Ok(loom_dir)
}

/// Resolve the daemon log file path: `LOOM_DAEMON_LOG` (full override) when
/// set, else `<loom dir>/daemon.log` derived from [`resolve_loom_dir`]. This
/// means `LOOM_SOCKET_PATH`-style test isolation covers the log file for free
/// — a test daemon pointed at a tempdir socket also logs into that tempdir,
/// never into the operator's `~/.loom/daemon.log` (Issue #4010). Precedence is
/// env > default only; see #4010 for why a config tier is out of scope here
/// (`setup_logging()` runs before workspace/config resolution).
fn resolve_log_path() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("LOOM_DAEMON_LOG") {
        return Ok(PathBuf::from(path));
    }
    Ok(resolve_loom_dir()?.join("daemon.log"))
}

fn setup_logging() -> Result<()> {
    let log_path = resolve_log_path()?;

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

#[cfg(test)]
mod resolve_paths_tests {
    //! Tests for [`resolve_loom_dir`] and [`resolve_log_path`] (Issue #4010).
    //!
    //! `setup_logging()` itself is deliberately NOT unit-tested here: it calls
    //! `env_logger::Builder::...init()`, which panics if called a second time
    //! in the same process. Splitting the pure path-resolution logic out into
    //! these two functions is exactly what makes it testable at all.
    //!
    //! Both tests mutate the process-global `LOOM_SOCKET_PATH` / `LOOM_DAEMON_LOG`
    //! env vars, so they're `#[serial]` (the crate already depends on
    //! `serial_test` for this exact purpose — see `dispatch_tests` below) to
    //! avoid racing other env-mutating tests in the same binary.
    use super::{resolve_log_path, resolve_loom_dir};
    use serial_test::serial;

    #[test]
    #[serial]
    fn resolve_loom_dir_defaults_to_home_loom() {
        std::env::remove_var("LOOM_SOCKET_PATH");
        let expected = dirs::home_dir().expect("home dir").join(".loom");
        assert_eq!(resolve_loom_dir().expect("resolve"), expected);
    }

    #[test]
    #[serial]
    fn resolve_loom_dir_honors_socket_path_override() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("loom-daemon.sock");
        std::env::set_var("LOOM_SOCKET_PATH", &socket_path);

        let resolved = resolve_loom_dir().expect("resolve");

        std::env::remove_var("LOOM_SOCKET_PATH");
        assert_eq!(resolved, dir.path());
    }

    #[test]
    #[serial]
    fn resolve_log_path_defaults_to_home_loom_daemon_log() {
        std::env::remove_var("LOOM_SOCKET_PATH");
        std::env::remove_var("LOOM_DAEMON_LOG");
        let expected = dirs::home_dir()
            .expect("home dir")
            .join(".loom")
            .join("daemon.log");
        assert_eq!(resolve_log_path().expect("resolve"), expected);
    }

    #[test]
    #[serial]
    fn resolve_log_path_honors_loom_daemon_log_override() {
        std::env::remove_var("LOOM_SOCKET_PATH");
        std::env::set_var("LOOM_DAEMON_LOG", "/tmp/some/d/daemon.log");

        let resolved = resolve_log_path().expect("resolve");

        std::env::remove_var("LOOM_DAEMON_LOG");
        assert_eq!(resolved, std::path::PathBuf::from("/tmp/some/d/daemon.log"));
    }

    /// `LOOM_DAEMON_LOG` must win even when `LOOM_SOCKET_PATH` is also set —
    /// the explicit log override always takes precedence over the derived path.
    #[test]
    #[serial]
    fn resolve_log_path_daemon_log_wins_over_socket_path_derivation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("loom-daemon.sock");
        std::env::set_var("LOOM_SOCKET_PATH", &socket_path);
        std::env::set_var("LOOM_DAEMON_LOG", "/tmp/explicit/daemon.log");

        let resolved = resolve_log_path().expect("resolve");

        std::env::remove_var("LOOM_SOCKET_PATH");
        std::env::remove_var("LOOM_DAEMON_LOG");
        assert_eq!(resolved, std::path::PathBuf::from("/tmp/explicit/daemon.log"));
    }
}
