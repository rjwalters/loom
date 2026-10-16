//! `loom-daemon prompt-tree` (#12509): checkout-freshness telemetry for
//! attended sessions.
//!
//! `session-start` is what the user-scope `SessionStart` hook
//! (`defaults/hooks/prompt-tree-session.sh`) calls. Without `--foreground` it
//! reads the hook JSON from stdin, starts a detached copy of itself and
//! returns; with it, it does the network reads and posts one
//! `session.prompt_tree` record. Fail-open: always exits 0 and prints nothing
//! into the session.

use std::io::{IsTerminal, Read};
use std::path::PathBuf;

use anyhow::Result;
use loom_daemon::role_tick_telemetry::prompt_tree::session::{self, Spec};

/// Attended-session prompt-tree telemetry.
#[derive(clap::Args)]
pub(crate) struct PromptTreeArgs {
    #[command(subcommand)]
    action: PromptTreeCommand,
}

#[derive(clap::Subcommand)]
enum PromptTreeCommand {
    /// Record how far this session's checkout is behind GitHub's tip. Reads
    /// the hook payload from stdin (`session_id`, `cwd`, `source` only).
    SessionStart(SessionStartArgs),
}

#[derive(clap::Args)]
struct SessionStartArgs {
    /// Do the work in this process (what the detached copy runs).
    #[arg(long)]
    foreground: bool,
    /// The checkout (foreground only).
    #[arg(long)]
    cwd: Option<PathBuf>,
    /// The session id (foreground only).
    #[arg(long)]
    session_id: Option<String>,
    /// What started the session (foreground only).
    #[arg(long)]
    source: Option<String>,
}

impl PromptTreeArgs {
    pub(crate) fn run(self) -> Result<()> {
        let PromptTreeCommand::SessionStart(args) = self.action;
        if args.foreground {
            if let (Some(cwd), Some(session_id)) = (args.cwd, args.session_id) {
                session::run(&Spec {
                    cwd,
                    session_id,
                    source: args.source,
                });
            }
            return Ok(());
        }
        let mut stdin = String::new();
        if !std::io::stdin().is_terminal() {
            std::io::stdin()
                .take(1 << 20)
                .read_to_string(&mut stdin)
                .ok();
        }
        let _ = session::start(&stdin);
        Ok(())
    }
}
