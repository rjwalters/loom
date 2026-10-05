//! `loom-daemon guards status` (Issue #10434): effective guard configuration
//! with its source, hook wiring, decision-log summary, and misconfiguration
//! warnings. Argument parsing and rendering only; the logic is
//! [`loom_daemon::guards_status`]. Always exits 0 (it is a diagnostic).

use std::path::PathBuf;

use anyhow::Result;

use loom_daemon::guards_status::{self, Inputs, DEFAULT_ASK_THRESHOLD};

#[derive(clap::Subcommand)]
pub(crate) enum GuardsCommand {
    /// Show every guard category's effective value and source (env, config or
    /// default), hook wiring in repo and `~/.claude` settings, a per-rule
    /// ask/deny summary of the decision log, and warnings for known
    /// misconfigurations.
    Status(StatusArgs),
}

#[derive(clap::Args)]
pub(crate) struct StatusArgs {
    /// Repository root. Defaults to the current directory.
    #[arg(long, value_name = "PATH")]
    repo: Option<PathBuf>,

    /// Emit the report as JSON.
    #[arg(long)]
    json: bool,

    /// Asks within 7 days before suggesting the toggle.
    #[arg(long, value_name = "N", default_value_t = DEFAULT_ASK_THRESHOLD)]
    ask_threshold: usize,
}

impl GuardsCommand {
    pub(crate) fn run(self) -> Result<()> {
        match self {
            GuardsCommand::Status(args) => args.run(),
        }
    }
}

impl StatusArgs {
    fn run(self) -> Result<()> {
        let inputs = Inputs {
            repo_root: self
                .repo
                .or_else(|| std::env::current_dir().ok())
                .unwrap_or_else(|| PathBuf::from(".")),
            home: std::env::var_os("HOME").map(PathBuf::from),
            now: chrono::Utc::now(),
            ask_threshold: self.ask_threshold,
            log_path: std::env::var_os("LOOM_GUARD_DECISION_LOG_FILE").map(PathBuf::from),
        };
        let report = guards_status::collect(&inputs);
        if self.json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            print!("{}", guards_status::render(&report));
        }
        Ok(())
    }
}
