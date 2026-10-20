//! `loom-daemon command-analysis` — the shell guards' door onto the shared
//! command lexer (#12236, slice 1 of #11113).
//!
//! The guard hooks used to re-derive "what will this command execute" with
//! hand-written shell masking, so literal text (a heredoc body, a quoted
//! argument) was scanned as a live command. This verb answers from
//! [`loom_daemon::role_tool_policy::command_match`], which lexes the command
//! line, recurses into `$( )`, backticks, `bash -c`/`eval` payloads and
//! wrappers, and treats a heredoc body as data unless a shell reads it or it is
//! written to a file.
//!
//! ```text
//! loom-daemon command-analysis gh-pr-merge --command "$COMMAND"
//! ```
//!
//! Exit 0 = the command would execute `gh pr merge`; exit 1 = it would not.
//! Any other exit (no binary, a daemon predating the verb) is "unanswered" and
//! the caller must fail closed.

use anyhow::Result;

use loom_daemon::role_tool_policy::command_match::executes_gh_pr_merge;

/// Exit code for "this command does not execute the queried program".
pub(crate) const EX_NO: i32 = 1;

#[derive(clap::Subcommand)]
pub(crate) enum CommandAnalysisCommand {
    /// Exit 0 when the command line would execute `gh pr merge`, 1 when not.
    GhPrMerge(GhPrMergeArgs),
}

#[derive(clap::Args)]
pub(crate) struct GhPrMergeArgs {
    /// The Bash command line to analyze.
    #[arg(long, value_name = "CMD")]
    pub command: String,
}

impl CommandAnalysisCommand {
    pub(crate) fn run(self) -> Result<()> {
        let code = match self {
            Self::GhPrMerge(args) => {
                if executes_gh_pr_merge(&args.command) {
                    0
                } else {
                    EX_NO
                }
            }
        };
        std::process::exit(code);
    }
}
