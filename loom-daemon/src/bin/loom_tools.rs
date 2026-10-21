//! `loom-tools`: the tools-only entry point (#12938, epic #12708; design
//! `docs/design/loom-tools-cli-boundary.md` §6.2 step 4, §7.3, §10).
//!
//! Only `strip-ansi`, `checkpoint` and `config get`. The handlers are the
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
    ConfigArgs,
};
use loom_daemon::tools_mode::{self, Route};

#[derive(Parser)]
#[command(
    name = "loom-tools",
    about = "Loom agent tools (strip-ansi, checkpoint, config get)"
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
    }
}

fn main() {
    if let Err(err) = run() {
        eprintln!("Error: {err:?}");
        let _ = std::io::stderr().flush();
        std::process::exit(1);
    }
}
