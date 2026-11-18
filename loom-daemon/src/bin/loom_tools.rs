//! `loom-tools`: the tools-only entry point (#12938, epic #12708; design
//! `docs/design/loom-tools-cli-boundary.md` §6.2 step 4, §7.3, §10).
//!
//! Only `strip-ansi`, `checkpoint`, `config get` and `capabilities` (#12939), plus the
//! install trio `release-fetch`, `install-binary` and `resync-payload` (#12940), plus
//! `resolve-model` (#13878) and `resolve-role-model` (#13956): the
//! tools-only installer (`scripts/install/provision-tools.sh`) has no daemon to call. The handlers are the
//! library's own (the same functions `loom-daemon strip-ansi|checkpoint|config|resolve-model`
//! call), so there is no second copy. This crate links `loom_daemon` and not
//! `daemon_service`, which is private to the `loom-daemon` bin.
//!
//! `forge <read verb>` (#14395) is the family of forge reads: the same
//! `ForgeAction` tree and handler `loom-daemon forge` runs, minus the verbs
//! that write (#14396). It and `checkpoint read` (§10.3) are the commands
//! behind the mode gate: it resolves `mode` ([`tools_mode`])
//! before `forge_gateway::enable_cli_reads()` is considered, and enables it
//! only on the fleet route. `strip-ansi`, the other checkpoint actions and
//! `config get` never route, and `config get mode` must work with `mode` unset.

use std::io::Write;

use anyhow::Result;
use clap::{Parser, Subcommand};
use loom_daemon::cli::{
    config_get_dispatch, handle_checkpoint_command, handle_forge_read_command,
    handle_resolve_model_command, handle_resolve_role_model_command, handle_strip_ansi_command,
    CheckpointAction, ConfigArgs, ForgeReadCli, InstallBinaryCommand, ReleaseFetchArgs,
    ResolveModelArgs, ResolveRoleModelArgs, ResyncPayloadArgs,
};
use loom_daemon::install_compat;
use loom_daemon::tools_mode::{self, Route};

#[derive(Parser)]
#[command(
    name = "loom-tools",
    about = "Loom agent tools (strip-ansi, checkpoint, config get, capabilities, install, resolve-model, resolve-role-model, forge reads)"
)]
#[command(version = loom_daemon::self_update::BUILD_IDENTITY)]
struct ToolsCli {
    #[command(subcommand)]
    command: ToolsCommands,
}

#[derive(Subcommand)]
enum ToolsCommands {
    /// Strip ANSI noise from stdin, or deep-clean a captured log with `--file`.
    StripAnsi {
        /// Post-process a captured log file instead of filtering stdin.
        #[arg(long, value_name = "PATH")]
        file: Option<String>,
    },
    /// Manage builder checkpoints for progress tracking.
    Checkpoint {
        #[command(subcommand)]
        action: CheckpointAction,
    },
    /// Read the resolved effective config (`config get mode`).
    Config(ConfigArgs),
    /// Download, verify and print a release binary (`--bin loom-tools` for
    /// this one): the same checksum/signature/glibc verification as
    /// `loom-daemon release-fetch`.
    ReleaseFetch(ReleaseFetchArgs),
    /// Stage a binary beside its destination and publish it over it, keeping
    /// the previous one as `<dest>.previous` (same as `loom-daemon install-binary`).
    #[command(subcommand)]
    InstallBinary(InstallBinaryCommand),
    /// Resync a workspace's installed Loom from the payload embedded in this
    /// binary (same as `loom-daemon resync-payload`).
    ResyncPayload(ResyncPayloadArgs),
    /// Resolve a logical model tier/alias to the concrete model ID — the
    /// same arguments, stdout and exit codes as `loom-daemon resolve-model`
    /// (#13878), so `resolve-model.sh` works with no daemon binary installed.
    ResolveModel(ResolveModelArgs),
    /// Resolve a role's model for an attended (Task-tool) subagent spawn —
    /// the same arguments, stdout and exit codes as `loom-daemon
    /// resolve-role-model` (issue #13956), so `resolve-role-model.sh` works
    /// with no daemon binary installed.
    ResolveRoleModel(ResolveRoleModelArgs),
    /// The read-only `forge` verbs (`check-claim`, `wait-checks`, `issue`, ...):
    /// the same arguments, stdout, stdin and exit codes as `loom-daemon forge`
    /// (#14395), on the person's own forge credential. A verb that writes is
    /// refused with exit 64 (served by `loom-daemon forge` until #14396).
    Forge(ForgeReadCli),
    /// Print this binary's side of the install-compat contract as JSON: the
    /// same object `loom-daemon install-compat show --json` prints, so the
    /// shell wrappers carry no capability rules of their own.
    Capabilities {
        /// Accepted for symmetry with `install-compat show --json`; the
        /// output is always JSON.
        #[arg(long)]
        json: bool,
        /// A repo clone whose default-branch `.loom/install-metadata.json` to
        /// classify against this binary.
        #[arg(long, value_name = "PATH")]
        repo: Option<std::path::PathBuf>,
    },
}

/// `capabilities`: one JSON object on stdout. Read-only, no forge call.
fn capabilities(repo: Option<&std::path::Path>) -> Result<()> {
    let daemon = install_compat::DaemonCompat::this_binary(None).ok_or_else(|| {
        anyhow::anyhow!("this binary's version or SUPPORTS_INSTALLED is not MAJOR.MINOR.PATCH")
    })?;
    let installed = match repo {
        Some(repo) => install_compat::read_default_branch(repo)?,
        None => None,
    };
    let mut obj = install_compat::capabilities_json(&daemon, installed.as_ref());
    obj["binary"] = "loom-tools".into();
    println!("{obj}");
    Ok(())
}

/// Resolve `mode`, then (fleet route only) adopt the exported read routing.
fn mode_gate() -> Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let resolution =
        tools_mode::resolve(&tools_mode::inputs_from_env(&cwd)).map_err(anyhow::Error::msg)?;
    for note in &resolution.notes {
        eprintln!("{note}");
    }
    match resolution.route {
        Route::Fleet => loom_daemon::forge_gateway::enable_cli_reads(),
        // Nothing has read them yet; drop them so no later code honors them.
        Route::Local => {
            for var in &resolution.ignored_vars {
                std::env::remove_var(var);
            }
        }
    }
    Ok(())
}

fn run() -> Result<()> {
    match ToolsCli::parse().command {
        ToolsCommands::StripAnsi { file } => handle_strip_ansi_command(file.as_deref()),
        ToolsCommands::Checkpoint { action } => {
            if matches!(action, CheckpointAction::Read { .. }) {
                mode_gate()?;
            }
            handle_checkpoint_command(action)
        }
        ToolsCommands::Config(args) => config_get_dispatch(args),
        ToolsCommands::ReleaseFetch(args) => args.run(),
        ToolsCommands::InstallBinary(cmd) => cmd.run(),
        ToolsCommands::ResyncPayload(args) => args.run(),
        // No forge call, so no mode gate (§10.3 gates `checkpoint read` only).
        ToolsCommands::ResolveModel(args) => handle_resolve_model_command(&args),
        ToolsCommands::ResolveRoleModel(args) => handle_resolve_role_model_command(&args),
        ToolsCommands::Forge(cli) => {
            // Refuse a write verb before the gate can enable any routing for it.
            if cli.is_read() && cli.reaches_forge() {
                mode_gate()?;
            }
            handle_forge_read_command(cli)
        }
        ToolsCommands::Capabilities { json: _, repo } => capabilities(repo.as_deref()),
    }
}

fn main() {
    if let Err(err) = run() {
        eprintln!("Error: {err:?}");
        let _ = std::io::stderr().flush();
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};

    use super::{ToolsCli, ToolsCommands};

    /// Run `f` on an 8 MiB thread, like the binary's main thread: the clap
    /// tree now carries `ForgeAction`, whose debug-build parse frames do not
    /// fit libtest's 2 MiB worker (#10616).
    fn on_main_sized_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        std::thread::Builder::new()
            .stack_size(8 << 20)
            .spawn(f)
            .unwrap()
            .join()
            .unwrap()
    }

    /// #14395: the capability list is the clap tree. Every top-level
    /// command is listed (`forge` through its verbs), and every listed command
    /// names a real one, so `capabilities --json` cannot drift from what the
    /// binary serves.
    #[test]
    fn capability_list_matches_the_clap_tree() {
        use loom_daemon::install_compat::TOOLS_COMMANDS;
        let top: Vec<String> = on_main_sized_stack(|| {
            let mut cmd = ToolsCli::command();
            cmd.build();
            cmd.get_subcommands()
                .map(|c| c.get_name().to_string())
                .filter(|n| n != "help")
                .collect()
        });
        for name in &top {
            let listed = TOOLS_COMMANDS.iter().any(|c| {
                c == name
                    || c.strip_prefix(name.as_str())
                        .is_some_and(|r| r.starts_with(' '))
            });
            assert!(listed, "`{name}` is served but missing from TOOLS_COMMANDS");
        }
        for listed in TOOLS_COMMANDS {
            let head = listed.split(' ').next().unwrap();
            assert!(top.iter().any(|n| n == head), "`{listed}` names no loom-tools command");
        }
    }

    /// #14395: `loom-tools forge <verb>` accepts the daemon's argv, including
    /// the exit-code-as-data verbs.
    #[test]
    fn forge_reads_keep_the_daemon_argv_surface() {
        on_main_sized_stack(|| {
            for argv in [
                vec!["loom-tools", "forge", "check-claim", "42"],
                vec!["loom-tools", "forge", "check-open-pr", "42"],
                vec!["loom-tools", "forge", "wait-checks", "7"],
                vec![
                    "loom-tools",
                    "forge",
                    "issue",
                    "view",
                    "42",
                    "--json",
                    "labels",
                ],
                vec![
                    "loom-tools",
                    "forge",
                    "parent",
                    "split-state",
                    "--parent",
                    "9",
                ],
            ] {
                let cli =
                    ToolsCli::try_parse_from(&argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
                assert!(matches!(cli.command, ToolsCommands::Forge(_)), "{argv:?}");
            }
        });
    }

    /// #13878: `loom-tools resolve-model` accepts the same argv as
    /// `loom-daemon resolve-model`. The daemon-side mirror of this table is
    /// `resolve_model_keeps_its_alias_surface` in `legacy_script_cmds.rs`;
    /// both roots wrap the one `ResolveModelArgs` struct, so the surfaces
    /// cannot drift.
    #[test]
    fn resolve_model_keeps_the_daemon_argv_surface() {
        for argv in [
            vec!["loom-tools", "resolve-model", "opus"],
            vec![
                "loom-tools",
                "resolve-model",
                "claude-opus-5",
                "--task-alias",
            ],
            vec!["loom-tools", "resolve-model", "opus", "--downgrade"],
            vec![
                "loom-tools",
                "resolve-model",
                "opus",
                "--generation",
                "--config",
                "x.json",
            ],
        ] {
            let argv2 = argv.clone();
            let ok = on_main_sized_stack(move || {
                ToolsCli::try_parse_from(&argv2)
                    .map(|cli| matches!(cli.command, ToolsCommands::ResolveModel(_)))
                    .map_err(|e| e.to_string())
            });
            assert_eq!(ok, Ok(true), "{argv:?}");
        }
    }

    /// The rejections mirror the daemon root's
    /// `tier_routing_and_jev_tier_entry_points_are_rejected`: removed tier-2.5
    /// routing, an unknown runtime flag, and a missing MODEL are clap errors,
    /// not silent aliases.
    #[test]
    fn resolve_model_rejects_the_daemon_rejections() {
        for argv in [
            vec!["loom-tools", "resolve-model", "--tier", "routine"],
            vec!["loom-tools", "resolve-model", "opus", "--runtime", "claude"],
            vec!["loom-tools", "resolve-model"],
        ] {
            let argv2 = argv.clone();
            let err = on_main_sized_stack(move || ToolsCli::try_parse_from(&argv2).is_err());
            assert!(err, "{argv:?} must be rejected");
        }
    }
}
