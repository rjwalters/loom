//! Tools dispatch for the shared daemon lease command tree.

use anyhow::Result;

use super::forge_read::EX_WRITE_VERB_NOT_MIGRATED;
use super::lease_ensure::LeaseCommand;

/// The `lease` arguments shared with the daemon CLI.
#[derive(clap::Args)]
pub struct LeaseCli {
    #[command(subcommand)]
    command: LeaseCommand,
}

/// Run the supported lease operations through the shared handlers.
///
/// # Errors
/// The selected workspace's mode or fleet route is unavailable or invalid.
pub fn handle_lease_command(cli: LeaseCli) -> Result<()> {
    match cli.command {
        LeaseCommand::Pr(action) => action.run_tools(),
        LeaseCommand::Ensure(_)
        | LeaseCommand::CoOccupancy(_)
        | LeaseCommand::Release(_)
        | LeaseCommand::Renewer(_) => {
            eprintln!("loom-tools lease: use loom-daemon for issue lease operations");
            std::process::exit(EX_WRITE_VERB_NOT_MIGRATED);
        }
    }
}
