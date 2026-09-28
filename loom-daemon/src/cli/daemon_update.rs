//! `loom-daemon daemon-update` — the subcommand behind
//! `.loom/scripts/cli/loom-daemon-update.sh` (#8088, epic #7810's third and
//! last port).
//!
//! Argument parsing is deliberately **not** delegated to clap, for the same
//! three reasons `cli/daemon_start.rs` gives (#8087):
//!
//! * `--help` prints the script's own banner, which the retained suite greps
//!   flag-by-flag;
//! * an unknown flag prints `Unknown option '<arg>'` and exits **1**, not
//!   clap's usage block and exit 2;
//! * `--timeout` carries its operator-typed string through verbatim into
//!   `loom-daemon restart --drain --timeout <SECS>`, never re-rendered from a
//!   parsed integer.
//!
//! So this takes the arguments as trailing raw values and hands them to the
//! same loop the shell ran.

use anyhow::Result;

#[derive(clap::Args)]
#[command(disable_help_flag = true)]
pub(crate) struct DaemonUpdateArgs {
    /// The script's own flags, parsed by `daemon_update::args` rather than
    /// clap.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub argv: Vec<String>,

    /// What the refusal and re-run messages should name.
    ///
    /// The shell printed `$0`, which is the script path an operator typed.
    /// The stub exports it so the ported messages keep naming the entry point
    /// rather than `~/.local/bin/loom-daemon`, which is not what anybody runs
    /// — and which `current_exe()` is precisely the wrong helper for here
    /// anyway (see `daemon_update::selfrepl`).
    #[arg(long = "argv0", env = "LOOM_UPDATE_ARGV0", hide = true)]
    pub argv0: Option<String>,
}

impl DaemonUpdateArgs {
    /// Never returns: exits with the update flow's own code, which the `loom`
    /// dispatcher, the daemon's own self-update path and the retained suites
    /// all branch on.
    pub(crate) fn run(self) -> Result<()> {
        let argv0 = self
            .argv0
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "loom-daemon-update.sh".to_string());
        loom_daemon::daemon_update::run(&raw_tail(&self.argv), &argv0);
    }
}

/// The argument list as the SHELL saw it, recovered from `args_os()` rather
/// than taken from clap.
///
/// `trailing_var_arg` + `allow_hyphen_values` gets every *other* hyphenated
/// token through intact, but clap still consumes a bare `--` as its own
/// end-of-options escape. So `loom-daemon-update.sh --` reached the ported
/// argument loop as NO arguments and ran a full update, where the shell
/// refused it with `Unknown option '--'` and exit 1 — a usage error turned
/// into an action, which is the worst direction for this particular script to
/// be wrong in.
///
/// Found by `tests/differential_daemon_update.rs`, whose corpus generates the
/// separators in the grammar rather than the ones an assertion author thinks
/// of; no retained assertion passes a bare `--`.
///
/// The scan starts at index 1 and takes the FIRST `daemon-update` token: that
/// is the subcommand name itself, because `loom-daemon` takes no global
/// options ahead of its subcommand. If the token is somehow absent, this falls
/// back to clap's own vector rather than inventing an empty one — degrading to
/// the previous behaviour beats refusing a legitimate invocation.
fn raw_tail(parsed: &[String]) -> Vec<String> {
    let all: Vec<String> = std::env::args().collect();
    all.iter()
        .skip(1)
        .position(|a| a == "daemon-update")
        .map_or_else(|| parsed.to_vec(), |idx| all[idx + 2..].to_vec())
}
