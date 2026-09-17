//! `loom-daemon daemon-watchdog` — the subcommand behind
//! `.loom/scripts/cli/loom-daemon-watchdog.sh` (#8086, epic #7810).
//!
//! The flag surface is deliberately tiny and is contract: the script accepted
//! only `--help|-h` and `--verbose|-v`, and exited **2** on anything else. A
//! launchd/systemd timer invokes it with no arguments at all.

use anyhow::Result;

#[derive(clap::Args)]
#[command(disable_help_flag = true)]
pub(crate) struct WatchdogArgs {
    /// Print the design banner and exit.
    ///
    /// Handled here rather than by clap because the banner IS the contract:
    /// the retained suite greps it for the marker / `StartInterval` rationale
    /// and for the #4398 and #5944 knob names, and it is the design record for
    /// every incident that shaped this detector.
    #[arg(long = "help", short = 'h', action = clap::ArgAction::SetTrue)]
    pub help: bool,

    /// Echo `OK`-level lines to stderr too. They are always written to the log;
    /// this only makes a healthy tick noisy on the console.
    #[arg(long = "verbose", short = 'v', action = clap::ArgAction::SetTrue)]
    pub verbose: bool,
}

impl WatchdogArgs {
    /// Never returns: exits with the tick's own code, which callers branch on.
    /// 0 healthy or deliberate stop, 1 divergence or state mismatch, 3 liveness
    /// undetermined, 2 usage.
    pub(crate) fn run(self) -> Result<()> {
        if self.help {
            print!("{}", loom_daemon::watchdog::HELP_BANNER);
            std::process::exit(0);
        }
        std::process::exit(loom_daemon::watchdog::tick(self.verbose));
    }
}
