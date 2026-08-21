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
            feed_egress,
            feed_egress_ingest_key_file,
            feed_egress_sink_url,
            feed_egress_deny_patterns,
            feed_egress_delay_seconds,
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
                feed_egress_enabled: feed_egress,
                feed_egress_ingest_key_file: feed_egress_ingest_key_file.map(PathBuf::from),
                feed_egress_sink_url,
                feed_egress_deny_patterns,
                feed_egress_delay_seconds,
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
        FleetAction::Roll { .. } => {
            // Routed directly in `main()` (it needs the async runtime for the
            // per-host `tokio::time::timeout` fanout, #5504), never
            // dispatched through this sync handler.
            unreachable!("Fleet Roll is handled in main() before handle_cli_command")
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

/// Auto-init a just-registered workspace that has no `/loom:sweep` command
/// installed (Issue #5682, "auto-init" — chosen over the "fail loudly +
/// `--force`" alternative the issue also offered, since there is no reading
/// of `workspace add` where the caller actually wants a
/// registered-but-undispatchable root left behind).
///
/// No-op (returns `Ok(())` without printing anything) when `sweep.md` is
/// already present — the common case. Otherwise runs the same
/// `loom_daemon::init::initialize_workspace` scaffolding `loom-daemon init`
/// would, against the `"defaults"` directory (identical to
/// [`crate::cli::misc_cmds::run_init`]'s own default `--defaults` value, so
/// this resolves the same way `loom-daemon init <path>` would from the same
/// cwd — dev checkout, git-root-relative, or the machine-level payload; see
/// `init::git::resolve_defaults_path`).
///
/// Scoped to targets that are **already git repositories**: `git init`-ed but
/// missing `sweep.md` is exactly the undispatchable-forever root issue #5682
/// describes, and it is the only shape `initialize_workspace` can actually
/// repair (its own `validate_git_repository` rejects anything else). A target
/// with no `.git` at all keeps the long-standing warn-and-continue behavior of
/// `workspace add` — the caller's `looks_like_workspace` warning already covers
/// the "confirm this is a Loom-managed repo" case, and `dispatch()` refuses
/// such a root regardless. Hard-failing there instead regressed the
/// pre-existing (#5345) `workspace_add_behaves_unchanged_with_no_delegation_configured`
/// integration test, which registers a plain directory and asserts exit 0.
///
/// Returns `Err` only when auto-init of a *git* target fails — the
/// registration has already been persisted by the caller at that point, so
/// that failure is reported as a hard, non-zero exit rather than silently
/// swallowed; the error message tells the operator to run `loom-daemon init`
/// manually.
///
/// Issue #6636: a successful auto-init writes the Loom surfaces straight into
/// the target's working tree as **uncommitted** files — `initialize_workspace`
/// never commits. On a tracked repo registered independently on multiple
/// hosts that means per-host drift until someone commits a canonical install,
/// and the generated files sit there as a standing risk of being swept into
/// an unrelated commit (e.g. a Builder's `git add -A`). The success message
/// below names both the consequence and the follow-up
/// (`install.sh --quick <path>` + a commit) so the operator does not have to
/// discover it the way issue #6636 did.
fn auto_init_missing_sweep_command(canonical: &std::path::Path) -> Result<()> {
    if SweepRegistryConfig::new(canonical.to_path_buf()).has_sweep_command() {
        return Ok(());
    }
    if !canonical.join(".git").exists() {
        eprintln!(
            "  warning: {} has no .git — skipping auto-init of the /loom:sweep command. \
             Dispatch will be refused for this root until you run `loom-daemon init {}` \
             inside a git repository there.",
            canonical.display(),
            canonical.display()
        );
        return Ok(());
    }
    let canonical_str = canonical
        .to_str()
        .ok_or_else(|| anyhow!("workspace path is not valid UTF-8: {}", canonical.display()))?;
    println!(
        "  {} has no .claude/commands/loom/sweep.md — running `loom-daemon init` to install it...",
        canonical.display()
    );
    match loom_daemon::init::initialize_workspace(canonical_str, "defaults", false) {
        Ok(_report) => {
            println!(
                "  Initialized {} — /loom:sweep is now installed; dispatch is no longer refused.",
                canonical.display()
            );
            println!(
                "  Note: the installed Loom surfaces are UNCOMMITTED in {0} — every host that \
                 registers this repo repeats this step independently, so they can drift until \
                 someone commits a canonical install, and an unrelated `git add -A` in this repo \
                 could sweep them in by accident. Follow up with `install.sh --quick {0}` (from a \
                 Loom checkout) and commit the result.",
                canonical.display()
            );
            Ok(())
        }
        Err(e) => Err(anyhow!(
            "workspace {} was registered, but auto-init failed: {e}\n  The workspace remains \
             registered without the /loom:sweep command installed — dispatch will be refused \
             until you run `loom-daemon init {}` manually.",
            canonical.display(),
            canonical.display()
        )),
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
            no_init,
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
                    // this case (the load-bearing fix).
                    //
                    // Issue #5682: a stderr-only warning here was too easy to
                    // miss — the command still exits 0, and the registered
                    // root then sits in `status` indistinguishable from a
                    // healthy idle repo forever. Auto-init instead: there is
                    // no reading of `workspace add` where the caller actually
                    // wants a registered-but-undispatchable root, so run the
                    // same scaffolding `loom-daemon init` would — but only for
                    // a target that is already a git repo (the shape
                    // `initialize_workspace` can actually repair). A target
                    // with no `.git` keeps the warn-and-continue behavior of
                    // the `looks_like_workspace` warning just above. If
                    // auto-init of a git target fails, surface that as a hard,
                    // non-zero-exit error — the registration already landed
                    // (see `registry.save` above), so the failure here is
                    // reported, not silently swallowed.
                    //
                    // Issue #5788: `--no-init` opts a caller out of this side
                    // effect entirely — registration only. Callers like
                    // `migrate-consumer.sh` reach this point having already
                    // put the repo in the correct daemon-model shape
                    // themselves (tombstones removed, install-metadata.json
                    // re-stamped, CLAUDE.md's marker section refreshed); a
                    // fresh first-time-install layered on top would clobber
                    // all three.
                    if no_init {
                        println!(
                            "  --no-init: skipping /loom:sweep auto-init for {}",
                            canonical.display()
                        );
                    } else {
                        auto_init_missing_sweep_command(&canonical)?;
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

#[cfg(test)]
mod tests {
    use super::auto_init_missing_sweep_command;
    use loom_daemon::sweep_registry::SweepRegistryConfig;
    use std::process::Command;
    use tempfile::TempDir;

    /// A bare `git init` directory: `looks_like_workspace()` would already
    /// pass (it has `.git`), but `/loom:sweep` is not installed — exactly the
    /// gap issue #5682 describes.
    fn git_init_dir() -> TempDir {
        let tmp = TempDir::new().unwrap();
        let status = Command::new("git")
            .arg("init")
            .arg("--quiet")
            .arg(tmp.path())
            .status()
            .expect("git init should run");
        assert!(status.success(), "git init failed");
        tmp
    }

    #[test]
    fn auto_init_missing_sweep_command_is_a_noop_when_already_installed() {
        let tmp = git_init_dir();
        let sweep_dir = tmp.path().join(".claude").join("commands").join("loom");
        std::fs::create_dir_all(&sweep_dir).unwrap();
        std::fs::write(sweep_dir.join("sweep.md"), "# /loom:sweep\n").unwrap();

        assert!(
            SweepRegistryConfig::new(tmp.path().to_path_buf()).has_sweep_command(),
            "fixture setup: sweep.md should already exist"
        );
        let result = auto_init_missing_sweep_command(tmp.path());
        assert!(result.is_ok(), "no-op path should not error: {result:?}");
        // Untouched — auto-init must not have run (no other .loom/ scaffolding
        // was written for an already-installed workspace).
        assert!(
            !tmp.path().join(".loom").join("config.json").exists(),
            "auto-init must be a true no-op when sweep.md is already present"
        );
    }

    /// Issue #5682, acceptance criterion 2 (auto-init): a workspace missing
    /// `.claude/commands/loom/sweep.md` gets it installed by
    /// `auto_init_missing_sweep_command` rather than being left with only a
    /// stderr warning. `resolve_defaults_path("defaults")` falls back to
    /// this crate's own git-root-relative `defaults/` (this test runs inside
    /// the loom repo itself), so no fixture `defaults/` directory needs to be
    /// constructed here.
    #[test]
    fn auto_init_missing_sweep_command_installs_sweep_md_on_a_bare_git_repo() {
        let tmp = git_init_dir();
        assert!(
            !SweepRegistryConfig::new(tmp.path().to_path_buf()).has_sweep_command(),
            "fixture setup: sweep.md should not exist yet"
        );

        let result = auto_init_missing_sweep_command(tmp.path());
        assert!(result.is_ok(), "auto-init should succeed on a valid git repo: {result:?}");
        assert!(
            SweepRegistryConfig::new(tmp.path().to_path_buf()).has_sweep_command(),
            "sweep.md should be installed after auto-init"
        );
    }

    /// A target with no `.git` at all is **not** the shape issue #5682
    /// describes (its repro is a `git init`-ed directory), and
    /// `initialize_workspace` cannot repair it anyway —
    /// `validate_git_repository` rejects it. Auto-init must therefore skip it
    /// and let `workspace add` keep succeeding exactly as it always has: the
    /// caller's `looks_like_workspace` warning already covers the case, and
    /// `dispatch()` refuses such a root regardless. Hard-failing here instead
    /// regressed the pre-existing (#5345) integration test
    /// `workspace_add_behaves_unchanged_with_no_delegation_configured`.
    #[test]
    fn auto_init_missing_sweep_command_skips_a_non_git_directory_without_failing() {
        let tmp = TempDir::new().unwrap();
        // No `git init` — deliberately not a git repository.
        let result = auto_init_missing_sweep_command(tmp.path());
        assert!(
            result.is_ok(),
            "auto-init must warn-and-continue (not hard-fail) on a non-git directory: {result:?}"
        );
        // And it must not have attempted (or half-applied) any scaffolding.
        assert!(
            !SweepRegistryConfig::new(tmp.path().to_path_buf()).has_sweep_command(),
            "no sweep.md should have been installed into a non-git directory"
        );
        assert!(
            !tmp.path().join(".loom").join("config.json").exists(),
            "no .loom scaffolding should have been written into a non-git directory"
        );
    }
}
