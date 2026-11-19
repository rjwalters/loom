//! `loom-tools`: the tools-only entry point (#12938, epic #12708; design
//! `docs/design/loom-tools-cli-boundary.md` §6.2 step 4, §7.3, §10).
//!
//! The tools-only installer (`scripts/install/provision-tools.sh`) can run
//! the commands advertised by `capabilities` without a daemon. Both binaries
//! use the library's handlers, including the worktree operations in #14474.
//!
//! `forge <read verb>` (#14395) is the family of forge reads: the same
//! `ForgeAction` tree and handler `loom-daemon forge` runs, minus the verbs
//! that write (#14396). See [`tools_mode`] for how commands that reach the
//! forge choose a route. Worktree commands resolve mode at the nested read,
//! keeping their local operations available offline.

use std::io::Write;

use anyhow::Result;
use clap::{Parser, Subcommand};
use loom_daemon::cli::{
    config_get_dispatch, handle_checkpoint_command, handle_forge_read_command,
    handle_operator_decision_command, handle_park_record_command, handle_resolve_model_command,
    handle_resolve_role_model_command, handle_strip_ansi_command, CheckpointAction, ConfigArgs,
    ForgeReadCli, InstallBinaryCommand, OperatorDecisionCli, ParkRecordCli, ReleaseFetchArgs,
    ResolveModelArgs, ResolveRoleModelArgs, ResyncPayloadArgs, WorktreeCommands,
};
use loom_daemon::install_compat;
use loom_daemon::tools_mode;

#[derive(Parser)]
#[command(
    name = "loom-tools",
    about = "Loom agent tools (strip-ansi, checkpoint, config get, capabilities, install, resolve-model, resolve-role-model, forge reads, park-record, operator-decision)"
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
    /// Render, read, apply and retract the `loom:blocked` park record: the
    /// same arguments, stdin, stdout and exit codes as `loom-daemon
    /// park-record` (#14464). `render`, `parse`, `retract --file`, `released`
    /// and `decision` run offline; `apply` and a forge `retract` edit the body
    /// (and `apply` the labels) on the person's own credential.
    ParkRecord(ParkRecordCli),
    /// Validate, apply and resolve an operator decision: the same arguments,
    /// stdin, stdout and exit codes as `loom-daemon operator-decision`
    /// (#14464). `resolve` records only from an attended terminal.
    OperatorDecision(OperatorDecisionCli),
    #[command(flatten)]
    Worktree(WorktreeCommands),
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
    tools_mode::configure_forge_reads(&cwd).map_err(anyhow::Error::msg)
}

/// `apply --new` files through `create-issue.sh`, which calls `$LOOM_DAEMON_BIN
/// forge create-issue` (default `loom-daemon`). A tools-only host has no daemon
/// binary, so unless the caller named one, the nested call resolves to this
/// binary: the filing stays on the route the mode gate just chose.
fn use_this_binary_for_nested_filing() {
    if std::env::var_os("LOOM_DAEMON_BIN").is_some_and(|v| !v.is_empty()) {
        return;
    }
    if let Ok(me) = std::env::current_exe() {
        std::env::set_var("LOOM_DAEMON_BIN", me);
    }
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
        ToolsCommands::ParkRecord(cli) => {
            // Classified by effect, from the parsed invocation: the offline
            // forms never resolve `mode`, so they work with `mode` unset.
            if cli.reaches_forge() {
                mode_gate()?;
            }
            handle_park_record_command(cli)
        }
        ToolsCommands::OperatorDecision(cli) => {
            if cli.reaches_forge() {
                mode_gate()?;
            }
            if cli.files_new_issue() {
                use_this_binary_for_nested_filing();
            }
            handle_operator_decision_command(cli)
        }
        ToolsCommands::Worktree(commands) => commands.run_tools(),
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

    /// #14464: both families keep the daemon's argv, and the mode gate is
    /// chosen from the parsed invocation's effects: only a form that can reach
    /// the forge reports `reaches_forge`.
    #[test]
    fn park_record_and_operator_decision_classify_by_effect() {
        on_main_sized_stack(|| {
            // (argv tail, reaches the forge?)
            let park: [(&[&str], bool); 6] = [
                (&["render", "--blocked-by", "5"], false),
                (&["parse", "--json"], false),
                (&["released", "--pr", "7"], false),
                (&["decision", "--pr", "7", "--outcome", "hold"], false),
                (&["apply", "--issue", "7", "--blocked-by", "5", "--dry-run"], true),
                (
                    &[
                        "retract",
                        "--issue",
                        "7",
                        "--blocked-by",
                        "5",
                        "--reason",
                        "r",
                    ],
                    true,
                ),
            ];
            for (tail, want) in park {
                let mut argv = vec!["loom-tools", "park-record"];
                argv.extend(tail);
                let cli =
                    ToolsCli::try_parse_from(&argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
                let ToolsCommands::ParkRecord(c) = cli.command else {
                    panic!("{argv:?}")
                };
                assert_eq!(c.reaches_forge(), want, "{argv:?}");
            }
            let offline = ToolsCli::try_parse_from([
                "loom-tools",
                "park-record",
                "retract",
                "--file",
                "-",
                "--blocked-by",
                "5",
                "--reason",
                "r",
            ])
            .unwrap();
            let ToolsCommands::ParkRecord(c) = offline.command else {
                panic!()
            };
            assert!(!c.reaches_forge(), "retract --file composes offline");

            let od: [(&[&str], bool, bool); 6] = [
                (&["validate", "--input", "-"], false, false),
                (
                    &[
                        "apply",
                        "--new",
                        "--title",
                        "T",
                        "--input",
                        "-",
                        "--dry-run",
                    ],
                    false,
                    false,
                ),
                (&["apply", "--new", "--title", "T", "--input", "-"], true, true),
                (&["apply", "7", "--input", "-"], true, false),
                (&["apply", "7", "--input", "-", "--dry-run"], true, false),
                (&["resolve", "7", "--option", "a", "--dry-run"], true, false),
            ];
            for (tail, reaches, files) in od {
                let mut argv = vec!["loom-tools", "operator-decision"];
                argv.extend(tail);
                let cli =
                    ToolsCli::try_parse_from(&argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
                let ToolsCommands::OperatorDecision(c) = cli.command else {
                    panic!("{argv:?}")
                };
                assert_eq!(c.reaches_forge(), reaches, "{argv:?}");
                assert_eq!(c.files_new_issue(), files, "{argv:?}");
            }
            // An unknown effect refuses before any forge call.
            let bad = ToolsCli::try_parse_from([
                "loom-tools",
                "operator-decision",
                "resolve",
                "7",
                "--option",
                "a",
                "--effect",
                "bogus",
                "--dry-run",
            ])
            .unwrap();
            let ToolsCommands::OperatorDecision(c) = bad.command else {
                panic!()
            };
            assert!(!c.reaches_forge());
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
