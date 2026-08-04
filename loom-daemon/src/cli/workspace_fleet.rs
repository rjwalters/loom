//! `loom-daemon calibrate` / `workspace` / `fleet` (add-worker, drain)
//! handlers (Issue #4712 — split out of `main.rs`). `fleet status` lives in
//! `cli::status` (it needs the async runtime for the local-host round-trip).

use anyhow::{anyhow, Context, Result};
use std::path::PathBuf;
use std::time::Duration;

use loom_daemon::sweep_registry::SweepRegistryConfig;

use crate::{FleetAction, WorkspaceAction};

/// Handle `loom-daemon calibrate` (issue #4390; measurement-only since #4512):
/// measure the host + the currently-resolved knobs and print the
/// `min(token axis, disk, maxConcurrent)` breakdown, which term binds, and the
/// one-line tuning reading. See `loom_daemon::calibrate`.
///
/// `--write` is **accepted but ignored** with a deprecation notice: #4512
/// removed the CPU-headroom term the recommendation was derived from, so there
/// is nothing to compute and write — `maxConcurrent` is a per-machine knob the
/// operator tunes from these measurements. Keeping the flag (rather than
/// deleting it) means an existing `calibrate --write` in a script keeps exiting
/// 0 instead of failing on an unknown argument.
///
/// The retired-knob deprecation notice is printed to **stderr** here rather than
/// left to `work_finder::warn_deprecated_cpu_knobs`'s `log::warn!`: CLI
/// subcommands return from `main` before `setup_logging()` runs, so on this path
/// a log warning would go nowhere at all. stderr also keeps `--json`'s stdout
/// payload clean for machine consumers.
pub(crate) fn handle_calibrate_command(workspace: &str, write: bool, json: bool) -> Result<()> {
    use loom_daemon::calibrate;
    use loom_daemon::worktree_ops::repo;

    let repo_root = repo::resolve_repo_root(workspace)?;
    let measurements = calibrate::measure(&repo_root);

    if let Some(notice) = loom_daemon::work_finder::deprecated_cpu_knob_notice(
        &loom_daemon::work_finder::read_work_finder_config(&repo_root),
    ) {
        eprintln!("loom-daemon calibrate: {notice}");
    }

    if write {
        eprintln!(
            "loom-daemon calibrate: --write is deprecated and ignored (#4512). calibrate no \
             longer derives a recommendation: the CPU-headroom admission term it was based on was \
             removed, and autonomous.workFinder.maxConcurrent is now a per-machine knob you tune \
             from the measurements below. Edit .loom/config.json directly, then restart the daemon."
        );
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&calibrate::report_json(&measurements))?);
    } else {
        print!("{}", calibrate::report_human(&measurements));
    }

    Ok(())
}

/// Handle `loom-daemon fleet …` (epic #4340). Thin clap→module wiring: all
/// bootstrap logic (the step planner/executor, shell templates, fleet registry)
/// lives in [`loom_daemon::fleet`].
pub(crate) fn handle_fleet_command(action: FleetAction) -> Result<()> {
    use loom_daemon::fleet::add_worker::{self, AddWorkerConfig};
    use loom_daemon::fleet::drain::{self, DrainConfig};
    use loom_daemon::fleet::spice_runner::{self, SpiceBootstrapConfig};

    match action {
        FleetAction::AddWorker {
            ssh_host,
            repo,
            priority,
            pat_file,
            accounts_env,
            loom_repo,
            safehouse,
            safehouse_tailnet_auth_key_file,
            safehouse_secrets_file,
            safehouse_repo_url,
            safehouse_homeserver_url,
            safehouse_room,
            safehouse_personas,
            safehouse_invite_exec,
            idle_shutdown_minutes,
            dry_run,
        } => {
            let config = AddWorkerConfig {
                ssh_host,
                repos: repo,
                priority,
                dry_run,
                loom_repo_url: loom_repo,
                pat_file: pat_file.map(PathBuf::from),
                accounts_env_file: accounts_env.map(PathBuf::from),
                safehouse_enabled: safehouse,
                idle_shutdown_minutes,
                safehouse_tailnet_auth_key_file: safehouse_tailnet_auth_key_file.map(PathBuf::from),
                safehouse_secrets_file: safehouse_secrets_file.map(PathBuf::from),
                safehouse_repo_url,
                safehouse_homeserver_url,
                safehouse_room,
                safehouse_personas,
                safehouse_invite_exec,
            };
            add_worker::run(&config)
        }
        FleetAction::BootstrapSpice {
            ssh_host,
            ngspice_repo_url,
            ngspice_ref,
            skip_xyce,
            xyce_repo_url,
            xyce_ref,
            trilinos_repo_url,
            trilinos_ref,
            gf180mcu_repo_url,
            gf180mcu_ref,
            gf180mcu_models_path,
            sky130_repo_url,
            sky130_ref,
            sky130_models_path,
            dry_run,
        } => {
            let config = SpiceBootstrapConfig {
                ssh_host,
                dry_run,
                ngspice_repo_url,
                ngspice_ref,
                install_xyce: !skip_xyce,
                xyce_repo_url,
                xyce_ref,
                trilinos_repo_url,
                trilinos_ref,
                gf180mcu_repo_url,
                gf180mcu_ref,
                gf180mcu_models_path,
                sky130_repo_url,
                sky130_ref,
                sky130_models_path,
            };
            spice_runner::run(&config)
        }
        FleetAction::Status { .. } => {
            // Routed directly in `main()` (it needs the async runtime for the
            // local host's in-process socket round-trip), never dispatched
            // through this sync handler.
            unreachable!("Fleet Status is handled in main() before handle_cli_command")
        }
        FleetAction::Drain {
            ssh_host,
            timeout,
            force_after_timeout,
            json,
        } => {
            // Resolved once, from the operator's own cwd (mirrors
            // `WorkspacePool::start_safehouse_narration`'s
            // `safehouse::resolve_config(repo_root)` call shape) — see
            // `fleet::drain::flush_safehouse`'s doc comment for the real
            // supervised-stop flush check this now gates on (#3998).
            let cwd = std::env::current_dir().context("resolving cwd for safehouse config")?;
            let safehouse_enabled = loom_daemon::safehouse::resolve_config(&cwd).enabled;

            let poll_interval = Duration::from_secs(drain::DEFAULT_POLL_INTERVAL_SECS);
            let max_polls = ((timeout / drain::DEFAULT_POLL_INTERVAL_SECS.max(1)) + 12) as u32;
            let config = DrainConfig {
                ssh_host,
                timeout_secs: timeout,
                force_after_timeout,
                poll_interval,
                max_polls,
                safehouse_enabled,
                json,
            };
            let report = drain::run(&config)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print!("{}", report.render_human());
            }
            let code = report.exit_code();
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
    }
}

/// Refuse a daemon-admin CLI action when the **invoking** repo (resolved
/// from cwd the same way `calibrate`/`tokens select` already resolve it —
/// see [`loom_daemon::worktree_ops::repo::resolve_repo_root`]) declares
/// `daemon.delegatedTo` (issue #5345). Prints a pointer to the delegate repo
/// on stderr and exits non-zero — mirrors the existing
/// `std::process::exit(1)` convention `handle_tokens_command`'s `Bootstrap`
/// error arms already use.
///
/// Soft-fails to "allowed" (returns without exiting) when cwd is not inside
/// any Loom repository, or when no delegation is configured — the default-off,
/// behavior-preserving case for every repo today.
fn refuse_if_daemon_admin_delegated(action_desc: &str) {
    let Ok(repo_root) = loom_daemon::worktree_ops::repo::resolve_repo_root(".") else {
        return;
    };
    if let Some(delegate) = loom_daemon::config_resolver::daemon_delegated_to(&repo_root) {
        eprintln!("error: daemon admin is delegated to {delegate} — perform {action_desc} there.");
        std::process::exit(1);
    }
}

/// Handle the `workspace` subcommand — mutate/inspect the machine-level
/// workspace registry (`~/.loom/workspaces.json`) directly on the filesystem.
/// This runs whether or not the daemon is up; a running daemon re-reads the
/// same file on its next tick (hot-apply), and its `RegisterWorkspace` /
/// `DeregisterWorkspace` / `ListWorkspaces` IPC handlers touch the same file.
///
/// `Add`/`SetPriority`/`Remove` are gated by
/// [`refuse_if_daemon_admin_delegated`] (issue #5345) — `List` is read-only
/// and is deliberately never gated.
pub(crate) fn handle_workspace_command(action: WorkspaceAction) -> Result<()> {
    use loom_daemon::workspace_registry::{AddOutcome, WorkspaceRegistry};

    let path = loom_daemon::workspace_registry::default_registry_path()?;

    match action {
        WorkspaceAction::Add {
            path: repo_path,
            priority,
            config_overrides,
        } => {
            refuse_if_daemon_admin_delegated("workspace registration");
            let overrides = match config_overrides {
                Some(raw) => Some(
                    serde_json::from_str::<serde_json::Value>(&raw)
                        .map_err(|e| anyhow!("--config-overrides is not valid JSON: {e}"))?,
                ),
                None => None,
            };
            let mut registry = WorkspaceRegistry::load(&path)?;
            // Mark the canonical workspace root trusted in ~/.claude.json as
            // part of registration (issue #5314) — merges, never clobbers,
            // and is idempotent on an already-registered workspace so a
            // pre-existing registry entry self-heals on the next `workspace
            // add` of the same path.
            let claude_state_path = loom_daemon::terminal::claude_config_state_path();
            match registry.add_and_trust(
                std::path::Path::new(&repo_path),
                overrides,
                priority,
                &claude_state_path,
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
                    // Issue #4027: `.git`/`.loom` alone does not mean the
                    // `/loom:sweep` slash command is installed — those files
                    // are install-not-committed (gitignored), so a bare
                    // `git clone` passes the `looks_like_workspace` check
                    // above while still being undispatchable. Dispatching
                    // into it insta-crashes on `Unknown command:
                    // /loom:sweep`, and because the reaper reverts
                    // `loom:building` -> `loom:issue` on that insta-crash,
                    // the work-finder re-dispatches every tick — an infinite
                    // token-burning loop. `dispatch()` itself now refuses
                    // this case (the load-bearing fix), but warn here too so
                    // the operator sees it at registration time, before any
                    // dispatch is attempted.
                    if !SweepRegistryConfig::new(canonical.clone()).has_sweep_command() {
                        eprintln!(
                            "  warning: {} has no .claude/commands/loom/sweep.md — the \
                             /loom:sweep command is not installed there. Dispatch into this \
                             workspace will be refused until you run `loom-daemon init {}`.",
                            canonical.display(),
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
            refuse_if_daemon_admin_delegated("workspace priority changes");
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
            refuse_if_daemon_admin_delegated("workspace removal");
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
