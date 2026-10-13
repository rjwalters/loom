//! `loom-daemon roles draw --explain` (#11726): each Loom stage's effective
//! weighted draw table on this host — per attempt, with each candidate's
//! weight and probability, the config tier that set the table, and what
//! `runtimes.excludeProviders` or weight 0 took out. A stage with no
//! `runtimes.roleDraw` table shows the `rolePreference` list (or static
//! binding) that still decides it. Read-only; never consults a credential pool.

use anyhow::Result;
use loom_daemon::runtime_preference::role_draw::explain;
use std::path::PathBuf;

#[derive(clap::Subcommand)]
pub(crate) enum RolesCommand {
    /// Show the weighted per-stage, per-attempt runtime draw tables (#11726).
    Draw(DrawArgs),
}

#[derive(clap::Args)]
pub(crate) struct DrawArgs {
    /// Show which config tier set each table and why entries are off. The
    /// view always includes this; the flag is accepted for symmetry with
    /// fleet-gitops' `--explain`.
    #[arg(long)]
    explain: bool,
    /// Emit JSON instead of text.
    #[arg(long)]
    json: bool,
    /// Workspace root (default: the current directory).
    #[arg(long)]
    workspace: Option<PathBuf>,
}

impl RolesCommand {
    pub(crate) fn run(self) -> Result<()> {
        match self {
            RolesCommand::Draw(args) => {
                let _ = args.explain;
                let root = match args.workspace {
                    Some(root) => root,
                    None => std::env::current_dir()?,
                };
                if args.json {
                    println!("{}", serde_json::to_string_pretty(&explain::explain_json(&root))?);
                } else {
                    print!("{}", explain::explain_text(&root));
                }
                Ok(())
            }
        }
    }
}
