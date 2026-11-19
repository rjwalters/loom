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
//! behind the mode gate, as are role-keyed PR leases. The gate resolves `mode` ([`tools_mode`])
//! before `forge_gateway::enable_cli_reads()` is considered, and enables it
//! only on the fleet route. `strip-ansi`, the other checkpoint actions and
//! `config get` never route, and `config get mode` must work with `mode` unset.
//!
//! The top-level reads and config-inspection commands (#14429, §13.7) are
//! the same story one level up: the same Args trees and handlers
//! `loom-daemon` runs, registered in `install_compat::TOOLS_COMMANDS`, with
//! the forge-reaching ones (`pr-queue` without `--input`, `premise-check
//! --issue`, `forge-probe`, `check-stale-blocked`, `labels undeclared` /
//! `migrate`) behind the same gate and the write forms (`labels generate
//! --write`, `labels undeclared --prune`, `labels migrate --apply`,
//! `forge-probe --live-write`, `forge-inventory gate --update`) refused with
//! exit 64 before any handler runs, exactly as a `forge` write verb is.
//!
//! `champion-hold`, `curation-complete` and `notify-cleared-blockers` (#14465, §13.8)
//! apply the same mode gate. Workflow writes require the local route and scoped forge access.

use std::io::Write;

use anyhow::Result;
use clap::{Parser, Subcommand};
use loom_daemon::cli::{
    config_get_dispatch, handle_checkpoint_command, handle_forge_inventory_read_command,
    handle_forge_probe_read_command, handle_forge_read_command, handle_labels_read_command,
    handle_lease_command, handle_operator_decision_command, handle_park_record_command,
    handle_resolve_model_command, handle_resolve_role_model_command, handle_strip_ansi_command,
    AffectedFilesCommand, ChampionHoldCommand, CheckPointsMarkerArgs, CheckpointAction, ConfigArgs,
    CurationCompleteArgs, DuplicateScanArgs, ForgeInventoryCommand, ForgeProbeArgs, ForgeReadCli,
    GuardsCommand, HooksHomeArgs, InstallBinaryCommand, InstallCompatCommand, LabelDuplicatesArgs,
    LabelsArgs, LeaseCli, NotifyClearedBlockersArgs, OperatorDecisionCli, ParkRecordCli,
    PrQueueArgs, PremiseCheckArgs, ReleaseFetchArgs, ResolveModelArgs, ResolveRoleModelArgs,
    ResyncPayloadArgs, RolesCommand, SkipLabelsArgs, StaleBlockedArgs,
};
use loom_daemon::install_compat;
use loom_daemon::tools_mode::{self, Route};

#[derive(Parser)]
#[command(
    name = "loom-tools",
    about = "Loom command-line tools for agent sessions"
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
    /// Read Champion hold evidence for a pull request.
    #[command(subcommand)]
    ChampionHold(ChampionHoldCommand),
    /// Project the trusted points estimate and curation labels.
    CurationComplete(CurationCompleteArgs),
    /// Notify dependents whose cited blockers have closed.
    NotifyClearedBlockers(NotifyClearedBlockersArgs),

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
    /// Claim, check, fence and release role-keyed pull-request leases.
    Lease(LeaseCli),
    /// Query the label registry (#10013): the same subcommands, stdout and
    /// exit codes as `loom-daemon labels` (#14429). `undeclared` and
    /// `migrate`'s dry runs read the forge through the mode gate; the write
    /// forms (`generate --write`, `undeclared --prune`, `migrate --apply`)
    /// are refused with exit 64 (#14396).
    Labels(LabelsArgs),
    /// Ordered PR work for Judge, Doctor and Champion (#10518): the same
    /// JSON queue and exit codes as `loom-daemon pr-queue` (#14429).
    /// `--input` runs offline; without it the queue is fetched with the
    /// person's own `gh`, through the mode gate.
    PrQueue(PrQueueArgs),
    /// The premise gate (#8396): the same KEY=VALUE report and exit codes as
    /// `loom-daemon premise-check` (#14429). `--issue` reads the forge
    /// through the mode gate; `--body-file` is hermetic and never gated.
    PremiseCheck(PremiseCheckArgs),
    /// The similarity scan behind `check-duplicate.sh` (#8360): the same
    /// verdict lines, near-match file and exit codes as `loom-daemon
    /// duplicate-scan` (#14429). Stdin in, no forge call.
    DuplicateScan(DuplicateScanArgs),
    /// Validate the Curator's points marker in one stdin body (#9056): the
    /// same line and exit codes as `loom-daemon check-points-marker`
    /// (#14429). Stdin in, no forge call.
    CheckPointsMarker(CheckPointsMarkerArgs),
    /// `labels.yml` duplicate-declared-name scan (#8875): the same count and
    /// report as `loom-daemon label-duplicates` (#14429). One file in, no
    /// forge call.
    LabelDuplicates(LabelDuplicatesArgs),
    /// The hosted-qualification probe runner (#9789): the same sanitized
    /// receipt and exit codes as `loom-daemon forge-probe` (#14429), on the
    /// credentials its runbook exports. Its whole purpose is forge calls, so
    /// the mode gate always runs; `--live-write` (the write cases) is
    /// refused with exit 64 before the gate (#14396).
    ForgeProbe(ForgeProbeArgs),
    /// The forge operation inventory (#9777): the same verbs, views and
    /// exit codes as `loom-daemon forge-inventory` (#14429). No forge call,
    /// so no gate; `gate --update` (a baseline rewrite) is refused with
    /// exit 64 (#14396).
    #[command(subcommand)]
    ForgeInventory(ForgeInventoryCommand),
    /// Curator's affected-files preflight (#9400): the same classification
    /// lines and exit codes as `loom-daemon affected-files check` (#14429).
    /// Local git objects and one body file in, no forge call.
    #[command(subcommand)]
    AffectedFiles(AffectedFilesCommand),
    /// The per-repository `loom:blocked` re-check (#8927): strictly
    /// read-only, always exit 0, the same report as `loom-daemon
    /// check-stale-blocked` (#14429). It always reads the forge, so the
    /// mode gate always runs.
    CheckStaleBlocked(StaleBlockedArgs),
    /// The combined skip-label list for fallback queries (#8255): the same
    /// four renderings and exit codes as `loom-daemon skip-labels` (#14429).
    /// Offline.
    SkipLabels(SkipLabelsArgs),
    /// The per-stage runtime draw tables (#11726): the same text/JSON views
    /// as `loom-daemon roles draw` (#14429). Reads config and admission
    /// state; launches nothing.
    #[command(subcommand)]
    Roles(RolesCommand),
    /// Guard configuration diagnostics (#10434, #11568): `guards status` and
    /// the `remote-exec` classifier, the same output and exit codes as
    /// `loom-daemon guards` (#14429). Offline.
    #[command(subcommand)]
    Guards(GuardsCommand),
    /// The installed-Loom compatibility contract (#10716): `show` and
    /// `check`, the same output and exit codes as `loom-daemon
    /// install-compat` (#14429).
    #[command(subcommand)]
    InstallCompat(InstallCompatCommand),
    /// Print the directory this release's sessions run their wired
    /// user-scope hooks from (2am#4501): the same line and exit codes as
    /// `loom-daemon hooks-home` (#14429).
    HooksHome(HooksHomeArgs),
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

/// The fleet broker's role profile has no label or comment operations.
/// Never run these mutations on an ambient gh credential in fleet mode.
fn workflow_gate(root: &std::path::Path, writes: bool) -> Result<()> {
    let checkout = loom_daemon::repo_root::find_repo_root(root);
    check_workflow_route(checkout.as_deref().unwrap_or(root), writes)
}

fn check_workflow_route(root: &std::path::Path, writes: bool) -> Result<()> {
    if writes
        && loom_daemon::config_resolver::tier_conflicts(root)
            .iter()
            .any(|c| c.key == "mode")
    {
        anyhow::bail!(
            "conflicting fleet and workspace mode settings; reconcile them before a workflow write"
        );
    }
    if tools_mode::configure_forge_reads_with_route(root).map_err(anyhow::Error::msg)?
        == Route::Fleet
    {
        anyhow::ensure!(!writes, "the scoped fleet write broker does not authorize workflow label edits or blocker comments");
        anyhow::ensure!(
            loom_daemon::forge_gateway::cli_reads_reader_only(),
            "no usable fleet read route"
        );
    }
    Ok(())
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
    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
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
        ToolsCommands::ChampionHold(cmd) => {
            cmd.run_with_gate(|root| check_workflow_route(root, false))
        }
        ToolsCommands::CurationComplete(args) => {
            workflow_gate(&args.workspace_root, true)?;
            args.run()
        }
        ToolsCommands::NotifyClearedBlockers(args) => {
            workflow_gate(args.repo_root.as_deref().unwrap_or(&cwd), !args.dry_run)?;
            args.run()
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
        ToolsCommands::Lease(cli) => handle_lease_command(cli),
        // #14429, design §13.7: the same classified-by-effect gate the forge
        // and park-record families use, and the same exit-64 refusals for
        // the write forms (#14396).
        ToolsCommands::Labels(args) => {
            if args.is_read() && args.reaches_forge() {
                mode_gate()?;
            }
            handle_labels_read_command(args)
        }
        ToolsCommands::PrQueue(args) => {
            if args.reaches_forge() {
                mode_gate()?;
            }
            args.run()
        }
        ToolsCommands::PremiseCheck(args) => {
            if args.reaches_forge() {
                mode_gate()?;
            }
            args.run()
        }
        ToolsCommands::DuplicateScan(args) => args.run(),
        ToolsCommands::CheckPointsMarker(args) => args.run(),
        ToolsCommands::LabelDuplicates(args) => args.run(),
        ToolsCommands::ForgeProbe(args) => {
            // Refuse a write form before the gate can enable any routing
            // for it, exactly as a `forge` write verb is.
            if args.is_read() && args.reaches_forge() {
                mode_gate()?;
            }
            handle_forge_probe_read_command(args)
        }
        ToolsCommands::ForgeInventory(cmd) => handle_forge_inventory_read_command(cmd),
        ToolsCommands::AffectedFiles(cmd) => cmd.run(),
        ToolsCommands::CheckStaleBlocked(args) => {
            mode_gate()?;
            args.run()
        }
        ToolsCommands::SkipLabels(args) => args.run(),
        ToolsCommands::Roles(cmd) => cmd.run(),
        ToolsCommands::Guards(cmd) => cmd.run(),
        ToolsCommands::InstallCompat(cmd) => cmd.run(),
        ToolsCommands::HooksHome(args) => args.run(),
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

    /// #14429: the top-level reads and config-inspection commands keep the
    /// daemon's argv, and the gate / refusal classification is by parsed
    /// effect: only a forge-reaching form reports `reaches_forge`, and only
    /// a write form is `!is_read` (refused with exit 64, #14396).
    #[test]
    fn top_level_reads_classify_by_effect() {
        use loom_daemon::cli::{
            AffectedFilesCommand, GuardsCommand, InstallCompatCommand, RolesCommand,
        };
        on_main_sized_stack(|| {
            // (argv tail after the subcommand name, reaches forge?, is read?)
            let cases: [(&[&str], bool, bool); 12] = [
                (&["labels", "list"], false, true),
                (&["labels", "get", "loom:issue"], false, true),
                (&["labels", "check"], false, true),
                (&["labels", "generate"], false, true),
                (&["labels", "generate", "--write"], false, false),
                (
                    &[
                        "labels",
                        "undeclared",
                        "--repo",
                        "a/b",
                        "--labels-file",
                        "f",
                    ],
                    true,
                    true,
                ),
                (
                    &[
                        "labels",
                        "undeclared",
                        "--repo",
                        "a/b",
                        "--labels-file",
                        "f",
                        "--prune",
                    ],
                    true,
                    false,
                ),
                (&["labels", "migrate", "--repo", "a/b"], true, true),
                (&["labels", "migrate", "--apply"], true, false),
                (&["pr-queue", "--role", "judge"], true, true),
                (&["pr-queue", "--role", "judge", "--input", "f"], false, true),
                (&["premise-check", "--body-file", "b", "--title", "T"], false, true),
            ];
            for (tail, reaches, read) in cases {
                let head = tail[0];
                let mut argv = vec!["loom-tools"];
                argv.extend_from_slice(tail);
                let cli =
                    ToolsCli::try_parse_from(&argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
                let (got_reaches, got_read) = match &cli.command {
                    ToolsCommands::Labels(a) => (a.reaches_forge(), a.is_read()),
                    ToolsCommands::PrQueue(a) => (a.reaches_forge(), true),
                    ToolsCommands::PremiseCheck(a) => (a.reaches_forge(), true),
                    _ => panic!("{argv:?}: not a classifier command"),
                };
                assert_eq!(got_reaches, reaches, "{argv:?}: reaches_forge");
                assert_eq!(got_read, read, "{argv:?}: is_read");
                let _ = head;
            }
            // `premise-check --issue` is forge mode and gated.
            let cli =
                ToolsCli::try_parse_from(["loom-tools", "premise-check", "--issue", "7"]).unwrap();
            let ToolsCommands::PremiseCheck(a) = cli.command else {
                panic!()
            };
            assert!(a.reaches_forge());
            // Whole-tree families: everything parses, nothing reaches the
            // forge except `forge-probe`, and the write forms are not reads.
            for argv in [
                vec!["loom-tools", "check-points-marker", "--issue", "7"],
                vec!["loom-tools", "label-duplicates", "labels.yml"],
                vec!["loom-tools", "check-stale-blocked", "--json"],
                vec!["loom-tools", "skip-labels", "--jq-not"],
                vec!["loom-tools", "hooks-home"],
                vec![
                    "loom-tools",
                    "duplicate-scan",
                    "--pool",
                    "open-issues",
                    "--title",
                    "t",
                ],
                vec!["loom-tools", "affected-files", "check", "--body-file", "b"],
                vec!["loom-tools", "guards", "status", "--json"],
                vec!["loom-tools", "roles", "draw", "--json"],
                vec!["loom-tools", "install-compat", "show", "--json"],
            ] {
                let cli =
                    ToolsCli::try_parse_from(&argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
                assert!(
                    matches!(
                        cli.command,
                        ToolsCommands::CheckPointsMarker(_)
                            | ToolsCommands::LabelDuplicates(_)
                            | ToolsCommands::CheckStaleBlocked(_)
                            | ToolsCommands::SkipLabels(_)
                            | ToolsCommands::HooksHome(_)
                            | ToolsCommands::DuplicateScan(_)
                            | ToolsCommands::AffectedFiles(AffectedFilesCommand::Check(_))
                            | ToolsCommands::Guards(GuardsCommand::Status(_))
                            | ToolsCommands::Roles(RolesCommand::Draw(_))
                            | ToolsCommands::InstallCompat(InstallCompatCommand::Show(_))
                    ),
                    "{argv:?}"
                );
            }
            let probe_read = ToolsCli::try_parse_from([
                "loom-tools",
                "forge-probe",
                "--origin",
                "https://x.example.com",
            ])
            .unwrap();
            let ToolsCommands::ForgeProbe(a) = probe_read.command else {
                panic!()
            };
            assert!(a.is_read() && a.reaches_forge());
            let probe_write =
                ToolsCli::try_parse_from(["loom-tools", "forge-probe", "--live-write"]).unwrap();
            let ToolsCommands::ForgeProbe(a) = probe_write.command else {
                panic!()
            };
            assert!(!a.is_read());
            let inv_read =
                ToolsCli::try_parse_from(["loom-tools", "forge-inventory", "report"]).unwrap();
            let ToolsCommands::ForgeInventory(c) = inv_read.command else {
                panic!()
            };
            assert!(c.is_read());
            let inv_write =
                ToolsCli::try_parse_from(["loom-tools", "forge-inventory", "gate", "--update"])
                    .unwrap();
            let ToolsCommands::ForgeInventory(c) = inv_write.command else {
                panic!()
            };
            assert!(!c.is_read());
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
