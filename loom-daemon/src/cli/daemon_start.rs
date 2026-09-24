//! `loom-daemon daemon-start` — the subcommand behind
//! `.loom/scripts/cli/loom-daemon-start.sh` (#8087, epic #7810).
//!
//! Argument parsing is deliberately **not** delegated to clap. The script's
//! surface is contract in three ways clap cannot reproduce without editing the
//! oracle:
//!
//! * `--help` prints the script's own banner, which the retained suite greps;
//! * an unknown flag prints `Unknown option '<arg>'` and exits **1**, not
//!   clap's usage block and exit 2;
//! * `--fg` is an alias of `--foreground`, and the raw argv is persisted
//!   verbatim to `.loom/.daemon.flags` for `loom-daemon-update.sh` to replay.
//!
//! So this takes the arguments as trailing raw values and hands them to the
//! same loop the shell ran.

use anyhow::Result;

#[derive(clap::Args)]
#[command(disable_help_flag = true)]
pub(crate) struct DaemonStartArgs {
    /// The script's own flags, parsed by `daemon_start::args` rather than clap.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub argv: Vec<String>,

    /// What the refusal messages should name when they tell an operator to
    /// re-run the command.
    ///
    /// The shell printed `$0`, which is the script path an operator typed. The
    /// stub exports it so the ported messages keep naming the entry point
    /// rather than `~/.local/bin/loom-daemon`, which is not what anybody runs.
    #[arg(long = "argv0", env = "LOOM_START_ARGV0", hide = true)]
    pub argv0: Option<String>,
}

impl DaemonStartArgs {
    /// Never returns: exits with the start flow's own code, which the `loom`
    /// dispatcher, the daemon's own restart paths and the retained suite all
    /// branch on.
    pub(crate) fn run(self) -> Result<()> {
        let argv0 = self
            .argv0
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "loom-daemon-start.sh".to_string());
        loom_daemon::daemon_start::run(&self.argv, &argv0);
    }
}
