//! `loom-tools`: the tools-only entry point (#12938, epic #12708; design
//! `docs/design/loom-tools-cli-boundary.md` §6.2 step 4, §7.3, §10).
//!
//! Only `strip-ansi`, `checkpoint`, `config get` and `capabilities` (#12939), plus the
//! install trio `release-fetch`, `install-binary` and `resync-payload` (#12940): the
//! tools-only installer (`scripts/install/provision-tools.sh`) has no daemon to call. The handlers are the
//! library's own (the same functions `loom-daemon strip-ansi|checkpoint|config`
//! call), so there is no second copy. This crate links `loom_daemon` and not
//! `daemon_service`, which is private to the `loom-daemon` bin.
//!
//! The one forge call in the slice is `checkpoint read` (§10.3), so that is the
//! only command behind the mode gate: it resolves `mode` ([`tools_mode`])
//! before `forge_gateway::enable_cli_reads()` is considered, and enables it
//! only on the fleet route. `strip-ansi`, the other checkpoint actions and
//! `config get` never route, and `config get mode` must work with `mode` unset.

use std::io::Write;

use anyhow::Result;
use clap::{Parser, Subcommand};
use loom_daemon::cli::{
    config_get_dispatch, handle_checkpoint_command, handle_strip_ansi_command, CheckpointAction,
    ConfigArgs, InstallBinaryCommand, ReleaseFetchArgs, ResyncPayloadArgs,
};
use loom_daemon::install_compat;
use loom_daemon::tools_mode::{self, Route};

#[derive(Parser)]
#[command(
    name = "loom-tools",
    about = "Loom agent tools (strip-ansi, checkpoint, config get, capabilities, install)"
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
